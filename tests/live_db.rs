//! Test contro database reali. Richiedono i container di test:
//!   LEMURE_TEST_MYSQL=127.0.0.1:13306  (root/lemure, db testdb)
//!   LEMURE_TEST_PG=127.0.0.1:15432     (postgres/lemure, db testdb)
//! Se le variabili non sono impostate i test vengono saltati.

use lemure::config::{DataSource, DbKind};
use lemure::db::{CsvOptions, DbConn, StmtResult};
use lemure::sqlutil::{self, Dialect};
use std::sync::atomic::AtomicBool;

fn ds(kind: DbKind) -> Option<DataSource> {
    let var = match kind {
        DbKind::MySql => "LEMURE_TEST_MYSQL",
        DbKind::Postgres => "LEMURE_TEST_PG",
    };
    let addr = std::env::var(var).ok()?;
    let (host, port) = addr.split_once(':')?;
    Some(DataSource {
        id: "t".into(),
        name: "test".into(),
        kind,
        host: host.into(),
        port: port.parse().ok()?,
        user: if kind == DbKind::MySql {
            "root".into()
        } else {
            "postgres".into()
        },
        password: "lemure".into(),
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
    let page = rows(c.execute(&sqlutil::paged_sql(&base, 501, 500)).unwrap());
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
    let dir = std::env::temp_dir().join(format!("lemure-test-{}", std::process::id()));
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
