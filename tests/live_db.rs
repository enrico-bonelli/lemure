//! Test contro database reali. Richiedono i container di test:
//!   LEMURE_TEST_MYSQL=127.0.0.1:13306  (root/lemure, db testdb)
//!   LEMURE_TEST_PG=127.0.0.1:15432     (postgres/lemure, db testdb)
//!   LEMURE_TEST_MONGO=127.0.0.1:27018  (senza autenticazione, db testdb)
//! Se le variabili non sono impostate i test vengono saltati.

use lemure::config::{DataSource, DbKind};
use lemure::db::{CsvOptions, DbConn, StmtResult};
use lemure::sqlutil::{self, Dialect};
use std::sync::atomic::AtomicBool;

fn ds(kind: DbKind) -> Option<DataSource> {
    let var = match kind {
        DbKind::MySql => "LEMURE_TEST_MYSQL",
        DbKind::Postgres => "LEMURE_TEST_PG",
        DbKind::Mongo => "LEMURE_TEST_MONGO",
    };
    let addr = std::env::var(var).ok()?;
    let (host, port) = addr.split_once(':')?;
    Some(DataSource {
        id: "t".into(),
        name: "test".into(),
        kind,
        host: host.into(),
        port: port.parse().ok()?,
        user: match kind {
            DbKind::MySql => "root".into(),
            DbKind::Postgres => "postgres".into(),
            DbKind::Mongo => String::new(),
        },
        password: if kind == DbKind::Mongo {
            String::new()
        } else {
            "lemure".into()
        },
        database: "testdb".into(),
    })
}

fn rows(r: StmtResult) -> lemure::db::RowSet {
    match r {
        StmtResult::Rows(rs) => rs,
        other => panic!("attese righe, ottenuto {other:?}"),
    }
}

fn check_backend(kind: DbKind) {
    let Some(ds) = ds(kind) else {
        eprintln!("salto {kind:?}: variabile non impostata");
        return;
    };
    let dialect = lemure::db::dialect_of(kind);
    let mut c = DbConn::open(&ds).expect("connessione");
    assert!(c.server_version().contains(kind.label()));

    let tables = c
        .list_tables(if kind == DbKind::MySql { "testdb" } else { "" })
        .unwrap();
    let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        &names[..4],
        &["customers", "order_items", "orders", "products"],
        "{names:?}"
    );
    assert!(
        tables
            .iter()
            .any(|t| t.name == "v_customer_totals" && t.is_view)
    );

    let schema = if kind == DbKind::MySql {
        "testdb"
    } else {
        "public"
    };
    let cols = c.list_columns(schema, "orders").unwrap();
    let cnames: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        cnames,
        ["customer_id", "id", "ordered_on", "status", "total"]
    );
    assert_eq!(cols[1].key, "PRI");

    // Paginazione lato server
    let base = sqlutil::pageable_base("SELECT * FROM orders ORDER BY id;", dialect).unwrap();
    let page = rows(
        c.execute(&sqlutil::paged_sql(&base, 501, 500, dialect))
            .unwrap(),
    );
    assert_eq!(page.rows.len(), 501);
    assert_eq!(page.rows[0][0].as_deref(), Some("501"));
    assert_eq!(page.columns.len(), 5);
    assert!(!page.columns[0].type_name.is_empty());
    let total = c.query_scalar(&sqlutil::count_sql(&base, dialect)).unwrap();
    assert_eq!(total.as_deref(), Some("30000"));

    // Conteggio di una JOIN con colonne omonime (in MySQL una subquery fallirebbe)
    let join = sqlutil::pageable_base(
        "SELECT * FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.status = 'paid' ORDER BY o.id",
        dialect,
    )
    .unwrap();
    assert_eq!(
        c.query_scalar(&sqlutil::count_sql(&join, dialect))
            .unwrap()
            .as_deref(),
        Some("10000")
    );

    // NULL, testo multiriga, binari
    let rs = rows(
        c.execute("SELECT notes, city FROM customers WHERE id IN (3, 7) ORDER BY id")
            .unwrap(),
    );
    assert_eq!(rs.rows[0][1], None);
    assert!(rs.rows[1][0].as_deref().unwrap().contains('\n'));
    let rs = rows(
        c.execute("SELECT thumb FROM products WHERE sku = 'SKU00001'")
            .unwrap(),
    );
    let v = rs.rows[0][0].as_deref().unwrap().to_ascii_lowercase();
    assert!(v.contains("89504e47"), "{v}");

    // Statement senza righe
    match c
        .execute("UPDATE products SET price = price WHERE sku = 'SKU00002'")
        .unwrap()
    {
        StmtResult::Affected { .. } => {}
        other => panic!("{other:?}"),
    }
    assert!(c.execute("SELECT * FROM tabella_inesistente").is_err());

    // Analisi tabella
    let a = c.analyze_table(schema, "orders").unwrap();
    assert_eq!(a.primary_key.as_ref().unwrap().columns, "id");
    assert!(
        a.indexes.iter().any(|i| i.columns == "status, ordered_on"),
        "{:?}",
        a.indexes
    );
    assert_eq!(a.foreign_keys.len(), 1);
    assert_eq!(a.foreign_keys[0].on_delete, "CASCADE");
    assert_eq!(a.referenced_by.len(), 1);
    assert!(a.ddl.contains("CREATE TABLE"));
    let a = c.analyze_table(schema, "order_items").unwrap();
    assert!(
        a.key_description.contains("composta"),
        "{}",
        a.key_description
    );
    assert_eq!(a.foreign_keys.len(), 2);

    // Metadati per l'autocompletamento
    let meta = c.load_schema_meta(3000).unwrap();
    assert_eq!(meta.default_schema, schema);
    let orders = meta.find_table(None, "orders").unwrap();
    let cols = meta.tables[orders].columns.as_ref().unwrap();
    assert_eq!(cols.len(), 5);
    assert_eq!(cols[0].name, "id");
    let lazy = c.load_schema_meta(1).unwrap();
    assert!(lazy.tables.iter().all(|t| t.columns.is_none()));
    let comp =
        lemure::completion::complete("SELECT o. FROM orders o", 9, dialect, &meta, false).unwrap();
    assert!(comp.items.iter().any(|i| i.label == "customer_id"));

    // Export CSV in streaming
    let dir = std::env::temp_dir().join(format!("lemure-test-{}-{kind:?}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{kind:?}.csv"));
    let opts = CsvOptions {
        separator: ";".into(),
        header: true,
        quote_all: false,
    };
    let stop = AtomicBool::new(false);
    let n = c
        .export_csv(
            "SELECT id, name, notes FROM customers ORDER BY id",
            &path,
            &opts,
            &stop,
            &|_| {},
        )
        .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.starts_with("id;name;notes\n1;Cliente 1;\n"),
        "{}",
        &text[..60]
    );
    assert!(text.contains("7;Cliente 7;\"nota multiriga\nseconda"));
    assert_eq!(n, 12345);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn mongo_backend() {
    let Some(ds) = ds(DbKind::Mongo) else {
        eprintln!("salto Mongo: variabile non impostata");
        return;
    };
    let d = Dialect::Mongo;
    let mut c = DbConn::open(&ds).expect("connessione");
    assert!(c.server_version().starts_with("MongoDB"));
    assert!(c.list_databases().unwrap().contains(&"testdb".to_string()));

    let tables = c.list_tables("testdb").unwrap();
    let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["customers", "event-log", "orders", "paid_orders"]);
    assert!(tables.iter().any(|t| t.name == "paid_orders" && t.is_view));

    // Campi dedotti a campione, con indici e sotto-campi
    let cols = c.list_columns("testdb", "customers").unwrap();
    let col = |n: &str| {
        cols.iter()
            .find(|c| c.name == n)
            .unwrap_or_else(|| panic!("{n}: {cols:?}"))
    };
    assert_eq!(col("_id").key, "PRI");
    assert_eq!(col("email").key, "UNI");
    assert_eq!(col("address.city").data_type, "string | null");
    assert!(col("notes").nullable);
    assert_eq!(col("created").data_type, "date");

    // Paginazione lato server e conteggio
    let base = sqlutil::pageable_base("db.orders.find({}).sort({seq: 1});", d).unwrap();
    let page = rows(c.execute(&sqlutil::paged_sql(&base, 501, 500, d)).unwrap());
    assert_eq!(page.rows.len(), 501);
    let seq = page.columns.iter().position(|c| c.name == "seq").unwrap();
    assert_eq!(page.rows[0][seq].as_deref(), Some("501"));
    assert_eq!(page.columns[0].name, "_id");
    assert_eq!(page.columns[0].type_name, "objectId");
    assert!(
        page.rows[0][0]
            .as_deref()
            .unwrap()
            .starts_with("ObjectId(\"")
    );
    let items = page.columns.iter().position(|c| c.name == "items").unwrap();
    assert!(
        page.rows[0][items]
            .as_deref()
            .unwrap()
            .starts_with("[{\"sku\"")
    );
    assert_eq!(
        c.query_scalar(&sqlutil::count_sql(&base, d))
            .unwrap()
            .as_deref(),
        Some("12345")
    );
    let agg = sqlutil::pageable_base(
        "db.orders.aggregate([{$group: {_id: '$status', n: {$sum: 1}}}, {$sort: {_id: 1}}])",
        d,
    )
    .unwrap();
    let rs = rows(c.execute(&sqlutil::paged_sql(&agg, 501, 0, d)).unwrap());
    assert_eq!(rs.rows[1], vec![Some("paid".into()), Some("4115".into())]);
    assert_eq!(
        c.query_scalar(&sqlutil::count_sql(&agg, d))
            .unwrap()
            .as_deref(),
        Some("3")
    );
    let one = |c: &mut DbConn, q: &str| c.query_scalar(q).unwrap().unwrap_or_default();
    assert_eq!(one(&mut c, "db.paid_orders.countDocuments({})"), "4115");
    assert_eq!(
        one(
            &mut c,
            "db.orders.find({status: 'paid', total: {$gte: 100}}).count()"
        ),
        one(
            &mut c,
            "db.orders.countDocuments({status: /^pa/, total: {$gte: 100}})"
        )
    );
    assert_eq!(
        one(
            &mut c,
            "db.customers.countDocuments({created: {$lt: ISODate('2025-01-02')}})"
        ),
        "23"
    );
    let rs = rows(c.execute("db.orders.distinct('status')").unwrap());
    assert_eq!(rs.rows.len(), 3);
    let rs = rows(
        c.execute("db['event-log'].find({kind: 'logout'}, {_id: 0})")
            .unwrap(),
    );
    assert_eq!(
        rs.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["kind", "at", "extra"]
    );

    // Scritture, use e comandi
    c.execute("db.scratch.drop()").ok();
    match c
        .execute("db.scratch.insertMany([{a: 1}, {a: 2}, {a: 3}])")
        .unwrap()
    {
        StmtResult::Affected { rows, .. } => assert_eq!(rows, 3),
        other => panic!("{other:?}"),
    }
    match c
        .execute("db.scratch.updateMany({a: {$gt: 1}}, {$inc: {a: 10}})")
        .unwrap()
    {
        StmtResult::Affected { rows, .. } => assert_eq!(rows, 2),
        other => panic!("{other:?}"),
    }
    match c.execute("db.scratch.deleteOne({a: 12})").unwrap() {
        StmtResult::Affected { rows, .. } => assert_eq!(rows, 1),
        other => panic!("{other:?}"),
    }
    assert!(
        matches!(c.execute("db.scratch.createIndex({a: -1})").unwrap(), StmtResult::Message(m) if m.contains("a_-1"))
    );
    assert!(matches!(
        c.execute("db.scratch.drop()").unwrap(),
        StmtResult::Message(_)
    ));
    assert!(matches!(
        c.execute("use admin").unwrap(),
        StmtResult::Message(_)
    ));
    assert_eq!(c.current_schema().unwrap(), "admin");
    c.execute("use testdb").unwrap();
    let rs = rows(c.execute("db.runCommand({ping: 1})").unwrap());
    assert_eq!(rs.rows[0][0].as_deref(), Some("ok"));
    let e = c.execute("db.orders.find({$bad: 1})").unwrap_err();
    assert!(e.starts_with("ERRORE"), "{e}");
    assert!(c.execute("db.orders.find({a: }").is_err());

    // Analisi della collection
    let a = c.analyze_table("testdb", "orders").unwrap();
    assert_eq!(a.primary_key.as_ref().unwrap().columns, "_id ASC");
    assert!(
        a.indexes
            .iter()
            .any(|i| i.columns == "status ASC, ordered_on DESC"),
        "{:?}",
        a.indexes
    );
    assert!(
        a.ddl.contains("db.orders.createIndex({\"customer_id\":1}"),
        "{}",
        a.ddl
    );
    assert!(
        a.properties
            .iter()
            .any(|(k, v)| k == "Documenti" && v == "12.345"),
        "{:?}",
        a.properties
    );
    assert!(
        a.columns
            .rows
            .iter()
            .any(|r| r[0].as_deref() == Some("items.sku"))
    );
    let v = c.analyze_table("testdb", "paid_orders").unwrap();
    assert!(v.ddl.starts_with("db.createView("), "{}", v.ddl);
    let e = c.analyze_table("testdb", "event-log").unwrap();
    assert!(
        e.ddl.contains("db.createCollection(\"event-log\")"),
        "{}",
        e.ddl
    );

    // Metadati e autocompletamento
    let meta = c.load_schema_meta(3000).unwrap();
    assert_eq!(meta.default_schema, "testdb");
    assert!(!meta.schemas.contains(&"admin".to_string()));
    let comp = lemure::completion::complete("db.ord", 6, d, &meta, false).unwrap();
    assert_eq!(comp.items[0].label, "orders");

    // Export CSV in streaming
    let dir = std::env::temp_dir().join(format!("lemure-test-mongo-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("customers.csv");
    let opts = CsvOptions {
        separator: ";".into(),
        header: true,
        quote_all: false,
    };
    let stop = AtomicBool::new(false);
    let n = c
        .export_csv(
            "db.customers.find({}).sort({_id: 1})",
            &path,
            &opts,
            &stop,
            &|_| {},
        )
        .unwrap();
    assert_eq!(n, 1234);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.starts_with("_id;name;email;address;tags;created"),
        "{}",
        &text[..80]
    );
    assert!(text.contains("\"nota multiriga\nseconda riga\""));
    std::fs::remove_dir_all(&dir).ok();

    // Stop di una query lenta da un altro thread
    let cancel = c.cancel_handle();
    let t = std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let r = c.execute("db.orders.find({$where: 'sleep(50) || true'})");
        (r, started.elapsed())
    });
    std::thread::sleep(std::time::Duration::from_millis(700));
    cancel.cancel().unwrap();
    let (r, elapsed) = t.join().unwrap();
    assert!(r.is_err(), "la query doveva essere interrotta");
    assert!(elapsed.as_secs() < 10, "{elapsed:?}");
}

#[test]
fn mysql_backend() {
    check_backend(DbKind::MySql);
}

#[test]
fn postgres_backend() {
    check_backend(DbKind::Postgres);
}

#[test]
fn dialect_helpers() {
    assert!(sqlutil::pageable_base("select 1", Dialect::Postgres).is_some());
}
