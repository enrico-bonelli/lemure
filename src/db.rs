//! Accesso ai database (MySQL e PostgreSQL) con driver sincroni.
//!
//! Ogni connessione vive su un thread dedicato (vedi `worker.rs`): qui ci sono
//! solo funzioni bloccanti, semplici e veloci. I risultati vengono letti con
//! il protocollo testuale, così qualunque tipo di dato diventa una stringa
//! senza conversioni costose.

use crate::completion::{CompColumn, SchemaMeta, TableMeta};
use crate::config::{DataSource, DbKind};
use crate::sqlutil::{self, Dialect};
use mysql::prelude::Queryable;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub type DbResult<T> = Result<T, String>;

#[derive(Clone, Debug)]
pub struct ColumnMeta {
    pub name: String,
    pub type_name: String,
}

/// Insieme di righe già convertite in testo. `None` = NULL.
#[derive(Clone, Debug, Default)]
pub struct RowSet {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Option<String>>>,
}

#[derive(Clone, Debug)]
pub enum StmtResult {
    Rows(RowSet),
    Affected { rows: u64, message: String },
}

#[derive(Clone, Debug)]
pub struct TableEntry {
    pub schema: String,
    pub name: String,
    pub is_view: bool,
}

#[derive(Clone, Debug)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub key: String,
    pub nullable: bool,
}

#[derive(Clone, Debug, Default)]
pub struct IndexInfo {
    pub name: String,
    pub primary: bool,
    pub unique: bool,
    pub method: String,
    pub columns: String,
}

#[derive(Clone, Debug, Default)]
pub struct ForeignKeyInfo {
    pub name: String,
    pub table: String,
    pub columns: String,
    pub ref_table: String,
    pub ref_columns: String,
    pub on_update: String,
    pub on_delete: String,
}

#[derive(Clone, Debug, Default)]
pub struct TableAnalysis {
    pub properties: Vec<(String, String)>,
    pub columns: RowSet,
    pub primary_key: Option<IndexInfo>,
    pub key_description: String,
    pub indexes: Vec<IndexInfo>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
    pub referenced_by: Vec<ForeignKeyInfo>,
    pub ddl: String,
}

#[derive(Clone, Debug)]
pub struct CsvOptions {
    pub separator: String,
    pub header: bool,
    pub quote_all: bool,
}

/// Permette di interrompere una query in corso da un altro thread.
#[derive(Clone)]
pub enum CancelHandle {
    MySql {
        opts: mysql::Opts,
        connection_id: u32,
    },
    Postgres(postgres::CancelToken),
}

impl CancelHandle {
    pub fn cancel(&self) -> DbResult<()> {
        match self {
            CancelHandle::MySql {
                opts,
                connection_id,
            } => {
                let mut c = mysql::Conn::new(opts.clone()).map_err(fmt_my_err)?;
                c.query_drop(format!("KILL QUERY {connection_id}"))
                    .map_err(fmt_my_err)
            }
            CancelHandle::Postgres(tok) => tok.cancel_query(postgres::NoTls).map_err(fmt_pg_err),
        }
    }
}

pub enum DbConn {
    MySql {
        conn: mysql::Conn,
        opts: mysql::Opts,
    },
    Postgres(postgres::Client),
}

pub fn dialect_of(kind: DbKind) -> Dialect {
    match kind {
        DbKind::MySql => Dialect::MySql,
        DbKind::Postgres => Dialect::Postgres,
    }
}

fn mysql_opts(ds: &DataSource) -> mysql::Opts {
    let db = (!ds.database.trim().is_empty()).then(|| ds.database.trim().to_string());
    mysql::OptsBuilder::new()
        .ip_or_hostname(Some(ds.host.trim()))
        .tcp_port(ds.port)
        .user(Some(ds.user.clone()))
        .pass(Some(ds.password.clone()))
        .db_name(db)
        .prefer_socket(false)
        .tcp_connect_timeout(Some(Duration::from_secs(10)))
        .into()
}

fn fmt_my_err(e: mysql::Error) -> String {
    match e {
        mysql::Error::MySqlError(e) => format!("ERRORE {} ({}): {}", e.code, e.state, e.message),
        other => other.to_string(),
    }
}

fn fmt_pg_err(e: postgres::Error) -> String {
    if let Some(db) = e.as_db_error() {
        let mut s = format!("{} {}: {}", db.severity(), db.code().code(), db.message());
        if let Some(d) = db.detail() {
            s.push_str(&format!("\nDETTAGLIO: {d}"));
        }
        if let Some(h) = db.hint() {
            s.push_str(&format!("\nSUGGERIMENTO: {h}"));
        }
        if let Some(postgres::error::ErrorPosition::Original(p)) = db.position() {
            s.push_str(&format!("\nPOSIZIONE: carattere {p}"));
        }
        s
    } else {
        // Le cause annidate (es. connessione rifiutata) sono le più utili.
        let mut s = e.to_string();
        let mut src = std::error::Error::source(&e);
        while let Some(inner) = src {
            s.push_str(&format!(": {inner}"));
            src = inner.source();
        }
        s
    }
}

impl DbConn {
    pub fn open(ds: &DataSource) -> DbResult<DbConn> {
        match ds.kind {
            DbKind::MySql => {
                let opts = mysql_opts(ds);
                let conn = mysql::Conn::new(opts.clone()).map_err(fmt_my_err)?;
                Ok(DbConn::MySql { conn, opts })
            }
            DbKind::Postgres => {
                let mut cfg = postgres::Config::new();
                cfg.host(ds.host.trim())
                    .port(ds.port)
                    .user(&ds.user)
                    .password(&ds.password)
                    .application_name("lemure")
                    .connect_timeout(Duration::from_secs(10));
                if !ds.database.trim().is_empty() {
                    cfg.dbname(ds.database.trim());
                }
                let client = cfg.connect(postgres::NoTls).map_err(fmt_pg_err)?;
                Ok(DbConn::Postgres(client))
            }
        }
    }

    pub fn dialect(&self) -> Dialect {
        match self {
            DbConn::MySql { .. } => Dialect::MySql,
            DbConn::Postgres(_) => Dialect::Postgres,
        }
    }

    pub fn cancel_handle(&self) -> CancelHandle {
        match self {
            DbConn::MySql { conn, opts } => CancelHandle::MySql {
                opts: opts.clone(),
                connection_id: conn.connection_id(),
            },
            DbConn::Postgres(c) => CancelHandle::Postgres(c.cancel_token()),
        }
    }

    pub fn server_version(&mut self) -> String {
        match self {
            DbConn::MySql { conn, .. } => conn
                .query_first::<String, _>("SELECT VERSION()")
                .ok()
                .flatten()
                .map(|v| format!("MySQL {v}"))
                .unwrap_or_else(|| {
                    let (a, b, c) = conn.server_version();
                    format!("MySQL {a}.{b}.{c}")
                }),
            DbConn::Postgres(c) => c
                .query_one("SHOW server_version", &[])
                .map(|r| format!("PostgreSQL {}", r.get::<_, String>(0)))
                .unwrap_or_else(|_| "PostgreSQL".into()),
        }
    }

    /// Verifica che la connessione sia viva (usato dopo lunghi periodi di inattività).
    pub fn is_alive(&mut self) -> bool {
        match self {
            DbConn::MySql { conn, .. } => conn.ping().is_ok(),
            DbConn::Postgres(c) => !c.is_closed() && c.simple_query("").is_ok(),
        }
    }

    /// Esegue uno statement. Se ci sono più result set viene restituito
    /// l'ultimo che contiene righe.
    pub fn execute(&mut self, sql: &str) -> DbResult<StmtResult> {
        match self {
            DbConn::MySql { conn, .. } => mysql_execute(conn, sql),
            DbConn::Postgres(c) => pg_execute(c, sql),
        }
    }

    pub fn query_scalar(&mut self, sql: &str) -> DbResult<Option<String>> {
        match self.execute(sql)? {
            StmtResult::Rows(rs) => Ok(rs
                .rows
                .into_iter()
                .next()
                .and_then(|r| r.into_iter().next().flatten())),
            StmtResult::Affected { .. } => Ok(None),
        }
    }

    pub fn current_schema(&mut self) -> DbResult<String> {
        let sql = match self {
            DbConn::MySql { .. } => "SELECT DATABASE()",
            DbConn::Postgres(_) => "SELECT current_schema()",
        };
        Ok(self.query_scalar(sql)?.unwrap_or_default())
    }

    pub fn list_databases(&mut self) -> DbResult<Vec<String>> {
        let mut out: Vec<String> = match self {
            DbConn::MySql { conn, .. } => conn.query("SHOW DATABASES").map_err(fmt_my_err)?,
            DbConn::Postgres(c) => c
                .query(
                    "SELECT nspname::text FROM pg_namespace WHERE nspname NOT LIKE 'pg\\_%' \
                     AND nspname <> 'information_schema'",
                    &[],
                )
                .map_err(fmt_pg_err)?
                .iter()
                .map(|r| r.get(0))
                .collect(),
        };
        sort_ci(&mut out, |s| s);
        Ok(out)
    }

    /// Tabelle e viste. Per MySQL `schema` è il database; per PostgreSQL se
    /// `schema` è vuoto vengono elencati tutti gli schemi utente.
    pub fn list_tables(&mut self, schema: &str) -> DbResult<Vec<TableEntry>> {
        let mut out: Vec<TableEntry> = match self {
            DbConn::MySql { conn, .. } => conn
                .exec_map(
                    "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES \
                     WHERE TABLE_SCHEMA = ?",
                    (schema,),
                    |(s, n, t): (String, String, String)| TableEntry {
                        schema: s,
                        name: n,
                        is_view: t.contains("VIEW"),
                    },
                )
                .map_err(fmt_my_err)?,
            DbConn::Postgres(c) => c
                .query(
                    "SELECT n.nspname::text, c.relname::text, c.relkind::text FROM pg_class c \
                     JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE c.relkind IN ('r','v','m','p','f') \
                       AND ($1 = '' OR n.nspname = $1) \
                       AND n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema'",
                    &[&schema],
                )
                .map_err(fmt_pg_err)?
                .iter()
                .map(|r| {
                    let kind: String = r.get(2);
                    TableEntry {
                        schema: r.get(0),
                        name: r.get(1),
                        is_view: kind == "v" || kind == "m",
                    }
                })
                .collect(),
        };
        sort_ci(&mut out, |t| &t.name);
        Ok(out)
    }

    pub fn list_columns(&mut self, schema: &str, table: &str) -> DbResult<Vec<ColumnInfo>> {
        let mut out: Vec<ColumnInfo> = match self {
            DbConn::MySql { conn, .. } => conn
                .exec_map(
                    "SELECT COLUMN_NAME, COLUMN_TYPE, COLUMN_KEY, IS_NULLABLE FROM information_schema.COLUMNS \
                     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
                    (schema, table),
                    |(n, t, k, nl): (String, String, String, String)| ColumnInfo {
                        name: n,
                        data_type: t,
                        key: k,
                        nullable: nl == "YES",
                    },
                )
                .map_err(fmt_my_err)?,
            DbConn::Postgres(c) => c
                .query(
                    "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), \
                       CASE WHEN EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conrelid = a.attrelid \
                              AND k.contype = 'p' AND a.attnum = ANY(k.conkey)) THEN 'PRI' \
                            WHEN EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conrelid = a.attrelid \
                              AND k.contype = 'u' AND a.attnum = ANY(k.conkey)) THEN 'UNI' \
                            WHEN EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conrelid = a.attrelid \
                              AND k.contype = 'f' AND a.attnum = ANY(k.conkey)) THEN 'FK' ELSE '' END, \
                       NOT a.attnotnull \
                     FROM pg_attribute a \
                     WHERE a.attrelid = (quote_ident($1) || '.' || quote_ident($2))::regclass \
                       AND a.attnum > 0 AND NOT a.attisdropped",
                    &[&schema, &table],
                )
                .map_err(fmt_pg_err)?
                .iter()
                .map(|r| ColumnInfo { name: r.get(0), data_type: r.get(1), key: r.get(2), nullable: r.get(3) })
                .collect(),
        };
        sort_ci(&mut out, |c| &c.name);
        Ok(out)
    }

    /// Metadati per l'autocompletamento: tutte le tabelle e (se lo schema non è
    /// enorme) tutte le colonne, con due sole query. Oltre `max_tables` tabelle
    /// le colonne vengono lasciate da caricare su richiesta.
    pub fn load_schema_meta(&mut self, max_tables: usize) -> DbResult<SchemaMeta> {
        use std::collections::HashMap;
        let mut meta = SchemaMeta {
            default_schema: self.current_schema()?,
            ..Default::default()
        };
        let mut col_rows: Vec<(String, String, String, String)> = Vec::new();
        match self {
            DbConn::MySql { conn, .. } => {
                const SYSTEM: &str = "('mysql','sys','information_schema','performance_schema')";
                meta.schemas = conn
                    .query::<String, _>("SHOW DATABASES")
                    .map_err(fmt_my_err)?
                    .into_iter()
                    .filter(|d| !SYSTEM.contains(&format!("'{d}'")))
                    .collect();
                // Con un database predefinito si caricano solo le sue tabelle.
                let filter = if meta.default_schema.is_empty() {
                    format!("TABLE_SCHEMA NOT IN {SYSTEM}")
                } else {
                    "TABLE_SCHEMA = DATABASE()".to_string()
                };
                let tables: Vec<(String, String, String)> = conn
                    .query(format!(
                        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE {filter}"
                    ))
                    .map_err(fmt_my_err)?;
                for (schema, name, kind) in tables {
                    meta.tables.push(TableMeta {
                        schema,
                        name,
                        is_view: kind.contains("VIEW"),
                        columns: None,
                    });
                }
                if meta.tables.len() <= max_tables {
                    col_rows = conn
                        .query(format!(
                            "SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, COLUMN_TYPE \
                             FROM information_schema.COLUMNS WHERE {filter} ORDER BY ORDINAL_POSITION"
                        ))
                        .map_err(fmt_my_err)?;
                }
            }
            DbConn::Postgres(c) => {
                const USER_NS: &str =
                    "n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema'";
                meta.schemas = c
                    .query(
                        &format!("SELECT n.nspname::text FROM pg_namespace n WHERE {USER_NS}"),
                        &[],
                    )
                    .map_err(fmt_pg_err)?
                    .iter()
                    .map(|r| r.get(0))
                    .collect();
                let rows = c
                    .query(
                        &format!(
                            "SELECT n.nspname::text, c.relname::text, c.relkind IN ('v','m') FROM pg_class c \
                             JOIN pg_namespace n ON n.oid = c.relnamespace \
                             WHERE c.relkind IN ('r','v','m','p','f') AND {USER_NS}"
                        ),
                        &[],
                    )
                    .map_err(fmt_pg_err)?;
                for r in &rows {
                    meta.tables.push(TableMeta {
                        schema: r.get(0),
                        name: r.get(1),
                        is_view: r.get(2),
                        columns: None,
                    });
                }
                if meta.tables.len() <= max_tables {
                    col_rows = c
                        .query(
                            &format!(
                                "SELECT n.nspname::text, c.relname::text, a.attname::text, \
                                   format_type(a.atttypid, a.atttypmod) \
                                 FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
                                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                                 WHERE c.relkind IN ('r','v','m','p','f') AND a.attnum > 0 \
                                   AND NOT a.attisdropped AND {USER_NS} ORDER BY a.attnum"
                            ),
                            &[],
                        )
                        .map_err(fmt_pg_err)?
                        .iter()
                        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
                        .collect();
                }
            }
        }
        if meta.tables.len() <= max_tables {
            let mut by_table: HashMap<(String, String), Vec<CompColumn>> = HashMap::new();
            for (schema, table, name, type_name) in col_rows {
                by_table
                    .entry((schema, table))
                    .or_default()
                    .push(CompColumn { name, type_name });
            }
            for t in &mut meta.tables {
                t.columns = Some(
                    by_table
                        .remove(&(t.schema.clone(), t.name.clone()))
                        .unwrap_or_default(),
                );
            }
        }
        Ok(meta)
    }

    pub fn analyze_table(&mut self, schema: &str, table: &str) -> DbResult<TableAnalysis> {
        match self {
            DbConn::MySql { conn, .. } => mysql_analyze(conn, schema, table),
            DbConn::Postgres(c) => pg_analyze(c, schema, table),
        }
    }

    /// Esporta in CSV lo statement in streaming, senza tenere le righe in memoria.
    pub fn export_csv(
        &mut self,
        sql: &str,
        path: &std::path::Path,
        opts: &CsvOptions,
        stop: &AtomicBool,
        progress: &dyn Fn(u64),
    ) -> DbResult<u64> {
        let file = File::create(path)
            .map_err(|e| format!("Impossibile creare {}: {e}", path.display()))?;
        let mut w = BufWriter::with_capacity(1 << 20, file);
        let n = match self {
            DbConn::MySql { conn, .. } => mysql_export(conn, sql, &mut w, opts, stop, progress)?,
            DbConn::Postgres(c) => pg_export(c, sql, &mut w, opts, stop, progress)?,
        };
        w.flush().map_err(|e| e.to_string())?;
        Ok(n)
    }
}

fn sort_ci<T>(v: &mut [T], key: impl Fn(&T) -> &str) {
    v.sort_by_cached_key(|t| key(t).to_lowercase());
}

// ---------------------------------------------------------------------------
// MySQL
// ---------------------------------------------------------------------------

fn mysql_type_name(c: &mysql::Column) -> String {
    use mysql::consts::{ColumnFlags, ColumnType::*};
    let binary = c.character_set() == 63;
    let name = match c.column_type() {
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "DECIMAL",
        MYSQL_TYPE_TINY => "TINYINT",
        MYSQL_TYPE_SHORT => "SMALLINT",
        MYSQL_TYPE_LONG => "INT",
        MYSQL_TYPE_FLOAT => "FLOAT",
        MYSQL_TYPE_DOUBLE => "DOUBLE",
        MYSQL_TYPE_NULL => "NULL",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "TIMESTAMP",
        MYSQL_TYPE_LONGLONG => "BIGINT",
        MYSQL_TYPE_INT24 => "MEDIUMINT",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "DATE",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "TIME",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "DATETIME",
        MYSQL_TYPE_YEAR => "YEAR",
        MYSQL_TYPE_BIT => "BIT",
        MYSQL_TYPE_JSON => "JSON",
        MYSQL_TYPE_ENUM => "ENUM",
        MYSQL_TYPE_SET => "SET",
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            if binary {
                "BLOB"
            } else {
                "TEXT"
            }
        }
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING => {
            if binary {
                "VARBINARY"
            } else {
                "VARCHAR"
            }
        }
        MYSQL_TYPE_STRING => {
            if c.flags().contains(ColumnFlags::ENUM_FLAG) {
                "ENUM"
            } else if c.flags().contains(ColumnFlags::SET_FLAG) {
                "SET"
            } else if binary {
                "BINARY"
            } else {
                "CHAR"
            }
        }
        MYSQL_TYPE_GEOMETRY => "GEOMETRY",
        MYSQL_TYPE_VECTOR => "VECTOR",
        _ => "?",
    };
    if c.flags().contains(ColumnFlags::UNSIGNED_FLAG) {
        format!("{name} UNSIGNED")
    } else {
        name.to_string()
    }
}

/// Le colonne binarie vengono mostrate in esadecimale.
fn mysql_is_binary(c: &mysql::Column) -> bool {
    use mysql::consts::ColumnType::*;
    c.character_set() == 63
        && matches!(
            c.column_type(),
            MYSQL_TYPE_TINY_BLOB
                | MYSQL_TYPE_MEDIUM_BLOB
                | MYSQL_TYPE_LONG_BLOB
                | MYSQL_TYPE_BLOB
                | MYSQL_TYPE_VARCHAR
                | MYSQL_TYPE_VAR_STRING
                | MYSQL_TYPE_STRING
                | MYSQL_TYPE_BIT
                | MYSQL_TYPE_GEOMETRY
        )
}

fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789ABCDEF";
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

fn mysql_value(v: mysql::Value, binary: bool) -> Option<String> {
    use mysql::Value;
    Some(match v {
        Value::NULL => return None,
        Value::Bytes(b) => {
            if binary {
                hex(&b)
            } else {
                String::from_utf8(b)
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
            }
        }
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Date(y, m, d, h, i, s, us) => {
            if us > 0 {
                format!("{y:04}-{m:02}-{d:02} {h:02}:{i:02}:{s:02}.{us:06}")
            } else {
                format!("{y:04}-{m:02}-{d:02} {h:02}:{i:02}:{s:02}")
            }
        }
        Value::Time(neg, d, h, i, s, us) => {
            let sign = if neg { "-" } else { "" };
            let h = d * 24 + h as u32;
            if us > 0 {
                format!("{sign}{h:02}:{i:02}:{s:02}.{us:06}")
            } else {
                format!("{sign}{h:02}:{i:02}:{s:02}")
            }
        }
    })
}

fn mysql_execute(conn: &mut mysql::Conn, sql: &str) -> DbResult<StmtResult> {
    let mut result = conn.query_iter(sql).map_err(fmt_my_err)?;
    let mut last_rows: Option<RowSet> = None;
    let mut affected = 0u64;
    let mut message = String::new();
    while let Some(set) = result.iter() {
        let cols = set.columns();
        let cols = cols.as_ref();
        if cols.is_empty() {
            affected += set.affected_rows();
            let info = set.info_str();
            if !info.is_empty() {
                message = info.into_owned();
            }
            if let Some(id) = set.last_insert_id().filter(|&id| id > 0) {
                message = format!("{message} Ultimo ID inserito: {id}")
                    .trim()
                    .to_string();
            }
            if set.warnings() > 0 {
                message = format!("{message} Avvisi: {}", set.warnings())
                    .trim()
                    .to_string();
            }
            continue;
        }
        let binary: Vec<bool> = cols.iter().map(mysql_is_binary).collect();
        let columns = cols
            .iter()
            .map(|c| ColumnMeta {
                name: c.name_str().into_owned(),
                type_name: mysql_type_name(c),
            })
            .collect();
        let mut rows = Vec::new();
        for row in set {
            let row = row.map_err(fmt_my_err)?;
            rows.push(
                row.unwrap()
                    .into_iter()
                    .zip(&binary)
                    .map(|(v, &b)| mysql_value(v, b))
                    .collect(),
            );
        }
        last_rows = Some(RowSet { columns, rows });
    }
    Ok(match last_rows {
        Some(rs) => StmtResult::Rows(rs),
        None => StmtResult::Affected {
            rows: affected,
            message,
        },
    })
}

fn mysql_export(
    conn: &mut mysql::Conn,
    sql: &str,
    w: &mut impl Write,
    opts: &CsvOptions,
    stop: &AtomicBool,
    progress: &dyn Fn(u64),
) -> DbResult<u64> {
    let mut result = conn.query_iter(sql).map_err(fmt_my_err)?;
    let mut n = 0u64;
    let mut written_set = false;
    while let Some(set) = result.iter() {
        let cols = set.columns();
        let cols = cols.as_ref();
        if cols.is_empty() || written_set {
            continue;
        }
        written_set = true;
        let binary: Vec<bool> = cols.iter().map(mysql_is_binary).collect();
        if opts.header {
            let names: Vec<String> = cols.iter().map(|c| c.name_str().into_owned()).collect();
            write_csv_row(w, names.iter().map(|s| Some(s.as_str())), opts)?;
        }
        for row in set {
            let row = row.map_err(fmt_my_err)?;
            let vals: Vec<Option<String>> = row
                .unwrap()
                .into_iter()
                .zip(&binary)
                .map(|(v, &b)| mysql_value(v, b))
                .collect();
            write_csv_row(w, vals.iter().map(|v| v.as_deref()), opts)?;
            n += 1;
            if n.is_multiple_of(10_000) {
                if stop.load(Ordering::Relaxed) {
                    return Err("Esportazione annullata".into());
                }
                progress(n);
            }
        }
    }
    Ok(n)
}

fn mysql_analyze(conn: &mut mysql::Conn, schema: &str, table: &str) -> DbResult<TableAnalysis> {
    let mut a = TableAnalysis::default();
    let info: Option<mysql::Row> = conn
        .exec_first(
            "SELECT TABLE_TYPE, ENGINE, TABLE_ROWS, AUTO_INCREMENT, TABLE_COLLATION, DATA_LENGTH, \
             INDEX_LENGTH, CREATE_TIME, UPDATE_TIME, TABLE_COMMENT, ROW_FORMAT \
             FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
            (schema, table),
        )
        .map_err(fmt_my_err)?;
    let info = info.ok_or_else(|| format!("Tabella {schema}.{table} non trovata"))?;
    let labels = [
        "Tipo",
        "Engine",
        "Righe (stima)",
        "Prossimo AUTO_INCREMENT",
        "Collation",
        "Dimensione dati",
        "Dimensione indici",
        "Creata il",
        "Aggiornata il",
        "Commento",
        "Formato righe",
    ];
    a.properties
        .push(("Tabella".into(), format!("{schema}.{table}")));
    for (i, l) in labels.iter().enumerate() {
        let v = info
            .as_ref(i)
            .cloned()
            .and_then(|v| mysql_value(v, false))
            .unwrap_or_default();
        let v = if i == 5 || i == 6 { human_bytes(&v) } else { v };
        if !v.is_empty() {
            a.properties.push((l.to_string(), v));
        }
    }

    // Colonne in ordine di definizione
    let cols: Vec<mysql::Row> = conn
        .exec(
            "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, COLUMN_DEFAULT, EXTRA, \
             COLLATION_NAME, COLUMN_COMMENT FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
            (schema, table),
        )
        .map_err(fmt_my_err)?;
    a.columns = rowset_from_mysql(
        &[
            "Colonna",
            "Tipo",
            "Null",
            "Chiave",
            "Default",
            "Extra",
            "Collation",
            "Commento",
        ],
        cols,
    );

    // Indici
    let idx: Vec<(String, i64, String, Option<String>, Option<String>)> = conn
        .exec(
            "SELECT INDEX_NAME, NON_UNIQUE, INDEX_TYPE, COLUMN_NAME, CAST(SUB_PART AS CHAR) \
             FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
             ORDER BY INDEX_NAME = 'PRIMARY' DESC, INDEX_NAME, SEQ_IN_INDEX",
            (schema, table),
        )
        .map_err(fmt_my_err)?;
    for (name, non_unique, method, col, sub) in idx {
        let col = match (col, sub) {
            (Some(c), Some(s)) => format!("{c}({s})"),
            (Some(c), None) => c,
            (None, _) => "(espressione)".into(),
        };
        match a.indexes.last_mut() {
            Some(last) if last.name == name => {
                last.columns.push_str(", ");
                last.columns.push_str(&col);
            }
            _ => a.indexes.push(IndexInfo {
                primary: name == "PRIMARY",
                unique: non_unique == 0,
                name,
                method,
                columns: col,
            }),
        }
    }
    a.primary_key = a.indexes.iter().find(|i| i.primary).cloned();

    let fk_sql = "SELECT k.CONSTRAINT_NAME, CONCAT(k.TABLE_SCHEMA, '.', k.TABLE_NAME), k.COLUMN_NAME, \
         CONCAT(k.REFERENCED_TABLE_SCHEMA, '.', k.REFERENCED_TABLE_NAME), k.REFERENCED_COLUMN_NAME, \
         r.UPDATE_RULE, r.DELETE_RULE \
         FROM information_schema.KEY_COLUMN_USAGE k \
         JOIN information_schema.REFERENTIAL_CONSTRAINTS r \
           ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME \
          AND r.TABLE_NAME = k.TABLE_NAME \
         WHERE k.REFERENCED_TABLE_NAME IS NOT NULL AND ";
    type FkRow = (String, String, String, String, String, String, String);
    let outgoing: Vec<FkRow> = conn
        .exec(
            format!("{fk_sql} k.TABLE_SCHEMA = ? AND k.TABLE_NAME = ? ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION"),
            (schema, table),
        )
        .map_err(fmt_my_err)?;
    let incoming: Vec<FkRow> = conn
        .exec(
            format!(
                "{fk_sql} k.REFERENCED_TABLE_SCHEMA = ? AND k.REFERENCED_TABLE_NAME = ? \
                 ORDER BY k.TABLE_NAME, k.CONSTRAINT_NAME, k.ORDINAL_POSITION"
            ),
            (schema, table),
        )
        .map_err(fmt_my_err)?;
    a.foreign_keys = group_fks(outgoing);
    a.referenced_by = group_fks(incoming);

    let auto_inc: Option<String> = conn
        .exec_first(
            "SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
             AND EXTRA LIKE '%auto_increment%'",
            (schema, table),
        )
        .map_err(fmt_my_err)?;
    a.key_description = describe_key(
        a.primary_key.as_ref(),
        auto_inc.map(|c| format!("AUTO_INCREMENT su {c}")),
    );

    let ddl: Option<(String, String)> = conn
        .query_first(format!(
            "SHOW CREATE TABLE {}.{}",
            sqlutil::quote_ident(schema, Dialect::MySql),
            sqlutil::quote_ident(table, Dialect::MySql)
        ))
        .unwrap_or(None);
    a.ddl = ddl.map(|(_, d)| d).unwrap_or_default();
    Ok(a)
}

fn rowset_from_mysql(headers: &[&str], rows: Vec<mysql::Row>) -> RowSet {
    RowSet {
        columns: headers
            .iter()
            .map(|h| ColumnMeta {
                name: h.to_string(),
                type_name: String::new(),
            })
            .collect(),
        rows: rows
            .into_iter()
            .map(|r| {
                r.unwrap()
                    .into_iter()
                    .map(|v| mysql_value(v, false))
                    .collect()
            })
            .collect(),
    }
}

fn group_fks(
    rows: Vec<(String, String, String, String, String, String, String)>,
) -> Vec<ForeignKeyInfo> {
    let mut out: Vec<ForeignKeyInfo> = Vec::new();
    for (name, tbl, col, rtbl, rcol, upd, del) in rows {
        match out.last_mut() {
            Some(l) if l.name == name && l.table == tbl => {
                l.columns.push_str(", ");
                l.columns.push_str(&col);
                l.ref_columns.push_str(", ");
                l.ref_columns.push_str(&rcol);
            }
            _ => out.push(ForeignKeyInfo {
                name,
                table: tbl,
                columns: col,
                ref_table: rtbl,
                ref_columns: rcol,
                on_update: upd,
                on_delete: del,
            }),
        }
    }
    out
}

fn describe_key(pk: Option<&IndexInfo>, generated: Option<String>) -> String {
    match pk {
        None => "Nessuna chiave primaria".into(),
        Some(pk) => {
            let n = pk.columns.split(", ").count();
            let kind = if n > 1 {
                format!("Chiave primaria composta ({n} colonne)")
            } else {
                "Chiave primaria semplice".into()
            };
            let mut s = format!("{kind}: ({})", pk.columns);
            if !pk.method.is_empty() {
                s.push_str(&format!(" — indice {}", pk.method));
            }
            if let Some(g) = generated {
                s.push_str(&format!(" — {g}"));
            }
            s
        }
    }
}

fn human_bytes(v: &str) -> String {
    let Ok(n) = v.parse::<f64>() else {
        return v.to_string();
    };
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut x = n;
    let mut u = 0;
    while x >= 1024.0 && u < units.len() - 1 {
        x /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{x:.1} {}", units[u])
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL
// ---------------------------------------------------------------------------

fn pg_execute(c: &mut postgres::Client, sql: &str) -> DbResult<StmtResult> {
    use postgres::SimpleQueryMessage as M;
    let msgs = c.simple_query(sql).map_err(fmt_pg_err)?;
    let mut last_rows: Option<RowSet> = None;
    let mut cur: Option<RowSet> = None;
    let mut affected = 0u64;
    let mut statements = 0usize;
    for m in msgs {
        match m {
            M::RowDescription(cols) => {
                cur = Some(RowSet {
                    columns: cols
                        .iter()
                        .map(|c| ColumnMeta {
                            name: c.name().to_string(),
                            type_name: String::new(),
                        })
                        .collect(),
                    rows: Vec::new(),
                })
            }
            M::Row(r) => {
                if let Some(rs) = cur.as_mut() {
                    rs.rows
                        .push((0..r.len()).map(|i| r.get(i).map(str::to_string)).collect());
                }
            }
            M::CommandComplete(n) => {
                statements += 1;
                match cur.take() {
                    Some(rs) => last_rows = Some(rs),
                    None => affected += n,
                }
            }
            _ => {}
        }
    }
    match last_rows {
        Some(mut rs) => {
            // I tipi delle colonne si ottengono preparando lo statement (non lo esegue).
            if statements == 1
                && let Ok(st) = c.prepare(sql)
                && st.columns().len() == rs.columns.len()
            {
                for (m, col) in rs.columns.iter_mut().zip(st.columns()) {
                    m.type_name = col.type_().name().to_string();
                }
            }
            Ok(StmtResult::Rows(rs))
        }
        None => Ok(StmtResult::Affected {
            rows: affected,
            message: String::new(),
        }),
    }
}

fn pg_export(
    c: &mut postgres::Client,
    sql: &str,
    w: &mut impl Write,
    opts: &CsvOptions,
    stop: &AtomicBool,
    progress: &dyn Fn(u64),
) -> DbResult<u64> {
    use std::io::Read;
    let sep = opts.separator.as_bytes();
    // Percorso veloce: COPY ... TO STDOUT, il server produce direttamente il CSV.
    if let (Some(base), [sep_byte]) = (sqlutil::pageable_base(sql, Dialect::Postgres), sep)
        && sep_byte.is_ascii()
        && *sep_byte != b'\''
        && *sep_byte != b'"'
        && *sep_byte != b'\n'
    {
        let quote_all = if opts.quote_all {
            ", FORCE_QUOTE *"
        } else {
            ""
        };
        let copy = format!(
            "COPY ({base}) TO STDOUT WITH (FORMAT csv, DELIMITER '{}', HEADER {}{quote_all})",
            *sep_byte as char, opts.header
        );
        let mut reader = c.copy_out(copy.as_str()).map_err(fmt_pg_err)?;
        let mut buf = vec![0u8; 1 << 16];
        let mut lines = 0u64;
        let mut last_report = 0u64;
        // I ritorni a capo dentro i valori quotati non chiudono la riga.
        let mut in_quotes = false;
        loop {
            let k = reader.read(&mut buf).map_err(|e| e.to_string())?;
            if k == 0 {
                break;
            }
            for &b in &buf[..k] {
                match b {
                    b'"' => in_quotes = !in_quotes,
                    b'\n' if !in_quotes => lines += 1,
                    _ => {}
                }
            }
            w.write_all(&buf[..k]).map_err(|e| e.to_string())?;
            if lines - last_report >= 10_000 {
                last_report = lines;
                if stop.load(Ordering::Relaxed) {
                    return Err("Esportazione annullata".into());
                }
                progress(lines);
            }
        }
        return Ok(lines.saturating_sub(opts.header as u64));
    }
    let rs = match pg_execute(c, sql)? {
        StmtResult::Rows(rs) => rs,
        StmtResult::Affected { .. } => return Err("Lo statement non restituisce righe".into()),
    };
    if opts.header {
        write_csv_row(w, rs.columns.iter().map(|c| Some(c.name.as_str())), opts)?;
    }
    for r in &rs.rows {
        write_csv_row(w, r.iter().map(|v| v.as_deref()), opts)?;
    }
    Ok(rs.rows.len() as u64)
}

fn pg_analyze(c: &mut postgres::Client, schema: &str, table: &str) -> DbResult<TableAnalysis> {
    let mut a = TableAnalysis::default();
    let rel = "(quote_ident($1) || '.' || quote_ident($2))::regclass";
    let info = c
        .query_one(
            &format!(
                "SELECT CASE c.relkind WHEN 'r' THEN 'Tabella' WHEN 'v' THEN 'Vista' \
                   WHEN 'm' THEN 'Vista materializzata' WHEN 'p' THEN 'Tabella partizionata' \
                   WHEN 'f' THEN 'Tabella esterna' ELSE c.relkind::text END, \
                 pg_get_userbyid(c.relowner)::text, c.reltuples::bigint::text, \
                 pg_size_pretty(pg_table_size(c.oid)), pg_size_pretty(pg_indexes_size(c.oid)), \
                 COALESCE(t.spcname::text, 'default'), COALESCE(obj_description(c.oid, 'pg_class'), '') \
                 FROM pg_class c LEFT JOIN pg_tablespace t ON t.oid = c.reltablespace \
                 WHERE c.oid = {rel}"
            ),
            &[&schema, &table],
        )
        .map_err(fmt_pg_err)?;
    a.properties
        .push(("Tabella".into(), format!("{schema}.{table}")));
    for (i, l) in [
        "Tipo",
        "Proprietario",
        "Righe (stima)",
        "Dimensione dati",
        "Dimensione indici",
        "Tablespace",
        "Commento",
    ]
    .iter()
    .enumerate()
    {
        let v: String = info.get(i);
        if !v.is_empty() {
            a.properties.push((l.to_string(), v));
        }
    }

    let cols = c
        .query(
            &format!(
                "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), \
                   CASE WHEN a.attnotnull THEN 'NO' ELSE 'YES' END, \
                   COALESCE((SELECT string_agg(CASE k.contype WHEN 'p' THEN 'PRI' WHEN 'u' THEN 'UNI' ELSE 'FK' END, ',') \
                     FROM pg_constraint k WHERE k.conrelid = a.attrelid AND k.contype IN ('p','u','f') \
                     AND a.attnum = ANY(k.conkey)), ''), \
                   pg_get_expr(d.adbin, d.adrelid), \
                   CASE a.attidentity WHEN 'a' THEN 'GENERATED ALWAYS AS IDENTITY' \
                     WHEN 'd' THEN 'GENERATED BY DEFAULT AS IDENTITY' ELSE \
                     CASE WHEN a.attgenerated = 's' THEN 'GENERATED STORED' ELSE '' END END, \
                   COALESCE(col_description(a.attrelid, a.attnum), '') \
                 FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 WHERE a.attrelid = {rel} AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum"
            ),
            &[&schema, &table],
        )
        .map_err(fmt_pg_err)?;
    a.columns = RowSet {
        columns: [
            "Colonna", "Tipo", "Null", "Chiave", "Default", "Extra", "Commento",
        ]
        .iter()
        .map(|h| ColumnMeta {
            name: h.to_string(),
            type_name: String::new(),
        })
        .collect(),
        rows: cols
            .iter()
            .map(|r| (0..7).map(|i| r.get::<_, Option<String>>(i)).collect())
            .collect(),
    };
    let generated = cols.iter().find_map(|r| {
        let extra: String = r.get::<_, Option<String>>(5).unwrap_or_default();
        let def: String = r.get::<_, Option<String>>(4).unwrap_or_default();
        let name: String = r.get(0);
        if extra.contains("IDENTITY") {
            Some(format!("{extra} su {name}"))
        } else if def.starts_with("nextval(") {
            Some(format!("SERIAL (sequenza) su {name}"))
        } else {
            None
        }
    });

    let idx = c
        .query(
            &format!(
                "SELECT i.relname::text, ix.indisprimary, ix.indisunique, am.amname::text, \
                   array_to_string(ARRAY(SELECT pg_get_indexdef(ix.indexrelid, k + 1, true) \
                     FROM generate_subscripts(ix.indkey, 1) AS k ORDER BY k), ', ') \
                 FROM pg_index ix JOIN pg_class i ON i.oid = ix.indexrelid JOIN pg_am am ON am.oid = i.relam \
                 WHERE ix.indrelid = {rel} ORDER BY ix.indisprimary DESC, i.relname"
            ),
            &[&schema, &table],
        )
        .map_err(fmt_pg_err)?;
    a.indexes = idx
        .iter()
        .map(|r| IndexInfo {
            name: r.get(0),
            primary: r.get(1),
            unique: r.get(2),
            method: r.get::<_, String>(3).to_uppercase(),
            columns: r.get(4),
        })
        .collect();
    a.primary_key = a.indexes.iter().find(|i| i.primary).cloned();
    a.key_description = describe_key(a.primary_key.as_ref(), generated);

    let fk_cols = |side: &str, rel_col: &str| {
        format!(
            "(SELECT string_agg(a.attname::text, ', ' ORDER BY k.n) FROM unnest(c.{side}) WITH ORDINALITY k(attnum, n) \
             JOIN pg_attribute a ON a.attrelid = c.{rel_col} AND a.attnum = k.attnum)"
        )
    };
    let fk_select = format!(
        "SELECT c.conname::text, c.conrelid::regclass::text, {}, c.confrelid::regclass::text, {}, \
           CASE c.confupdtype WHEN 'a' THEN 'NO ACTION' WHEN 'r' THEN 'RESTRICT' WHEN 'c' THEN 'CASCADE' \
             WHEN 'n' THEN 'SET NULL' ELSE 'SET DEFAULT' END, \
           CASE c.confdeltype WHEN 'a' THEN 'NO ACTION' WHEN 'r' THEN 'RESTRICT' WHEN 'c' THEN 'CASCADE' \
             WHEN 'n' THEN 'SET NULL' ELSE 'SET DEFAULT' END \
         FROM pg_constraint c WHERE c.contype = 'f' AND ",
        fk_cols("conkey", "conrelid"),
        fk_cols("confkey", "confrelid")
    );
    let to_fk = |r: &postgres::Row| ForeignKeyInfo {
        name: r.get(0),
        table: r.get(1),
        columns: r.get(2),
        ref_table: r.get(3),
        ref_columns: r.get(4),
        on_update: r.get(5),
        on_delete: r.get(6),
    };
    a.foreign_keys = c
        .query(
            &format!("{fk_select} c.conrelid = {rel} ORDER BY c.conname"),
            &[&schema, &table],
        )
        .map_err(fmt_pg_err)?
        .iter()
        .map(to_fk)
        .collect();
    a.referenced_by = c
        .query(
            &format!("{fk_select} c.confrelid = {rel} ORDER BY 2, c.conname"),
            &[&schema, &table],
        )
        .map_err(fmt_pg_err)?
        .iter()
        .map(to_fk)
        .collect();

    // DDL ricostruito dal catalogo
    let qualified = format!(
        "{}.{}",
        sqlutil::quote_ident(schema, Dialect::Postgres),
        sqlutil::quote_ident(table, Dialect::Postgres)
    );
    let mut ddl = format!("CREATE TABLE {qualified} (\n");
    let defs: Vec<String> = a
        .columns
        .rows
        .iter()
        .map(|r| {
            let get = |i: usize| r[i].clone().unwrap_or_default();
            let mut d = format!(
                "    {} {}",
                sqlutil::quote_ident(&get(0), Dialect::Postgres),
                get(1)
            );
            if !get(5).is_empty() && get(5).contains("IDENTITY") {
                d.push_str(&format!(" {}", get(5)));
            } else if !get(4).is_empty() {
                d.push_str(&format!(" DEFAULT {}", get(4)));
            }
            if get(2) == "NO" {
                d.push_str(" NOT NULL");
            }
            d
        })
        .collect();
    let constraints = c
        .query(
            &format!(
                "SELECT conname::text, pg_get_constraintdef(oid) FROM pg_constraint \
                 WHERE conrelid = {rel} AND contype IN ('p','u','f','c','x') \
                 ORDER BY CASE contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'c' THEN 2 ELSE 3 END, conname"
            ),
            &[&schema, &table],
        )
        .map_err(fmt_pg_err)?;
    let mut parts = defs;
    for r in &constraints {
        parts.push(format!(
            "    CONSTRAINT {} {}",
            sqlutil::quote_ident(&r.get::<_, String>(0), Dialect::Postgres),
            r.get::<_, String>(1)
        ));
    }
    ddl.push_str(&parts.join(",\n"));
    ddl.push_str("\n);\n");
    let index_defs = c
        .query(
            &format!(
                "SELECT pg_get_indexdef(ix.indexrelid) FROM pg_index ix \
                 WHERE ix.indrelid = {rel} AND NOT EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conindid = ix.indexrelid)"
            ),
            &[&schema, &table],
        )
        .map_err(fmt_pg_err)?;
    for r in &index_defs {
        ddl.push_str(&format!("{};\n", r.get::<_, String>(0)));
    }
    a.ddl = ddl;
    Ok(a)
}

// ---------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------

pub fn write_csv_row<'a>(
    w: &mut impl Write,
    values: impl Iterator<Item = Option<&'a str>>,
    opts: &CsvOptions,
) -> DbResult<()> {
    let sep = opts.separator.as_str();
    let mut first = true;
    for v in values {
        if !first {
            w.write_all(sep.as_bytes()).map_err(|e| e.to_string())?;
        }
        first = false;
        let Some(v) = v else { continue };
        let needs_quote = opts.quote_all
            || (!sep.is_empty() && v.contains(sep))
            || v.contains(['"', '\n', '\r'])
            || v.starts_with(' ')
            || v.ends_with(' ');
        if needs_quote {
            w.write_all(b"\"").map_err(|e| e.to_string())?;
            w.write_all(v.replace('"', "\"\"").as_bytes())
                .map_err(|e| e.to_string())?;
            w.write_all(b"\"").map_err(|e| e.to_string())?;
        } else {
            w.write_all(v.as_bytes()).map_err(|e| e.to_string())?;
        }
    }
    w.write_all(b"\n").map_err(|e| e.to_string())
}

/// Scrive un `RowSet` già in memoria (es. la pagina corrente) come CSV.
pub fn rowset_to_csv(rs: &RowSet, path: &std::path::Path, opts: &CsvOptions) -> DbResult<u64> {
    let file =
        File::create(path).map_err(|e| format!("Impossibile creare {}: {e}", path.display()))?;
    let mut w = BufWriter::new(file);
    if opts.header {
        write_csv_row(
            &mut w,
            rs.columns.iter().map(|c| Some(c.name.as_str())),
            opts,
        )?;
    }
    for r in &rs.rows {
        write_csv_row(&mut w, r.iter().map(|v| v.as_deref()), opts)?;
    }
    w.flush().map_err(|e| e.to_string())?;
    Ok(rs.rows.len() as u64)
}

/// Flag condiviso per interrompere le esportazioni.
pub type StopFlag = Arc<AtomicBool>;

#[cfg(test)]
mod tests {
    use super::*;

    fn csv(values: &[Option<&str>], sep: &str, quote_all: bool) -> String {
        let mut out = Vec::new();
        let opts = CsvOptions {
            separator: sep.into(),
            header: false,
            quote_all,
        };
        write_csv_row(&mut out, values.iter().copied(), &opts).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn csv_quoting() {
        assert_eq!(
            csv(&[Some("a"), None, Some("b;c")], ";", false),
            "a;;\"b;c\"\n"
        );
        assert_eq!(
            csv(&[Some("x\"y"), Some("l1\nl2")], ";", false),
            "\"x\"\"y\";\"l1\nl2\"\n"
        );
        assert_eq!(csv(&[Some("a,b"), Some("c")], ";", false), "a,b;c\n");
        assert_eq!(csv(&[Some("a"), Some("b")], "\t", true), "\"a\"\t\"b\"\n");
    }

    #[test]
    fn hex_format() {
        assert_eq!(hex(&[0x89, 0x50, 0x0a]), "0x89500A");
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_bytes("512"), "512 B");
        assert_eq!(human_bytes("16384"), "16.0 KB");
    }
}
