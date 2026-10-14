//! Persistenza di datasource, impostazioni e sessione (tab aperte).
//!
//! Tutto viene salvato in `~/.config/lemure/` con permessi 0600, perché i
//! datasource contengono le password in chiaro (uso solo in sviluppo).

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DbKind {
    MySql,
    Postgres,
}

impl DbKind {
    pub fn label(self) -> &'static str {
        match self {
            DbKind::MySql => "MySQL",
            DbKind::Postgres => "PostgreSQL",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            DbKind::MySql => 3306,
            DbKind::Postgres => 5432,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DataSource {
    pub id: String,
    pub name: String,
    pub kind: DbKind,
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub database: String,
}

impl DataSource {
    pub fn new_id() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("ds-{nanos:x}")
    }

    /// Descrizione breve per tooltip e barra di stato.
    pub fn summary(&self) -> String {
        let db = if self.database.is_empty() {
            String::new()
        } else {
            format!("/{}", self.database)
        };
        format!(
            "{} {}@{}:{}{}",
            self.kind.label(),
            self.user,
            self.host,
            self.port,
            db
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub editor_font_size: f64,
    pub grid_font_size: f64,
    pub csv_separator: String,
    pub csv_header: bool,
    pub auto_scroll_pages: bool,
    pub window_width: i32,
    pub window_height: i32,
    pub sidebar_width: i32,
    pub editor_height: i32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            editor_font_size: 11.0,
            grid_font_size: 10.0,
            csv_separator: ";".into(),
            csv_header: true,
            auto_scroll_pages: true,
            window_width: 1400,
            window_height: 900,
            sidebar_width: 300,
            editor_height: 340,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SavedTab {
    pub title: String,
    pub datasource_id: Option<String>,
    pub sql: String,
    /// File associato alla scheda, se è stata aperta o salvata su disco.
    #[serde(default)]
    pub path: Option<String>,
    /// Modifiche non ancora salvate nel file (il testo è comunque in `sql`).
    #[serde(default)]
    pub dirty: bool,
}

/// File SQL aperto o salvato di recente, con il datasource usato l'ultima volta.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecentFile {
    pub path: String,
    #[serde(default)]
    pub datasource_id: Option<String>,
}

pub const MAX_RECENT_FILES: usize = 15;

/// Porta `path` in cima all'elenco dei recenti (senza duplicati, al massimo `MAX_RECENT_FILES`).
pub fn push_recent(list: &mut Vec<RecentFile>, path: &str, datasource_id: Option<String>) {
    list.retain(|r| r.path != path);
    list.insert(
        0,
        RecentFile {
            path: path.to_string(),
            datasource_id,
        },
    );
    list.truncate(MAX_RECENT_FILES);
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub tabs: Vec<SavedTab>,
    pub current: usize,
}

fn config_dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("lemure")
}

fn load_json<T: for<'de> Deserialize<'de> + Default>(file: &str) -> T {
    let path = config_dir().join(file);
    match fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            eprintln!("lemure: {} non valido: {e}", path.display());
            T::default()
        }),
        Err(_) => T::default(),
    }
}

fn save_json<T: Serialize>(file: &str, value: &T) -> Result<(), String> {
    let dir = config_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    // Scrittura atomica: file temporaneo + rename.
    let tmp = dir.join(format!(".{file}.tmp"));
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| e.to_string())?;
        f.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, dir.join(file)).map_err(|e| e.to_string())
}

pub fn load_datasources() -> Vec<DataSource> {
    load_json("datasources.json")
}

pub fn save_datasources(list: &[DataSource]) -> Result<(), String> {
    save_json("datasources.json", &list)
}

pub fn load_settings() -> Settings {
    load_json("settings.json")
}

pub fn save_settings(s: &Settings) -> Result<(), String> {
    save_json("settings.json", s)
}

pub fn load_recent_files() -> Vec<RecentFile> {
    load_json("recent.json")
}

pub fn save_recent_files(list: &[RecentFile]) -> Result<(), String> {
    save_json("recent.json", &list)
}

pub fn load_session() -> Session {
    load_json("session.json")
}

pub fn save_session(s: &Session) -> Result<(), String> {
    save_json("session.json", s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_files_are_deduplicated_and_capped() {
        let mut list = Vec::new();
        for i in 0..20 {
            push_recent(&mut list, &format!("/q/{i}.sql"), None);
        }
        assert_eq!(list.len(), MAX_RECENT_FILES);
        assert_eq!(list[0].path, "/q/19.sql");
        push_recent(&mut list, "/q/10.sql", Some("ds".into()));
        assert_eq!(list[0].path, "/q/10.sql");
        assert_eq!(list[0].datasource_id.as_deref(), Some("ds"));
        assert_eq!(list.iter().filter(|r| r.path == "/q/10.sql").count(), 1);
    }

    #[test]
    fn old_session_files_still_load() {
        let s: Session = serde_json::from_str(
            r#"{"tabs":[{"title":"Q","datasource_id":null,"sql":"x"}],"current":0}"#,
        )
        .unwrap();
        assert_eq!(s.tabs[0].path, None);
        assert!(!s.tabs[0].dirty);
    }
}
