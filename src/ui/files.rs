//! Query e script su file: apertura, salvataggio, file recenti e conferma
//! alla chiusura di schede con modifiche non salvate.

use super::app::App;
use super::tab::QueryTab;
use super::util::show_error;
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use lemure::config::{self, RecentFile};
use std::path::{Path, PathBuf};
use std::rc::Rc;

fn sql_filters() -> gio::ListStore {
    let sql = gtk::FileFilter::new();
    sql.set_name(Some("File SQL (*.sql)"));
    sql.add_pattern("*.sql");
    sql.add_pattern("*.SQL");
    sql.add_mime_type("application/sql");
    let all = gtk::FileFilter::new();
    all.set_name(Some("Tutti i file"));
    all.add_pattern("*");
    let store = gio::ListStore::new::<gtk::FileFilter>();
    store.append(&sql);
    store.append(&all);
    store
}

/// Etichetta per il menu dei recenti: nome del file e cartella abbreviata.
fn recent_label(path: &str) -> String {
    let p = Path::new(path);
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir = p
        .parent()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    let dir = match dirs::home_dir() {
        Some(home) => {
            let home = home.display().to_string();
            match dir.strip_prefix(&home) {
                Some(rest) => format!("~{rest}"),
                None => dir,
            }
        }
        None => dir,
    };
    // I trattini bassi nelle etichette dei menu GTK indicano la scorciatoia.
    format!("{name}   —   {dir}").replace('_', "__")
}

impl App {
    /// Ricostruisce il sottomenu "File recenti".
    pub fn rebuild_recent_menu(&self) {
        let menu = &self.recent_menu;
        menu.remove_all();
        let files = gio::Menu::new();
        for r in self.recent.borrow().iter() {
            let item = gio::MenuItem::new(Some(&recent_label(&r.path)), None);
            item.set_action_and_target_value(Some("win.open-recent"), Some(&r.path.to_variant()));
            files.append_item(&item);
        }
        if self.recent.borrow().is_empty() {
            files.append(Some("(nessun file recente)"), Some("win.none"));
        }
        menu.append_section(None, &files);
        let tools = gio::Menu::new();
        tools.append(Some("Svuota elenco"), Some("win.clear-recent"));
        menu.append_section(None, &tools);
    }

    fn remember_file(&self, path: &Path, ds_id: Option<String>) {
        let path = path.display().to_string();
        config::push_recent(&mut self.recent.borrow_mut(), &path, ds_id);
        if let Err(e) = config::save_recent_files(&self.recent.borrow()) {
            eprintln!("lemure: impossibile salvare i file recenti: {e}");
        }
        self.rebuild_recent_menu();
    }

    pub fn clear_recent(&self) {
        self.recent.borrow_mut().clear();
        let _ = config::save_recent_files(&[]);
        self.rebuild_recent_menu();
    }

    pub fn open_file_dialog(self: &Rc<Self>) {
        let app = self.clone();
        glib::spawn_future_local(async move {
            let dialog = gtk::FileDialog::builder()
                .title("Apri query o script SQL")
                .modal(true)
                .filters(&sql_filters())
                .build();
            if let Some(dir) = app.last_dir() {
                dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
            }
            let Ok(files) = dialog.open_multiple_future(Some(&app.window)).await else {
                return;
            };
            for f in files.iter::<gio::File>().flatten() {
                if let Some(p) = f.path() {
                    app.open_path(&p);
                }
            }
        });
    }

    /// Cartella dell'ultimo file usato, per i dialoghi di apertura/salvataggio.
    fn last_dir(&self) -> Option<PathBuf> {
        let from_tab = self.current_tab().and_then(|t| {
            t.path
                .borrow()
                .as_ref()
                .and_then(|p| p.parent().map(Path::to_path_buf))
        });
        from_tab.or_else(|| {
            self.recent
                .borrow()
                .first()
                .and_then(|r| Path::new(&r.path).parent().map(Path::to_path_buf))
        })
    }

    /// Apre un file in una scheda (o porta in primo piano quella che lo contiene già).
    pub fn open_path(self: &Rc<Self>, path: &Path) {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(i) = self
            .tabs
            .borrow()
            .iter()
            .position(|t| t.path.borrow().as_deref() == Some(path.as_path()))
        {
            self.query_nb.set_current_page(Some(i as u32));
            return;
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                show_error(
                    Some(self.window.upcast_ref()),
                    "Impossibile aprire il file",
                    &format!("{}\n{e}", path.display()),
                );
                // un file recente sparito viene tolto dall'elenco
                self.recent
                    .borrow_mut()
                    .retain(|r| Path::new(&r.path) != path);
                let _ = config::save_recent_files(&self.recent.borrow());
                self.rebuild_recent_menu();
                return;
            }
        };
        let text = match String::from_utf8(bytes) {
            Ok(t) => t,
            Err(e) => {
                show_error(
                    Some(self.window.upcast_ref()),
                    "Il file non è in UTF-8",
                    "I caratteri non validi sono stati sostituiti: salvandolo verrà riscritto in UTF-8.",
                );
                String::from_utf8_lossy(e.as_bytes()).into_owned()
            }
        };
        // Datasource: quello usato l'ultima volta con questo file, se esiste ancora.
        let ds = self
            .recent
            .borrow()
            .iter()
            .find(|r| Path::new(&r.path) == path)
            .and_then(|r| r.datasource_id.clone())
            .filter(|id| self.datasource(id).is_some());
        // Una scheda vuota e senza nome viene riutilizzata.
        let reuse = self
            .current_tab()
            .filter(|t| t.path.borrow().is_none() && t.editor.text().trim().is_empty());
        let tab = match reuse {
            Some(t) => {
                if ds.is_some() {
                    t.set_datasource(ds.clone());
                }
                t
            }
            None => self.new_tab(ds.clone(), "", None),
        };
        tab.editor.set_text(&text);
        tab.editor
            .buffer
            .place_cursor(&tab.editor.buffer.start_iter());
        tab.editor.buffer.set_modified(false);
        tab.set_path(Some(path.clone()));
        self.remember_file(&path, tab.ds_id.borrow().clone());
    }

    /// Salva la scheda; con `save_as` (o senza file associato) chiede il nome.
    /// Ritorna `true` se il salvataggio è riuscito.
    pub async fn save_tab(self: &Rc<Self>, tab: &Rc<QueryTab>, save_as: bool) -> bool {
        let current = tab.path.borrow().clone();
        let path = match current {
            Some(p) if !save_as => p,
            _ => {
                let dialog = gtk::FileDialog::builder()
                    .title(if save_as {
                        "Salva con nome"
                    } else {
                        "Salva query"
                    })
                    .modal(true)
                    .filters(&sql_filters())
                    .build();
                match tab.path.borrow().as_ref() {
                    Some(p) => dialog.set_initial_file(Some(&gio::File::for_path(p))),
                    None => {
                        if let Some(dir) = self.last_dir() {
                            dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
                        }
                        dialog.set_initial_name(Some(&format!("{}.sql", tab.display_name())));
                    }
                }
                let Ok(file) = dialog.save_future(Some(&self.window)).await else {
                    return false;
                };
                let Some(mut p) = file.path() else {
                    return false;
                };
                if p.extension().is_none() {
                    p.set_extension("sql");
                }
                p
            }
        };
        if let Err(e) = write_atomic(&path, tab.editor.text().as_bytes()) {
            show_error(
                Some(self.window.upcast_ref()),
                "Salvataggio non riuscito",
                &format!("{}\n{e}", path.display()),
            );
            return false;
        }
        tab.editor.buffer.set_modified(false);
        tab.set_path(Some(path.clone()));
        self.remember_file(&path, tab.ds_id.borrow().clone());
        true
    }

    pub fn save_current(self: &Rc<Self>, save_as: bool) {
        if let Some(tab) = self.current_tab() {
            let app = self.clone();
            glib::spawn_future_local(async move {
                app.save_tab(&tab, save_as).await;
            });
        }
    }

    /// Chiude la scheda chiedendo conferma se il file ha modifiche non salvate.
    pub fn request_close_tab(self: &Rc<Self>, tab: &Rc<QueryTab>) {
        if !tab.is_dirty() {
            self.close_tab(tab);
            return;
        }
        let app = self.clone();
        let tab = tab.clone();
        glib::spawn_future_local(async move {
            let dialog = gtk::AlertDialog::builder()
                .message(format!("Salvare le modifiche a «{}»?", tab.display_name()))
                .detail("Se non salvi, le modifiche fatte dall'ultimo salvataggio andranno perse.")
                .modal(true)
                .buttons(["Annulla", "Non salvare", "Salva"])
                .cancel_button(0)
                .default_button(2)
                .build();
            match dialog.choose_future(Some(&app.window)).await {
                Ok(1) => app.close_tab(&tab),
                Ok(2) if app.save_tab(&tab, false).await => {
                    app.close_tab(&tab);
                }
                _ => {}
            }
        });
    }

    /// Permette di trascinare file .sql nella finestra.
    pub fn install_file_drop(self: &Rc<Self>) {
        let target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let weak = Rc::downgrade(self);
        target.connect_drop(move |_, value, _, _| {
            let (Some(app), Ok(list)) = (weak.upgrade(), value.get::<gdk::FileList>()) else {
                return false;
            };
            for f in list.files() {
                if let Some(p) = f.path() {
                    app.open_path(&p);
                }
            }
            true
        });
        self.window.add_controller(target);
    }

    #[cfg(feature = "devtools")]
    pub fn recent_files(&self) -> Vec<RecentFile> {
        self.recent.borrow().clone()
    }
}

/// Scrittura atomica: file temporaneo nella stessa cartella + rename, così un
/// errore a metà non lascia il file troncato.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.lemure-tmp"));
    std::fs::write(&tmp, data)?;
    // mantiene i permessi del file esistente
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_content() {
        let dir = std::env::temp_dir().join(format!("lemure-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("q.sql");
        write_atomic(&p, b"select 1;").unwrap();
        write_atomic(&p, b"select 2;").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "select 2;");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn recent_labels_escape_underscores() {
        assert!(recent_label("/tmp/my_query.sql").starts_with("my__query.sql"));
    }
}
