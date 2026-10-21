//! Esecuzione dei comandi MongoDB con il driver sincrono.
//!
//! I documenti diventano un `RowSet`: una colonna per ogni campo di primo
//! livello (nell'ordine in cui compaiono, `_id` per primo), i valori
//! semplici come testo e i sotto-documenti/array in JSON (extended JSON
//! relaxed, che il parser rilegge: si possono copiare nei filtri).

use super::parse::{self, Op};
use crate::completion::{SchemaMeta, TableMeta};
use crate::config::DataSource;
use crate::db::{
    ColumnInfo, ColumnMeta, CsvOptions, DbResult, IndexInfo, RowSet, StmtResult, TableAnalysis,
    TableEntry, human_bytes, write_csv_row,
};
use mongodb::bson::{Bson, Document, doc, spec::BinarySubtype};
use mongodb::error::{Error, ErrorKind, WriteFailure};
use mongodb::options::{ClientOptions, UpdateModifications};
use mongodb::sync::{Client, Database};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Database usato quando il datasource non ne indica uno (come la shell).
pub const DEFAULT_DB: &str = "test";
/// Documenti letti per dedurre i campi di una collection.
const SAMPLE_SIZE: i64 = 200;
const SYSTEM_DBS: [&str; 3] = ["admin", "config", "local"];

#[derive(Default)]
struct CancelShared {
    /// `comment` dell'operazione in corso, per trovarla con `$currentOp`.
    tag: Mutex<Option<String>>,
    stop: AtomicBool,
}

/// Permette di interrompere da un altro thread l'operazione in corso.
#[derive(Clone)]
pub struct MongoCancel {
    client: Client,
    shared: Arc<CancelShared>,
}

impl MongoCancel {
    pub fn cancel(&self) -> DbResult<()> {
        self.shared.stop.store(true, Ordering::Relaxed);
        let Some(tag) = self.shared.tag.lock().unwrap().clone() else {
            return Ok(());
        };
        let admin = self.client.database("admin");
        let ops = admin
            .run_cursor_command(doc! {
                "aggregate": 1,
                "pipeline": [
                    { "$currentOp": { "allUsers": true } },
                    { "$match": { "$or": [
                        { "command.comment": &tag },
                        { "cursor.originatingCommand.comment": &tag },
                    ] } },
                ],
                "cursor": {},
            })
            .run()
            .map_err(fmt_err)?;
        for op in ops {
            let op = op.map_err(fmt_err)?;
            if let Some(id) = op.get("opid") {
                admin
                    .run_command(doc! { "killOp": 1, "op": id.clone() })
                    .run()
                    .map_err(fmt_err)?;
            }
        }
        Ok(())
    }
}

pub struct MongoConn {
    client: Client,
    /// Database corrente (cambia con `use`).
    pub db: String,
    shared: Arc<CancelShared>,
}

fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

pub fn fmt_err(e: Error) -> String {
    match e.kind.as_ref() {
        ErrorKind::Command(c) => format!("ERRORE {} ({}): {}", c.code, c.code_name, c.message),
        ErrorKind::Write(WriteFailure::WriteError(w)) => {
            format!("ERRORE {}: {}", w.code, w.message)
        }
        ErrorKind::Write(WriteFailure::WriteConcernError(w)) => {
            format!("ERRORE {} ({}): {}", w.code, w.code_name, w.message)
        }
        ErrorKind::InsertMany(m) => match m.write_errors.as_ref().and_then(|v| v.first()) {
            Some(w) => format!(
                "ERRORE {}: {} (documento {})",
                w.code,
                w.message,
                w.index + 1
            ),
            None => e.to_string(),
        },
        ErrorKind::ServerSelection { message, .. } => {
            format!("Server MongoDB non raggiungibile: {message}")
        }
        ErrorKind::Authentication { message, .. } => format!("Autenticazione fallita: {message}"),
        _ => e.to_string(),
    }
}

impl MongoConn {
    pub fn open(ds: &DataSource) -> DbResult<MongoConn> {
        let host = ds.host.trim();
        // Nel campo host si può mettere anche un URI completo (mongodb:// o mongodb+srv://).
        let uri = if host.starts_with("mongodb://") || host.starts_with("mongodb+srv://") {
            host.to_string()
        } else {
            let auth = if ds.user.trim().is_empty() {
                String::new()
            } else {
                format!("{}:{}@", pct(ds.user.trim()), pct(&ds.password))
            };
            let host = if host.contains(':') && !host.starts_with('[') {
                format!("[{host}]")
            } else {
                host.to_string()
            };
            format!("mongodb://{auth}{host}:{}/?directConnection=true", ds.port)
        };
        let mut opts = ClientOptions::parse(uri.as_str())
            .run()
            .map_err(|e| format!("URI non valido: {}", fmt_err(e)))?;
        opts.app_name.get_or_insert_with(|| "lemure".into());
        opts.connect_timeout.get_or_insert(Duration::from_secs(10));
        opts.server_selection_timeout
            .get_or_insert(Duration::from_secs(10));
        let uri_db = opts.default_database.clone();
        let client = Client::with_options(opts).map_err(fmt_err)?;
        // Il driver si collega in modo pigro: il ping verifica subito raggiungibilità e credenziali.
        client
            .database("admin")
            .run_command(doc! { "ping": 1 })
            .run()
            .map_err(fmt_err)?;
        let db = Some(ds.database.trim().to_string())
            .filter(|d| !d.is_empty())
            .or(uri_db)
            .unwrap_or_else(|| DEFAULT_DB.to_string());
        Ok(MongoConn {
            client,
            db,
            shared: Arc::default(),
        })
    }

    pub fn cancel_handle(&self) -> MongoCancel {
        MongoCancel {
            client: self.client.clone(),
            shared: self.shared.clone(),
        }
    }

    pub fn server_version(&self) -> String {
        self.client
            .database("admin")
            .run_command(doc! { "buildInfo": 1 })
            .run()
            .ok()
            .and_then(|d| d.get_str("version").ok().map(|v| format!("MongoDB {v}")))
            .unwrap_or_else(|| "MongoDB".into())
    }

    pub fn is_alive(&self) -> bool {
        self.client
            .database("admin")
            .run_command(doc! { "ping": 1 })
            .run()
            .is_ok()
    }

    fn database(&self, name: Option<&str>) -> Database {
        self.client.database(name.unwrap_or(&self.db))
    }

    /// Prepara un'operazione annullabile: `comment` univoco e flag di stop azzerato.
    fn begin(&self) -> String {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let tag = format!(
            "lemure-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        *self.shared.tag.lock().unwrap() = Some(tag.clone());
        self.shared.stop.store(false, Ordering::Relaxed);
        tag
    }

    fn end(&self) {
        self.shared.tag.lock().unwrap().take();
    }

    fn stopped(&self) -> bool {
        self.shared.stop.load(Ordering::Relaxed)
    }

    /// Esegue un comando che restituisce un cursore e raccoglie tutti i documenti.
    fn cursor_docs(&self, db: &Database, mut cmd: Document) -> DbResult<Vec<Document>> {
        let tag = self.begin();
        cmd.insert("comment", tag);
        let res = (|| {
            let cursor = db.run_cursor_command(cmd).run().map_err(fmt_err)?;
            let mut out = Vec::new();
            for d in cursor {
                out.push(d.map_err(fmt_err)?);
                if out.len() % 256 == 0 && self.stopped() {
                    return Err("Query interrotta".to_string());
                }
            }
            Ok(out)
        })();
        self.end();
        res
    }

    fn command(&self, db: &Database, cmd: Document) -> DbResult<Document> {
        db.run_command(cmd).run().map_err(fmt_err)
    }

    pub fn execute(&mut self, src: &str) -> DbResult<StmtResult> {
        let planned = parse::parse_and_plan(src).map_err(|e| e.message)?;
        let db = self.database(planned.db.as_deref());
        let msg = |m: String| Ok(StmtResult::Message(m));
        match planned.op {
            Op::Find { coll, find } => {
                let docs = self.cursor_docs(&db, parse::find_command(&coll, &find))?;
                Ok(StmtResult::Rows(docs_to_rowset(&docs)))
            }
            Op::Aggregate {
                coll,
                pipeline,
                options,
            } => {
                let docs =
                    self.cursor_docs(&db, parse::aggregate_command(&coll, &pipeline, &options))?;
                Ok(StmtResult::Rows(docs_to_rowset(&docs)))
            }
            Op::Count {
                coll,
                filter,
                skip,
                limit,
                max_time_ms,
            } => {
                // come countDocuments(): $match + $group, così il filtro può usare qualunque operatore
                let mut pipeline = vec![doc! { "$match": filter }];
                if skip > 0 {
                    pipeline.push(doc! { "$skip": skip as i64 });
                }
                if let Some(l) = limit {
                    pipeline.push(doc! { "$limit": l });
                }
                pipeline.push(doc! { "$group": { "_id": 1, "n": { "$sum": 1 } } });
                let mut options = Document::new();
                if let Some(ms) = max_time_ms {
                    options.insert("maxTimeMS", ms);
                }
                let n = self.count_pipeline(&db, &coll, pipeline, &options)?;
                Ok(StmtResult::Rows(scalar("count", n)))
            }
            Op::AggregateCount {
                coll,
                mut pipeline,
                options,
            } => {
                pipeline.push(doc! { "$count": "n" });
                let n = self.count_pipeline(&db, &coll, pipeline, &options)?;
                Ok(StmtResult::Rows(scalar("count", n)))
            }
            Op::EstimatedCount { coll } => {
                let r = self.command(&db, doc! { "count": coll })?;
                Ok(StmtResult::Rows(scalar("count", as_i64(r.get("n")))))
            }
            Op::Distinct {
                coll,
                field,
                filter,
            } => {
                let r = self.command(
                    &db,
                    doc! { "distinct": coll, "key": &field, "query": filter },
                )?;
                let values = r.get_array("values").cloned().unwrap_or_default();
                Ok(StmtResult::Rows(values_rowset(&field, &values)))
            }
            Op::Insert { coll, docs } => {
                let res = db
                    .collection::<Document>(&coll)
                    .insert_many(&docs)
                    .run()
                    .map_err(fmt_err)?;
                let n = res.inserted_ids.len() as u64;
                let message = if n == 1 {
                    let id = res.inserted_ids.values().next().unwrap();
                    format!("_id: {}", cell_text(id).unwrap_or_default())
                } else {
                    String::new()
                };
                Ok(StmtResult::Affected { rows: n, message })
            }
            Op::Update {
                coll,
                filter,
                update,
                many,
                upsert,
                array_filters,
            } => {
                let upd = match update {
                    Bson::Array(stages) => UpdateModifications::Pipeline(
                        stages
                            .into_iter()
                            .filter_map(|s| s.as_document().cloned())
                            .collect(),
                    ),
                    Bson::Document(d) => UpdateModifications::Document(d),
                    _ => unreachable!(),
                };
                let c = db.collection::<Document>(&coll);
                let mut action = if many {
                    c.update_many(filter, upd)
                } else {
                    c.update_one(filter, upd)
                }
                .upsert(upsert);
                if let Some(af) = array_filters {
                    action = action.array_filters(af);
                }
                let res = action.run().map_err(fmt_err)?;
                Ok(update_result(
                    res.matched_count,
                    res.modified_count,
                    res.upserted_id,
                ))
            }
            Op::Replace {
                coll,
                filter,
                doc,
                upsert,
            } => {
                let res = db
                    .collection::<Document>(&coll)
                    .replace_one(filter, doc)
                    .upsert(upsert)
                    .run()
                    .map_err(fmt_err)?;
                Ok(update_result(
                    res.matched_count,
                    res.modified_count,
                    res.upserted_id,
                ))
            }
            Op::Delete { coll, filter, many } => {
                let c = db.collection::<Document>(&coll);
                let res = if many {
                    c.delete_many(filter).run()
                } else {
                    c.delete_one(filter).run()
                }
                .map_err(fmt_err)?;
                Ok(StmtResult::Affected {
                    rows: res.deleted_count,
                    message: String::new(),
                })
            }
            Op::CreateIndex {
                coll,
                keys,
                options,
            } => {
                let name = match options.get_str("name") {
                    Ok(n) => n.to_string(),
                    Err(_) => index_name(&keys),
                };
                let mut spec = doc! { "key": keys, "name": &name };
                for (k, v) in options {
                    if k != "name" {
                        spec.insert(k, v);
                    }
                }
                self.command(&db, doc! { "createIndexes": coll, "indexes": [spec] })?;
                msg(format!("Indice creato: {name}"))
            }
            Op::DropIndex { coll, index } => {
                self.command(&db, doc! { "dropIndexes": coll, "index": index })?;
                msg("Indice eliminato".into())
            }
            Op::ListIndexes { coll } => {
                let docs = self.cursor_docs(&db, doc! { "listIndexes": coll })?;
                Ok(StmtResult::Rows(docs_to_rowset(&docs)))
            }
            Op::DropCollection { coll } => {
                self.command(&db, doc! { "drop": &coll })?;
                msg(format!("Collection {coll} eliminata"))
            }
            Op::CollStats { coll } => {
                let r = self.command(&db, doc! { "collStats": coll })?;
                Ok(StmtResult::Rows(kv_rowset(&r)))
            }
            Op::Rename {
                coll,
                to,
                drop_target,
            } => {
                let name = db.name().to_string();
                self.command(
                    &self.client.database("admin"),
                    doc! {
                        "renameCollection": format!("{name}.{coll}"),
                        "to": format!("{name}.{to}"),
                        "dropTarget": drop_target,
                    },
                )?;
                msg(format!("Collection {coll} rinominata in {to}"))
            }
            Op::RunCommand { cmd, admin } => {
                let target = if admin {
                    self.client.database("admin")
                } else {
                    db
                };
                let r = self.command(&target, cmd)?;
                Ok(StmtResult::Rows(command_rowset(&r)))
            }
            Op::ListCollections => {
                let mut rows: Vec<Vec<Option<String>>> = self
                    .list_collections(&db)?
                    .into_iter()
                    .map(|t| {
                        vec![
                            Some(t.name),
                            Some(if t.is_view { "view" } else { "collection" }.into()),
                        ]
                    })
                    .collect();
                rows.sort_by_key(|r| r[0].clone().unwrap_or_default().to_lowercase());
                Ok(StmtResult::Rows(RowSet {
                    columns: cols(&[("name", "string"), ("type", "string")]),
                    rows,
                }))
            }
            Op::ListDatabases => {
                let r =
                    self.command(&self.client.database("admin"), doc! { "listDatabases": 1 })?;
                let rows = r
                    .get_array("databases")
                    .map(|a| a.as_slice())
                    .unwrap_or_default()
                    .iter()
                    .filter_map(Bson::as_document)
                    .map(|d| {
                        vec![
                            d.get_str("name").ok().map(String::from),
                            Some(human_bytes(&as_i64(d.get("sizeOnDisk")).to_string())),
                            d.get("empty").and_then(cell_text),
                        ]
                    })
                    .collect();
                Ok(StmtResult::Rows(RowSet {
                    columns: cols(&[("name", "string"), ("sizeOnDisk", ""), ("empty", "bool")]),
                    rows,
                }))
            }
            Op::Use(name) => {
                self.db = name;
                msg(format!("Database corrente: {}", self.db))
            }
            Op::CreateCollection { name, options } => {
                let mut cmd = doc! { "create": &name };
                cmd.extend(options);
                self.command(&db, cmd)?;
                msg(format!("Collection {name} creata"))
            }
            Op::DropDatabase => {
                self.command(&db, doc! { "dropDatabase": 1 })?;
                msg(format!("Database {} eliminato", db.name()))
            }
        }
    }

    fn count_pipeline(
        &self,
        db: &Database,
        coll: &str,
        pipeline: Vec<Document>,
        options: &Document,
    ) -> DbResult<i64> {
        let docs = self.cursor_docs(db, parse::aggregate_command(coll, &pipeline, options))?;
        Ok(docs.first().map(|d| as_i64(d.get("n"))).unwrap_or_default())
    }

    fn list_collections(&self, db: &Database) -> DbResult<Vec<TableEntry>> {
        let docs = self.cursor_docs(db, doc! { "listCollections": 1, "nameOnly": false })?;
        Ok(docs
            .iter()
            .filter_map(|d| {
                let name = d.get_str("name").ok()?;
                (!name.starts_with("system.")).then(|| TableEntry {
                    schema: db.name().to_string(),
                    name: name.to_string(),
                    is_view: d.get_str("type").is_ok_and(|t| t == "view"),
                })
            })
            .collect())
    }

    pub fn list_databases(&self) -> DbResult<Vec<String>> {
        self.client.list_database_names().run().map_err(fmt_err)
    }

    pub fn list_tables(&self, schema: &str) -> DbResult<Vec<TableEntry>> {
        let name = if schema.is_empty() { &self.db } else { schema };
        self.list_collections(&self.client.database(name))
    }

    /// Campi dedotti da un campione di documenti (anche annidati, in notazione puntata).
    pub fn list_columns(&self, schema: &str, coll: &str) -> DbResult<Vec<ColumnInfo>> {
        let db = self.database(Some(schema).filter(|s| !s.is_empty()));
        let (fields, sampled) = self.sample_fields(&db, coll)?;
        let indexed = self.indexed_fields(&db, coll).unwrap_or_default();
        Ok(fields
            .into_iter()
            .map(|(path, f)| ColumnInfo {
                key: if path == "_id" {
                    "PRI".into()
                } else {
                    indexed.get(&path).cloned().unwrap_or_default()
                },
                nullable: f.count < sampled || f.types.contains_key("null"),
                data_type: f.type_label(),
                name: path,
            })
            .collect())
    }

    fn sample_fields(
        &self,
        db: &Database,
        coll: &str,
    ) -> DbResult<(Vec<(String, FieldStats)>, u64)> {
        let docs = self.cursor_docs(
            db,
            doc! {
                "aggregate": coll,
                "pipeline": [ { "$sample": { "size": SAMPLE_SIZE } } ],
                "cursor": {},
                "maxTimeMS": 15000,
            },
        )?;
        let mut fields: BTreeMap<String, FieldStats> = BTreeMap::new();
        for d in &docs {
            collect_fields(d, "", 0, &mut fields);
        }
        let mut list: Vec<(String, FieldStats)> = fields.into_iter().collect();
        // _id per primo, poi in ordine alfabetico (i sotto-campi seguono il loro campo)
        list.sort_by_cached_key(|(p, _)| (p != "_id", p.to_lowercase()));
        Ok((list, docs.len() as u64))
    }

    /// Campo → "UNI"/"MUL" per i campi che compaiono in un indice.
    fn indexed_fields(&self, db: &Database, coll: &str) -> DbResult<HashMap<String, String>> {
        let mut out = HashMap::new();
        for ix in self.cursor_docs(db, doc! { "listIndexes": coll })? {
            let unique = matches!(ix.get("unique"), Some(Bson::Boolean(true)));
            if let Ok(key) = ix.get_document("key") {
                for k in key.keys() {
                    let e = out.entry(k.clone()).or_insert_with(|| "MUL".to_string());
                    if unique && key.len() == 1 {
                        *e = "UNI".into();
                    }
                }
            }
        }
        Ok(out)
    }

    pub fn load_schema_meta(&self) -> DbResult<SchemaMeta> {
        let schemas: Vec<String> = self
            .list_databases()?
            .into_iter()
            .filter(|d| !SYSTEM_DBS.contains(&d.as_str()))
            .collect();
        let tables = self
            .list_collections(&self.client.database(&self.db))?
            .into_iter()
            .map(|t| TableMeta {
                schema: t.schema,
                name: t.name,
                is_view: t.is_view,
                // i campi si leggono a campione solo quando servono
                columns: None,
            })
            .collect();
        Ok(SchemaMeta {
            default_schema: self.db.clone(),
            schemas,
            tables,
        })
    }

    pub fn analyze(&self, schema: &str, coll: &str) -> DbResult<TableAnalysis> {
        let db = self.database(Some(schema).filter(|s| !s.is_empty()));
        let mut a = TableAnalysis::default();
        let info = self
            .cursor_docs(
                &db,
                doc! { "listCollections": 1, "filter": { "name": coll } },
            )?
            .into_iter()
            .next()
            .ok_or_else(|| format!("Collection {coll} non trovata"))?;
        let is_view = info.get_str("type").is_ok_and(|t| t == "view");
        let options = info.get_document("options").cloned().unwrap_or_default();
        a.properties
            .push(("Database".into(), db.name().to_string()));
        a.properties.push((
            "Tipo".into(),
            if is_view { "vista" } else { "collection" }.into(),
        ));
        if !is_view && let Ok(st) = self.command(&db, doc! { "collStats": coll }) {
            let n = |k: &str| as_i64(st.get(k));
            a.properties
                .push(("Documenti".into(), thousands(n("count"))));
            a.properties.push((
                "Dimensione dati".into(),
                human_bytes(&n("size").to_string()),
            ));
            a.properties.push((
                "Dimensione media documento".into(),
                human_bytes(&n("avgObjSize").to_string()),
            ));
            a.properties.push((
                "Spazio su disco".into(),
                human_bytes(&n("storageSize").to_string()),
            ));
            a.properties.push((
                "Dimensione indici".into(),
                human_bytes(&n("totalIndexSize").to_string()),
            ));
            if matches!(st.get("capped"), Some(Bson::Boolean(true))) {
                a.properties.push((
                    "Capped".into(),
                    format!("sì, max {}", human_bytes(&n("maxSize").to_string())),
                ));
            }
        }
        if let Ok(v) = options.get_document("validator") {
            a.properties
                .push(("Validatore".into(), json(&Bson::Document(v.clone()))));
        }
        if is_view {
            if let Ok(src) = options.get_str("viewOn") {
                a.properties.push(("Vista su".into(), src.to_string()));
            }
            if let Ok(p) = options.get_array("pipeline") {
                a.properties
                    .push(("Pipeline".into(), json(&Bson::Array(p.clone()))));
            }
        }

        // Campi dedotti dal campione
        let (fields, sampled) = self.sample_fields(&db, coll)?;
        let indexed = if is_view {
            HashMap::new()
        } else {
            self.indexed_fields(&db, coll).unwrap_or_default()
        };
        a.properties.push((
            "Campi".into(),
            format!("{} (da un campione di {sampled} documenti)", fields.len()),
        ));
        let mut id_type = String::new();
        a.columns = RowSet {
            columns: cols(&[
                ("Campo", ""),
                ("Tipo", ""),
                ("Presenza", ""),
                ("Indice", ""),
            ]),
            rows: fields
                .iter()
                .map(|(path, f)| {
                    if path == "_id" {
                        id_type = f.type_label();
                    }
                    let pct = if sampled == 0 {
                        0.0
                    } else {
                        f.count as f64 * 100.0 / sampled as f64
                    };
                    vec![
                        Some(path.clone()),
                        Some(f.type_label()),
                        Some(format!("{pct:.0}%")),
                        Some(if path == "_id" {
                            "PRI".into()
                        } else {
                            indexed.get(path).cloned().unwrap_or_default()
                        }),
                    ]
                })
                .collect(),
        };

        // Indici
        let index_docs = if is_view {
            Vec::new()
        } else {
            self.cursor_docs(&db, doc! { "listIndexes": coll })?
        };
        let mut ddl = String::new();
        let mut create_opts = options.clone();
        create_opts.remove("viewOn");
        create_opts.remove("pipeline");
        if is_view {
            ddl.push_str(&format!(
                "db.createView({}, {}, {})\n",
                json(&Bson::String(coll.into())),
                json(&Bson::String(
                    options.get_str("viewOn").unwrap_or_default().into()
                )),
                json(
                    &options
                        .get("pipeline")
                        .cloned()
                        .unwrap_or(Bson::Array(vec![]))
                )
            ));
        } else if create_opts.is_empty() {
            ddl.push_str(&format!(
                "db.createCollection({})\n",
                json(&Bson::String(coll.into()))
            ));
        } else {
            ddl.push_str(&format!(
                "db.createCollection({}, {})\n",
                json(&Bson::String(coll.into())),
                json(&Bson::Document(create_opts))
            ));
        }
        let target = if parse::is_identifier(coll) {
            format!("db.{coll}")
        } else {
            format!("db.getCollection({})", json(&Bson::String(coll.into())))
        };
        for ix in &index_docs {
            let name = ix.get_str("name").unwrap_or_default().to_string();
            let key = ix.get_document("key").cloned().unwrap_or_default();
            let unique = matches!(ix.get("unique"), Some(Bson::Boolean(true)));
            let primary = name == "_id_";
            let mut method = key
                .values()
                .find_map(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| "btree".into());
            if matches!(ix.get("sparse"), Some(Bson::Boolean(true))) {
                method.push_str(", sparse");
            }
            if ix.contains_key("partialFilterExpression") {
                method.push_str(", parziale");
            }
            if let Some(ttl) = ix.get("expireAfterSeconds") {
                method.push_str(&format!(", TTL {} s", as_i64(Some(ttl))));
            }
            let columns = key
                .iter()
                .map(|(k, v)| match v {
                    Bson::String(s) => format!("{k} {s}"),
                    v if as_i64(Some(v)) < 0 => format!("{k} DESC"),
                    _ => format!("{k} ASC"),
                })
                .collect::<Vec<_>>()
                .join(", ");
            let info = IndexInfo {
                name: name.clone(),
                primary,
                unique: unique || primary,
                method,
                columns,
            };
            if primary {
                a.primary_key = Some(info.clone());
            } else {
                let mut opts = ix.clone();
                for k in ["v", "key", "ns"] {
                    opts.remove(k);
                }
                // la shell legacy salva le direzioni come double (1.0): si mostrano intere
                let key: Document = key
                    .into_iter()
                    .map(|(k, v)| match v {
                        Bson::Double(d) if d.fract() == 0.0 && d.abs() < 1e9 => {
                            (k, Bson::Int32(d as i32))
                        }
                        other => (k, other),
                    })
                    .collect();
                ddl.push_str(&format!(
                    "{target}.createIndex({}, {})\n",
                    json(&Bson::Document(key)),
                    json(&Bson::Document(opts))
                ));
            }
            a.indexes.push(info);
        }
        a.key_description = if is_view {
            "Vista: nessun indice proprio (usa quelli della collection di origine).".into()
        } else if id_type.is_empty() {
            "Chiave primaria: _id (collection vuota)".into()
        } else {
            let auto = if id_type == "objectId" {
                ", generato automaticamente se non indicato"
            } else {
                ""
            };
            format!("Chiave primaria: _id ({id_type}{auto}). MongoDB non ha chiavi esterne.")
        };
        a.ddl = ddl;
        Ok(a)
    }

    /// Esporta in streaming find e aggregate; le colonne sono quelle dei primi documenti.
    pub fn export_csv(
        &mut self,
        src: &str,
        w: &mut impl Write,
        opts: &CsvOptions,
        stop: &AtomicBool,
        progress: &dyn Fn(u64),
    ) -> DbResult<u64> {
        let planned = parse::parse_and_plan(src).map_err(|e| e.message)?;
        let db = self.database(planned.db.as_deref());
        let cmd = match &planned.op {
            Op::Find { coll, find } => parse::find_command(coll, find),
            Op::Aggregate {
                coll,
                pipeline,
                options,
            } => parse::aggregate_command(coll, pipeline, options),
            _ => {
                let StmtResult::Rows(rs) = self.execute(src)? else {
                    return Err("Il comando non restituisce documenti".into());
                };
                if opts.header {
                    write_csv_row(w, rs.columns.iter().map(|c| Some(c.name.as_str())), opts)?;
                }
                for r in &rs.rows {
                    write_csv_row(w, r.iter().map(|v| v.as_deref()), opts)?;
                }
                return Ok(rs.rows.len() as u64);
            }
        };
        const HEAD: usize = 1000;
        let mut cmd = cmd;
        cmd.insert("comment", self.begin());
        let res = (|| {
            let mut cursor = db.run_cursor_command(cmd).run().map_err(fmt_err)?;
            let mut head = Vec::new();
            for d in cursor.by_ref() {
                head.push(d.map_err(fmt_err)?);
                if head.len() >= HEAD {
                    break;
                }
            }
            let names = column_order(&head);
            if opts.header {
                write_csv_row(w, names.iter().map(|s| Some(s.as_str())), opts)?;
            }
            let mut n = 0u64;
            let mut write = |d: &Document, n: &mut u64| -> DbResult<()> {
                let vals: Vec<Option<String>> =
                    names.iter().map(|k| d.get(k).and_then(cell_text)).collect();
                write_csv_row(w, vals.iter().map(|v| v.as_deref()), opts)?;
                *n += 1;
                if n.is_multiple_of(10_000) {
                    if stop.load(Ordering::Relaxed) || self.stopped() {
                        return Err("Esportazione annullata".into());
                    }
                    progress(*n);
                }
                Ok(())
            };
            for d in &head {
                write(d, &mut n)?;
            }
            for d in cursor {
                write(&d.map_err(fmt_err)?, &mut n)?;
            }
            Ok(n)
        })();
        self.end();
        res
    }
}

fn update_result(matched: u64, modified: u64, upserted: Option<Bson>) -> StmtResult {
    let mut message = format!("corrispondenti {matched}");
    if let Some(id) = upserted {
        message.push_str(&format!(
            " · inserito _id: {}",
            cell_text(&id).unwrap_or_default()
        ));
    }
    StmtResult::Affected {
        rows: modified,
        message,
    }
}

/// Nome predefinito di un indice, come lo genera il server (`campo_1_altro_-1`).
fn index_name(keys: &Document) -> String {
    keys.iter()
        .map(|(k, v)| {
            let v = match v {
                Bson::String(s) => s.clone(),
                other => as_i64(Some(other)).to_string(),
            };
            format!("{k}_{v}")
        })
        .collect::<Vec<_>>()
        .join("_")
}

fn thousands(n: i64) -> String {
    let s = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

fn as_i64(v: Option<&Bson>) -> i64 {
    match v {
        Some(Bson::Int32(i)) => *i as i64,
        Some(Bson::Int64(i)) => *i,
        Some(Bson::Double(d)) => *d as i64,
        _ => 0,
    }
}

fn cols(list: &[(&str, &str)]) -> Vec<ColumnMeta> {
    list.iter()
        .map(|(n, t)| ColumnMeta {
            name: n.to_string(),
            type_name: t.to_string(),
        })
        .collect()
}

fn scalar(name: &str, n: i64) -> RowSet {
    RowSet {
        columns: cols(&[(name, "int64")]),
        rows: vec![vec![Some(n.to_string())]],
    }
}

// ---------------------------------------------------------------------------
// Conversione dei valori
// ---------------------------------------------------------------------------

/// Nome del tipo BSON, come in `$type`.
pub fn type_name(v: &Bson) -> &'static str {
    match v {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Array(_) => "array",
        Bson::Document(_) => "object",
        Bson::Boolean(_) => "bool",
        Bson::Null | Bson::Undefined => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) => "javascript",
        Bson::Int32(_) => "int32",
        Bson::Int64(_) => "int64",
        Bson::Timestamp(_) => "timestamp",
        Bson::Binary(b) if b.subtype == BinarySubtype::Uuid => "uuid",
        Bson::Binary(_) => "binData",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "date",
        Bson::Symbol(_) => "symbol",
        Bson::Decimal128(_) => "decimal128",
        Bson::MaxKey => "maxKey",
        Bson::MinKey => "minKey",
        Bson::DbPointer(_) => "dbPointer",
    }
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// JSON compatto (extended JSON relaxed): leggibile e rileggibile dal parser.
pub fn json(v: &Bson) -> String {
    v.clone().into_relaxed_extjson().to_string()
}

/// Testo della cella. `None` = null (o campo assente).
pub fn cell_text(v: &Bson) -> Option<String> {
    Some(match v {
        Bson::Null | Bson::Undefined => return None,
        Bson::String(s) => s.clone(),
        Bson::Int32(i) => i.to_string(),
        Bson::Int64(i) => i.to_string(),
        Bson::Double(d) if d.is_infinite() => {
            if *d > 0.0 { "Infinity" } else { "-Infinity" }.into()
        }
        Bson::Double(d) => d.to_string(),
        Bson::Boolean(b) => b.to_string(),
        Bson::ObjectId(o) => format!("ObjectId(\"{o}\")"),
        Bson::DateTime(d) => d
            .try_to_rfc3339_string()
            .unwrap_or_else(|_| d.timestamp_millis().to_string()),
        Bson::Decimal128(d) => d.to_string(),
        Bson::Binary(b) if b.subtype == BinarySubtype::Uuid => match b.to_uuid() {
            Ok(u) => format!("UUID(\"{u}\")"),
            Err(_) => format!("BinData(4, \"{}\")", base64(&b.bytes)),
        },
        Bson::Binary(b) => format!("BinData({}, \"{}\")", u8::from(b.subtype), base64(&b.bytes)),
        Bson::RegularExpression(r) => format!("/{}/{}", r.pattern, r.options),
        Bson::Timestamp(t) => format!("Timestamp({}, {})", t.time, t.increment),
        Bson::JavaScriptCode(c) => c.clone(),
        Bson::Symbol(s) => s.clone(),
        Bson::MinKey => "MinKey".into(),
        Bson::MaxKey => "MaxKey".into(),
        other => json(other),
    })
}

/// Ordine delle colonne: `_id` per primo, poi i campi nell'ordine in cui compaiono.
fn column_order(docs: &[Document]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut seen: HashMap<&str, ()> = HashMap::new();
    for d in docs {
        for k in d.keys() {
            if seen.insert(k.as_str(), ()).is_none() {
                names.push(k.clone());
            }
        }
    }
    if let Some(i) = names.iter().position(|n| n == "_id") {
        let id = names.remove(i);
        names.insert(0, id);
    }
    names
}

/// Tipo della colonna: quello comune ai valori non null, altrimenti "misto".
fn column_type<'a>(values: impl Iterator<Item = &'a Bson>) -> String {
    let mut found: Option<&'static str> = None;
    for v in values {
        let t = type_name(v);
        if t == "null" {
            continue;
        }
        match found {
            None => found = Some(t),
            Some(f) if f == t => {}
            Some(_) => return "misto".into(),
        }
    }
    found.unwrap_or("null").to_string()
}

pub fn docs_to_rowset(docs: &[Document]) -> RowSet {
    let names = column_order(docs);
    let columns = names
        .iter()
        .map(|n| ColumnMeta {
            name: n.clone(),
            type_name: column_type(docs.iter().filter_map(|d| d.get(n))),
        })
        .collect();
    let index: HashMap<&str, usize> = names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_str(), i))
        .collect();
    let rows = docs
        .iter()
        .map(|d| {
            let mut row = vec![None; names.len()];
            for (k, v) in d {
                row[index[k.as_str()]] = cell_text(v);
            }
            row
        })
        .collect();
    RowSet { columns, rows }
}

fn values_rowset(name: &str, values: &[Bson]) -> RowSet {
    RowSet {
        columns: vec![ColumnMeta {
            name: name.to_string(),
            type_name: column_type(values.iter()),
        }],
        rows: values.iter().map(|v| vec![cell_text(v)]).collect(),
    }
}

/// Un singolo documento (risultato di un comando) come coppie campo/valore.
fn kv_rowset(d: &Document) -> RowSet {
    RowSet {
        columns: cols(&[("campo", ""), ("valore", ""), ("tipo", "")]),
        rows: d
            .iter()
            .map(|(k, v)| {
                vec![
                    Some(k.clone()),
                    cell_text(v),
                    Some(type_name(v).to_string()),
                ]
            })
            .collect(),
    }
}

/// Risultato di runCommand: se contiene un cursore si mostrano i suoi documenti.
fn command_rowset(r: &Document) -> RowSet {
    if let Ok(batch) = r
        .get_document("cursor")
        .and_then(|c| c.get_array("firstBatch"))
    {
        let docs: Vec<Document> = batch
            .iter()
            .filter_map(|v| v.as_document().cloned())
            .collect();
        return docs_to_rowset(&docs);
    }
    kv_rowset(r)
}

// ---------------------------------------------------------------------------
// Campi dedotti
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FieldStats {
    /// Documenti in cui il campo compare.
    count: u64,
    types: BTreeMap<&'static str, u64>,
}

impl FieldStats {
    /// Tipi osservati, dal più frequente (es. "string | null").
    fn type_label(&self) -> String {
        let mut t: Vec<(&&str, &u64)> = self.types.iter().collect();
        t.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        t.iter().map(|(k, _)| **k).collect::<Vec<_>>().join(" | ")
    }
}

const MAX_DEPTH: usize = 3;
const MAX_FIELDS: usize = 500;

fn collect_fields(
    d: &Document,
    prefix: &str,
    depth: usize,
    out: &mut BTreeMap<String, FieldStats>,
) {
    for (k, v) in d {
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        if out.len() >= MAX_FIELDS && !out.contains_key(&path) {
            continue;
        }
        let f = out.entry(path.clone()).or_default();
        f.count += 1;
        *f.types.entry(type_name(v)).or_default() += 1;
        if depth >= MAX_DEPTH {
            continue;
        }
        match v {
            Bson::Document(sub) => collect_fields(sub, &path, depth + 1, out),
            // array di documenti: i campi degli elementi (in notazione puntata, come nelle query)
            Bson::Array(items) => {
                let mut sub_out: BTreeMap<String, FieldStats> = BTreeMap::new();
                for it in items {
                    if let Bson::Document(sub) = it {
                        collect_fields(sub, &path, depth + 1, &mut sub_out);
                    }
                }
                for (p, s) in sub_out {
                    let e = out.entry(p).or_default();
                    e.count += 1;
                    for t in s.types.keys() {
                        *e.types.entry(t).or_default() += 1;
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::{DateTime, oid::ObjectId};

    #[test]
    fn rowset_from_documents() {
        let oid = ObjectId::parse_str("5ceb9b5211c90d9af9b83548").unwrap();
        let docs = vec![
            doc! { "name": "a", "_id": oid, "n": 1, "sub": { "x": 1 } },
            doc! { "_id": 2, "n": 2.5, "extra": null, "when": DateTime::from_millis(0) },
        ];
        let rs = docs_to_rowset(&docs);
        let names: Vec<&str> = rs.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["_id", "name", "n", "sub", "extra", "when"]);
        assert_eq!(rs.columns[0].type_name, "misto");
        assert_eq!(rs.columns[1].type_name, "string");
        assert_eq!(rs.columns[4].type_name, "null");
        assert_eq!(
            rs.rows[0][0].as_deref(),
            Some("ObjectId(\"5ceb9b5211c90d9af9b83548\")")
        );
        assert_eq!(rs.rows[0][3].as_deref(), Some("{\"x\":1}"));
        assert_eq!(rs.rows[1][1], None);
        assert_eq!(rs.rows[1][5].as_deref(), Some("1970-01-01T00:00:00Z"));
    }

    #[test]
    fn json_round_trip() {
        let oid = ObjectId::new();
        let v = Bson::Document(
            doc! { "o": oid, "d": DateTime::from_millis(1_700_000_000_000_i64), "l": 5_000_000_000_i64 },
        );
        let back = parse::parse_value(&json(&v)).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn inferred_fields() {
        let mut out = BTreeMap::new();
        for d in [
            doc! { "a": 1, "m": { "x": "s" }, "v": [ { "n": 1 }, { "n": 2, "f": "z" } ] },
            doc! { "a": "t", "m": null },
        ] {
            collect_fields(&d, "", 0, &mut out);
        }
        let label = |p: &str| out[p].type_label();
        assert_eq!(label("a"), "int32 | string");
        assert_eq!(label("m"), "null | object");
        assert_eq!(label("m.x"), "string");
        assert_eq!(out["v.n"].count, 1);
        assert_eq!(label("v.f"), "string");
    }

    #[test]
    fn helpers() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(index_name(&doc! { "a": 1, "b": -1 }), "a_1_b_-1");
        assert_eq!(pct("a@b:c"), "a%40b%3Ac");
    }
}
