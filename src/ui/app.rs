//! Finestra principale: stato dell'applicazione, gestione dei datasource,
//! delle connessioni e delle schede query/risultati.

use super::dialogs;
use super::sidebar::{DsStatus, Sidebar};
use super::tab::QueryTab;
use super::util::{confirm, icon_button};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use lemure::completion::{CompColumn, SchemaMeta};
use lemure::config::{self, DataSource, DbKind, RecentFile, SavedTab, Session, Settings};
use lemure::db;
use lemure::sqlutil::{self, Dialect};
use lemure::worker::{self, Worker};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

type ConnResult = Result<Rc<Worker>, String>;
pub type MetaRef = Rc<RefCell<SchemaMeta>>;

/// Oltre questo numero di tabelle le colonne si caricano solo quando servono.
const META_MAX_TABLES: usize = 3000;

pub struct App {
    pub window: gtk::ApplicationWindow,
    pub settings: RefCell<Settings>,
    pub datasources: RefCell<Vec<DataSource>>,
    /// Modello condiviso dai menu a tendina delle schede (stesso ordine di `datasources`).
    pub ds_names: gtk::StringList,
    /// Vero mentre `ds_names` viene aggiornato: le schede ignorano i cambi di selezione.
    pub refreshing_ds: Cell<bool>,
    sessions: RefCell<HashMap<String, Rc<Worker>>>,
    pending: RefCell<HashMap<String, Vec<async_channel::Sender<ConnResult>>>>,
    /// Metadati per l'autocompletamento, per datasource.
    meta: RefCell<HashMap<String, MetaRef>>,
    meta_loading: RefCell<HashSet<String>>,
    columns_loading: RefCell<HashSet<(String, String, String)>>,
    pub sidebar: Rc<Sidebar>,
    pub(super) query_nb: gtk::Notebook,
    result_nb: gtk::Notebook,
    pub(super) tabs: RefCell<Vec<Rc<QueryTab>>>,
    /// File aperti/salvati di recente e relativo sottomenu.
    pub(super) recent: RefCell<Vec<RecentFile>>,
    pub(super) recent_menu: gio::Menu,
    hpaned: gtk::Paned,
    vpaned: gtk::Paned,
    css: gtk::CssProvider,
    tab_counter: Cell<u32>,
}

impl App {
    pub fn new(application: &gtk::Application) -> Rc<App> {
        let settings = config::load_settings();
        let mut datasources = config::load_datasources();
        datasources.sort_by_cached_key(|d| d.name.to_lowercase());

        let window = gtk::ApplicationWindow::builder()
            .application(application)
            .title("Lemure")
            .default_width(settings.window_width)
            .default_height(settings.window_height)
            .build();

        let query_nb = gtk::Notebook::new();
        query_nb.set_scrollable(true);
        query_nb.add_css_class("query-notebook");
        let result_nb = gtk::Notebook::new();
        result_nb.set_scrollable(true);
        result_nb.add_css_class("result-notebook");

        let vpaned = gtk::Paned::new(gtk::Orientation::Vertical);
        vpaned.set_start_child(Some(&query_nb));
        vpaned.set_end_child(Some(&result_nb));
        vpaned.set_position(settings.editor_height);
        vpaned.set_shrink_start_child(false);
        vpaned.set_shrink_end_child(false);
        vpaned.set_wide_handle(true);

        let hpaned = gtk::Paned::new(gtk::Orientation::Horizontal);
        hpaned.set_end_child(Some(&vpaned));
        hpaned.set_position(settings.sidebar_width);
        hpaned.set_shrink_start_child(false);
        hpaned.set_resize_start_child(false);
        hpaned.set_wide_handle(true);
        window.set_child(Some(&hpaned));

        let css = gtk::CssProvider::new();
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        let app = Rc::new_cyclic(|weak| App {
            window,
            settings: RefCell::new(settings),
            datasources: RefCell::new(datasources),
            ds_names: gtk::StringList::new(&[]),
            refreshing_ds: Cell::new(false),
            sessions: RefCell::default(),
            pending: RefCell::default(),
            meta: RefCell::default(),
            meta_loading: RefCell::default(),
            columns_loading: RefCell::default(),
            sidebar: Sidebar::new(weak.clone()),
            query_nb,
            result_nb,
            tabs: RefCell::default(),
            recent: RefCell::new(config::load_recent_files()),
            recent_menu: gio::Menu::new(),
            hpaned,
            vpaned,
            css,
            tab_counter: Cell::new(0),
        });
        app.hpaned.set_start_child(Some(&app.sidebar.widget));
        app.apply_css();
        app.refresh_datasources();
        app.rebuild_recent_menu();
        app.build_header();
        app.install_actions(application);
        app.install_file_drop();
        app.connect_notebooks();
        app.restore_session();

        // Salvataggio periodico delle schede, per non perdere le query in caso di crash.
        let weak = Rc::downgrade(&app);
        glib::timeout_add_seconds_local(30, move || match weak.upgrade() {
            Some(app) => {
                app.save_session();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });

        let weak = Rc::downgrade(&app);
        app.window.connect_close_request(move |_| {
            if let Some(app) = weak.upgrade() {
                app.save_settings();
                app.save_session();
            }
            glib::Propagation::Proceed
        });
        app
    }

    pub fn present(&self) {
        self.window.present();
        if let Some(t) = self.current_tab() {
            t.editor.view.grab_focus();
        }
    }

    // -----------------------------------------------------------------------
    // Aspetto
    // -----------------------------------------------------------------------

    fn apply_css(&self) {
        let s = self.settings.borrow();
        let css = format!(
            r#"
.sql-editor {{ font-family: Monospace; font-size: {ef}pt; }}
.line-numbers {{ background-color: alpha(currentColor, 0.04); }}
.syntax-status {{ font-size: 9pt; padding: 2px 0; opacity: 0.75; }}
.syntax-status.syntax-error {{ color: #e01b24; opacity: 1; }}
columnview.data-grid {{ font-family: Monospace; font-size: {gf}pt; }}
columnview.data-grid > listview > row {{ min-height: 0; padding: 0; }}
columnview.data-grid > listview > row > cell {{ padding: 0; min-height: 0; }}
columnview.data-grid .cell, columnview.data-grid .row-number {{ padding: 2px 6px; }}
columnview.data-grid > header > button {{ padding: 3px 6px; font-family: sans-serif; font-weight: bold; font-size: {hf}pt; }}
columnview.data-grid .null-value {{ opacity: 0.45; font-style: italic; }}
columnview.data-grid .row-number {{ opacity: 0.5; }}
columnview.data-grid .current-cell {{ background-color: alpha(#3584e4, 0.22); box-shadow: inset 0 0 0 1px alpha(#3584e4, 0.9); border-radius: 2px; }}
.schema-tree {{ font-size: {tf}pt; }}
.schema-tree > row {{ min-height: 0; padding: 3px 4px; margin: 0 4px; }}
.schema-tree .node-detail {{ opacity: 0.55; font-family: Monospace; font-size: {df}pt; }}
.schema-tree .ds-name {{ font-weight: bold; }}
.ds-connected {{ color: #26a269; }}
.ds-error, .error-text {{ color: #e01b24; }}
.success-text {{ color: #26a269; }}
.pk-column {{ color: #c88800; }}
.message-log {{ font-size: {gf}pt; }}
.query-notebook > header tab label.running {{ font-style: italic; }}
.key-description {{ font-weight: bold; }}
.completion-popup > contents {{ padding: 2px; }}
.completion-list {{ font-family: Monospace; font-size: {cf}pt; background: none; }}
.completion-list > row {{ padding: 2px 6px; min-height: 0; border-radius: 4px; }}
.completion-list .comp-detail {{ opacity: 0.55; font-size: {df}pt; }}
.comp-badge {{ font-weight: bold; font-size: {df}pt; border-radius: 3px; padding: 0 2px; color: white; }}
.comp-column {{ background-color: #3584e4; }}
.comp-table {{ background-color: #2ec27e; }}
.comp-view {{ background-color: #0e8a8a; }}
.comp-schema {{ background-color: #e66100; }}
.comp-keyword {{ background-color: #9141ac; }}
"#,
            ef = s.editor_font_size,
            gf = s.grid_font_size,
            hf = (s.grid_font_size - 0.5).max(7.0),
            tf = (s.grid_font_size + 0.5).max(8.0),
            df = (s.grid_font_size - 1.0).max(7.0),
            cf = (s.editor_font_size - 1.0).max(7.0),
        );
        self.css.load_from_string(&css);
    }

    fn zoom(&self, delta: f64) {
        {
            let mut s = self.settings.borrow_mut();
            if delta == 0.0 {
                let d = Settings::default();
                s.editor_font_size = d.editor_font_size;
                s.grid_font_size = d.grid_font_size;
            } else {
                s.editor_font_size = (s.editor_font_size + delta).clamp(7.0, 28.0);
                s.grid_font_size = (s.grid_font_size + delta).clamp(7.0, 28.0);
            }
        }
        self.apply_css();
        let (ef, gf) = {
            let s = self.settings.borrow();
            (s.editor_font_size, s.grid_font_size)
        };
        for t in self.tabs.borrow().iter() {
            t.editor.set_font_size(ef);
            t.results.grid.set_font_size(gf);
        }
    }

    fn build_header(self: &Rc<Self>) {
        let header = gtk::HeaderBar::new();
        let new_ds = icon_button("list-add-symbolic", "Nuovo datasource");
        new_ds.set_action_name(Some("win.new-datasource"));
        let new_tab = icon_button("tab-new-symbolic", "Nuova scheda query (Ctrl+T)");
        new_tab.set_action_name(Some("win.new-tab"));
        header.pack_start(&new_ds);
        header.pack_start(&new_tab);
        let open = icon_button("document-open-symbolic", "Apri query o script (Ctrl+O)");
        open.set_action_name(Some("win.open"));
        let recent = gtk::MenuButton::new();
        recent.set_icon_name("document-open-recent-symbolic");
        recent.set_tooltip_text(Some("File recenti"));
        recent.set_has_frame(false);
        recent.set_menu_model(Some(&self.recent_menu));
        let save = icon_button("document-save-symbolic", "Salva (Ctrl+S)");
        save.set_action_name(Some("win.save"));
        header.pack_start(&open);
        header.pack_start(&recent);
        header.pack_start(&save);

        let menu = gio::Menu::new();
        let file = gio::Menu::new();
        file.append(Some("Apri…"), Some("win.open"));
        file.append_submenu(Some("File recenti"), &self.recent_menu);
        file.append(Some("Salva"), Some("win.save"));
        file.append(Some("Salva con nome…"), Some("win.save-as"));
        menu.append_section(None, &file);
        let zoom = gio::Menu::new();
        zoom.append(Some("Ingrandisci caratteri"), Some("win.zoom-in"));
        zoom.append(Some("Riduci caratteri"), Some("win.zoom-out"));
        zoom.append(Some("Dimensione predefinita"), Some("win.zoom-reset"));
        menu.append_section(None, &zoom);
        let other = gio::Menu::new();
        other.append(Some("Rinomina scheda…"), Some("win.rename-tab"));
        other.append(Some("Scorciatoie da tastiera"), Some("win.shortcuts"));
        other.append(Some("Informazioni su Lemure"), Some("win.about"));
        menu.append_section(None, &other);
        let menu_btn = gtk::MenuButton::new();
        menu_btn.set_icon_name("open-menu-symbolic");
        menu_btn.set_menu_model(Some(&menu));
        header.pack_end(&menu_btn);
        self.window.set_titlebar(Some(&header));
    }

    fn install_actions(self: &Rc<Self>, application: &gtk::Application) {
        let add = |name: &str, accels: &[&str], f: fn(&Rc<App>)| {
            let a = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            a.connect_activate(move |_, _| {
                if let Some(app) = weak.upgrade() {
                    f(&app);
                }
            });
            self.window.add_action(&a);
            if !accels.is_empty() {
                application.set_accels_for_action(&format!("win.{name}"), accels);
            }
        };
        add("run", &["<Control>Return"], |app| {
            if let Some(t) = app.current_tab() {
                t.run(false);
            }
        });
        add("run-script", &["<Control><Shift>Return", "<Alt>x"], |app| {
            if let Some(t) = app.current_tab() {
                t.run(true);
            }
        });
        add("stop", &[], |app| {
            if let Some(t) = app.current_tab() {
                t.stop();
            }
        });
        add("new-tab", &["<Control>t"], |app| {
            let ds = app.default_ds_for_new_tab();
            app.new_tab(ds, "", None);
        });
        add("close-tab", &["<Control>w"], |app| {
            if let Some(t) = app.current_tab() {
                app.request_close_tab(&t);
            }
        });
        add("open", &["<Control>o"], |app| app.open_file_dialog());
        add("save", &["<Control>s"], |app| app.save_current(false));
        add("save-as", &["<Control><Shift>s"], |app| {
            app.save_current(true)
        });
        add("clear-recent", &[], |app| app.clear_recent());
        let open_recent = gio::SimpleAction::new("open-recent", Some(glib::VariantTy::STRING));
        let weak = Rc::downgrade(self);
        open_recent.connect_activate(move |_, p| {
            if let (Some(app), Some(path)) = (weak.upgrade(), p.and_then(|v| v.get::<String>())) {
                app.open_path(std::path::Path::new(&path));
            }
        });
        self.window.add_action(&open_recent);
        add("new-datasource", &[], |app| app.edit_datasource(None));
        add(
            "zoom-in",
            &["<Control>plus", "<Control>equal", "<Control>KP_Add"],
            |app| app.zoom(1.0),
        );
        add(
            "zoom-out",
            &["<Control>minus", "<Control>KP_Subtract"],
            |app| app.zoom(-1.0),
        );
        add("zoom-reset", &["<Control>0"], |app| app.zoom(0.0));
        add("rename-tab", &["F2"], |app| app.rename_current_tab());
        add("shortcuts", &["<Control>question"], |app| {
            app.show_shortcuts()
        });
        add("about", &[], |app| {
            let about = gtk::AboutDialog::builder()
                .program_name("Lemure")
                .version(env!("CARGO_PKG_VERSION"))
                .comments("Client SQL leggero per MySQL e PostgreSQL")
                .license_type(gtk::License::MitX11)
                .logo_icon_name("it.lemure.Lemure")
                .transient_for(&app.window)
                .modal(true)
                .build();
            about.present();
        });
    }

    fn show_shortcuts(&self) {
        let rows = [
            (
                "Ctrl+Invio",
                "Esegui lo statement sotto il cursore (o la selezione)",
            ),
            ("Ctrl+Shift+Invio, Alt+X", "Esegui l'intero script"),
            ("Ctrl+T / Ctrl+W", "Nuova scheda / chiudi scheda"),
            ("Ctrl+O / Ctrl+S", "Apri file / salva"),
            ("Ctrl+Shift+S", "Salva con nome"),
            ("F2", "Rinomina la scheda (o salva con nome se è un file)"),
            ("Ctrl+Spazio", "Autocompletamento di tabelle e colonne"),
            ("Ctrl+/", "Commenta/decommenta le righe"),
            ("Ctrl+C (griglia)", "Copia il valore della cella"),
            (
                "Ctrl+Shift+C (griglia)",
                "Copia le righe selezionate separate da |",
            ),
            ("Doppio clic su cella", "Mostra il valore completo"),
            ("Alt+← / Alt+→", "Pagina precedente / successiva"),
            ("Ctrl++ / Ctrl+- / Ctrl+0", "Dimensione caratteri"),
        ];
        let grid = gtk::Grid::builder()
            .row_spacing(6)
            .column_spacing(24)
            .build();
        {
            let m = 18;
            grid.set_margin_top(m);
            grid.set_margin_bottom(m);
            grid.set_margin_start(m);
            grid.set_margin_end(m);
        }
        for (i, (k, d)) in rows.iter().enumerate() {
            let kl = gtk::Label::new(Some(k));
            kl.set_xalign(1.0);
            kl.add_css_class("monospace");
            let dl = gtk::Label::new(Some(d));
            dl.set_xalign(0.0);
            grid.attach(&kl, 0, i as i32, 1, 1);
            grid.attach(&dl, 1, i as i32, 1, 1);
        }
        let w = gtk::Window::builder()
            .title("Scorciatoie da tastiera")
            .transient_for(&self.window)
            .modal(true)
            .child(&grid)
            .build();
        w.present();
    }

    fn rename_current_tab(self: &Rc<Self>) {
        let Some(tab) = self.current_tab() else {
            return;
        };
        // Il nome di una scheda legata a un file è il nome del file.
        if tab.path.borrow().is_some() {
            self.save_current(true);
            return;
        }
        let entry = gtk::Entry::builder()
            .text(&*tab.title.borrow())
            .activates_default(true)
            .build();
        let win = gtk::Window::builder()
            .title("Rinomina scheda")
            .transient_for(&self.window)
            .modal(true)
            .default_width(320)
            .build();
        let ok = gtk::Button::with_label("Rinomina");
        ok.add_css_class("suggested-action");
        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 12);
        vbox.set_margin_top(16);
        vbox.set_margin_bottom(16);
        vbox.set_margin_start(16);
        vbox.set_margin_end(16);
        vbox.append(&entry);
        ok.set_halign(gtk::Align::End);
        vbox.append(&ok);
        win.set_child(Some(&vbox));
        win.set_default_widget(Some(&ok));
        let w = win.clone();
        ok.connect_clicked(move |_| {
            let t = entry.text();
            if !t.trim().is_empty() {
                tab.set_title(t.trim());
            }
            w.close();
        });
        win.present();
    }

    // -----------------------------------------------------------------------
    // Datasource e connessioni
    // -----------------------------------------------------------------------

    pub fn datasource(&self, id: &str) -> Option<DataSource> {
        self.datasources
            .borrow()
            .iter()
            .find(|d| d.id == id)
            .cloned()
    }

    pub fn ds_kind(&self, id: &str) -> Option<DbKind> {
        self.datasources
            .borrow()
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.kind)
    }

    fn refresh_datasources(&self) {
        let names: Vec<String> = self
            .datasources
            .borrow()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        self.refreshing_ds.set(true);
        self.ds_names.splice(0, self.ds_names.n_items(), &refs);
        self.refreshing_ds.set(false);
        self.sidebar.set_datasources(&self.datasources.borrow());
        for t in self.tabs.borrow().iter() {
            t.sync_dropdown();
        }
    }

    pub fn edit_datasource(self: &Rc<Self>, ds: Option<DataSource>) {
        dialogs::datasource_dialog(self, ds);
    }

    pub fn save_datasource(self: &Rc<Self>, ds: DataSource, is_new: bool) {
        let mut needs_disconnect = false;
        {
            let mut list = self.datasources.borrow_mut();
            match list.iter_mut().find(|d| d.id == ds.id) {
                Some(existing) => {
                    needs_disconnect = existing.host != ds.host
                        || existing.port != ds.port
                        || existing.user != ds.user
                        || existing.password != ds.password
                        || existing.database != ds.database
                        || existing.kind != ds.kind;
                    *existing = ds.clone();
                }
                None => list.push(ds.clone()),
            }
            list.sort_by_cached_key(|d| d.name.to_lowercase());
        }
        if needs_disconnect {
            self.disconnect(&ds.id);
        }
        if let Err(e) = config::save_datasources(&self.datasources.borrow()) {
            super::util::show_error(Some(self.window.upcast_ref()), "Salvataggio fallito", &e);
        }
        self.refresh_datasources();
        for t in self.tabs.borrow().iter() {
            if t.ds_id.borrow().as_deref() == Some(ds.id.as_str()) {
                t.editor.set_dialect(db::dialect_of(ds.kind));
            }
        }
        // Un nuovo datasource diventa quello delle schede che non ne hanno uno.
        if is_new {
            for t in self.tabs.borrow().iter() {
                if t.ds_id.borrow().is_none() {
                    t.set_datasource(Some(ds.id.clone()));
                }
            }
        }
    }

    pub fn delete_datasource(self: &Rc<Self>, ds: DataSource) {
        let app = self.clone();
        glib::spawn_future_local(async move {
            let ok = confirm(
                Some(app.window.upcast_ref()),
                &format!("Eliminare il datasource «{}»?", ds.name),
                "La configurazione verrà rimossa definitivamente.",
                "Elimina",
            )
            .await;
            if !ok {
                return;
            }
            app.disconnect(&ds.id);
            app.datasources.borrow_mut().retain(|d| d.id != ds.id);
            let _ = config::save_datasources(&app.datasources.borrow());
            for t in app.tabs.borrow().iter() {
                if t.ds_id.borrow().as_deref() == Some(ds.id.as_str()) {
                    t.set_datasource(None);
                }
            }
            app.refresh_datasources();
        });
    }

    /// Restituisce la connessione principale del datasource, aprendola se necessario.
    /// Richieste concorrenti per lo stesso datasource condividono un unico tentativo.
    pub async fn ensure_connected(self: &Rc<Self>, ds_id: &str) -> ConnResult {
        if let Some(w) = self.sessions.borrow().get(ds_id) {
            return Ok(w.clone());
        }
        let ds = self.datasource(ds_id).ok_or("Datasource non trovato")?;
        let waiting = {
            let mut p = self.pending.borrow_mut();
            match p.get_mut(ds_id) {
                Some(list) => {
                    let (tx, rx) = async_channel::bounded(1);
                    list.push(tx);
                    Some(rx)
                }
                None => {
                    p.insert(ds_id.to_string(), Vec::new());
                    None
                }
            }
        };
        if let Some(rx) = waiting {
            return rx
                .recv()
                .await
                .unwrap_or_else(|_| Err("Connessione annullata".into()));
        }
        self.sidebar.set_status(ds_id, DsStatus::Connecting);
        let res = Worker::connect(ds).await.map(Rc::new);
        let waiters = self.pending.borrow_mut().remove(ds_id).unwrap_or_default();
        match &res {
            Ok(w) => {
                self.sessions
                    .borrow_mut()
                    .insert(ds_id.to_string(), w.clone());
                self.sidebar
                    .set_status(ds_id, DsStatus::Connected(w.server_version.clone()));
                self.load_meta(ds_id);
            }
            Err(e) => self.sidebar.set_status(ds_id, DsStatus::Error(e.clone())),
        }
        for tx in waiters {
            let _ = tx.send(res.clone()).await;
        }
        res
    }

    pub fn disconnect(&self, ds_id: &str) {
        self.sessions.borrow_mut().remove(ds_id);
        self.meta.borrow_mut().remove(ds_id);
        for t in self.tabs.borrow().iter() {
            t.drop_connection(ds_id);
        }
        self.sidebar.reset(ds_id);
    }

    /// Dopo un DDL ricarica l'albero del datasource (se già caricato).
    pub fn schema_changed(self: &Rc<Self>, ds_id: &str) {
        self.sidebar.refresh_loaded(ds_id);
        self.load_meta(ds_id);
    }

    // -----------------------------------------------------------------------
    // Metadati per l'autocompletamento
    // -----------------------------------------------------------------------

    /// Metadati del datasource. Se mancano, avvia in background connessione e
    /// caricamento (saranno pronti per i tasti successivi).
    pub fn schema_meta(self: &Rc<Self>, ds_id: &str) -> Option<MetaRef> {
        if let Some(m) = self.meta.borrow().get(ds_id) {
            return Some(m.clone());
        }
        if !self.meta_loading.borrow().contains(ds_id) {
            if self.sessions.borrow().contains_key(ds_id) {
                self.load_meta(ds_id);
            } else {
                let app = self.clone();
                let id = ds_id.to_string();
                // ensure_connected carica i metadati appena la connessione è pronta
                glib::spawn_future_local(async move {
                    let _ = app.ensure_connected(&id).await;
                });
            }
        }
        None
    }

    /// (Ri)carica i metadati su una connessione separata, per non rallentare l'albero.
    pub fn load_meta(self: &Rc<Self>, ds_id: &str) {
        let Some(ds) = self.datasource(ds_id) else {
            return;
        };
        if !self.meta_loading.borrow_mut().insert(ds_id.to_string()) {
            return;
        }
        let app = self.clone();
        let id = ds_id.to_string();
        glib::spawn_future_local(async move {
            #[cfg(feature = "devtools")]
            let max = std::env::var("LEMURE_META_MAX_TABLES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(META_MAX_TABLES);
            #[cfg(not(feature = "devtools"))]
            let max = META_MAX_TABLES;
            let res = worker::run_detached(ds, move |c| c.load_schema_meta(max)).await;
            app.meta_loading.borrow_mut().remove(&id);
            match res {
                // scartato se nel frattempo il datasource è stato disconnesso
                Ok(m) if app.sessions.borrow().contains_key(&id) => {
                    app.meta.borrow_mut().insert(id, Rc::new(RefCell::new(m)));
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("lemure: metadati per l'autocompletamento non disponibili: {e}")
                }
            }
        });
    }

    /// Carica su richiesta le colonne di alcune tabelle (schemi molto grandi).
    pub fn load_columns(
        self: &Rc<Self>,
        ds_id: &str,
        tables: Vec<(String, String)>,
        done: Box<dyn FnOnce()>,
    ) {
        let Some(meta) = self.meta.borrow().get(ds_id).cloned() else {
            return;
        };
        let todo: Vec<(String, String)> = tables
            .into_iter()
            .filter(|(s, t)| {
                self.columns_loading
                    .borrow_mut()
                    .insert((ds_id.to_string(), s.clone(), t.clone()))
            })
            .collect();
        if todo.is_empty() {
            return;
        }
        let app = self.clone();
        let id = ds_id.to_string();
        glib::spawn_future_local(async move {
            let Ok(w) = app.ensure_connected(&id).await else {
                return;
            };
            let req = todo.clone();
            let res = w
                .run(move |c| {
                    req.iter()
                        .map(|(s, t)| c.list_columns(s, t).unwrap_or_default())
                        .collect::<Vec<_>>()
                })
                .await;
            if let Ok(lists) = res {
                let mut m = meta.borrow_mut();
                for ((s, t), cols) in todo.iter().zip(lists) {
                    let cols = cols
                        .into_iter()
                        .map(|c| CompColumn {
                            name: c.name,
                            type_name: c.data_type,
                        })
                        .collect();
                    m.set_columns(s, t, cols);
                }
            }
            for (s, t) in &todo {
                app.columns_loading
                    .borrow_mut()
                    .remove(&(id.clone(), s.clone(), t.clone()));
            }
            done();
        });
    }

    pub fn select_sql(&self, ds_id: &str, schema: &str, table: &str, count: bool) -> String {
        let dialect = self
            .ds_kind(ds_id)
            .map(db::dialect_of)
            .unwrap_or(Dialect::MySql);
        let ds_db = self
            .datasource(ds_id)
            .map(|d| d.database)
            .unwrap_or_default();
        // Lo schema si omette quando è quello di default della connessione.
        let qualify = match dialect {
            Dialect::MySql => !schema.is_empty() && schema != ds_db,
            Dialect::Postgres => !schema.is_empty() && schema != "public",
        };
        let name = if qualify {
            format!(
                "{}.{}",
                sqlutil::quote_ident(schema, dialect),
                sqlutil::quote_ident(table, dialect)
            )
        } else {
            sqlutil::quote_ident(table, dialect)
        };
        if count {
            format!("SELECT COUNT(*) FROM {name};")
        } else {
            format!("SELECT * FROM {name};")
        }
    }

    // -----------------------------------------------------------------------
    // Schede
    // -----------------------------------------------------------------------

    fn connect_notebooks(self: &Rc<Self>) {
        // Durante "switch-page" la pagina corrente non è ancora aggiornata: una
        // guardia evita che i due notebook si rimbalzino il cambio all'infinito.
        let syncing = Rc::new(Cell::new(false));
        let link = |from: &gtk::Notebook, to: &gtk::Notebook| {
            let to = to.clone();
            let syncing = syncing.clone();
            from.connect_switch_page(move |_, _, n| {
                if !syncing.get() && to.current_page() != Some(n) {
                    syncing.set(true);
                    to.set_current_page(Some(n));
                    syncing.set(false);
                }
            });
        };
        link(&self.query_nb, &self.result_nb);
        link(&self.result_nb, &self.query_nb);
    }

    fn default_ds_for_new_tab(&self) -> Option<String> {
        self.current_tab()
            .and_then(|t| t.ds_id.borrow().clone())
            .or_else(|| self.sessions.borrow().keys().next().cloned())
            .or_else(|| self.datasources.borrow().first().map(|d| d.id.clone()))
    }

    #[cfg(feature = "devtools")]
    pub fn dev_tabs(&self) -> Vec<Rc<QueryTab>> {
        self.tabs.borrow().clone()
    }

    pub fn current_tab(&self) -> Option<Rc<QueryTab>> {
        let n = self.query_nb.current_page()? as usize;
        self.tabs.borrow().get(n).cloned()
    }

    pub fn new_tab(
        self: &Rc<Self>,
        ds_id: Option<String>,
        sql: &str,
        title: Option<String>,
    ) -> Rc<QueryTab> {
        let n = self.tab_counter.get() + 1;
        self.tab_counter.set(n);
        let title = title.unwrap_or_else(|| format!("Query {n}"));
        let ds_id = ds_id.or_else(|| self.default_ds_for_new_tab());
        let tab = QueryTab::new(self, &title, ds_id, sql);

        let label_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        label_box.append(&tab.tab_label);
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("small-button");
        close.set_tooltip_text(Some("Chiudi scheda (Ctrl+W)"));
        label_box.append(&close);
        let weak_app = Rc::downgrade(self);
        let weak_tab = Rc::downgrade(&tab);
        close.connect_clicked(move |_| {
            if let (Some(app), Some(tab)) = (weak_app.upgrade(), weak_tab.upgrade()) {
                app.request_close_tab(&tab);
            }
        });
        // Doppio clic sull'etichetta per rinominare la scheda
        let dbl = gtk::GestureClick::new();
        let weak_app = Rc::downgrade(self);
        let weak_tab = Rc::downgrade(&tab);
        dbl.connect_pressed(move |_, n, _, _| {
            if n == 2
                && let (Some(app), Some(tab)) = (weak_app.upgrade(), weak_tab.upgrade())
            {
                if let Some(i) = app.tabs.borrow().iter().position(|t| Rc::ptr_eq(t, &tab)) {
                    app.query_nb.set_current_page(Some(i as u32));
                }
                app.rename_current_tab();
            }
        });
        tab.tab_label.add_controller(dbl);

        self.tabs.borrow_mut().push(tab.clone());
        let idx = self.query_nb.append_page(&tab.page, Some(&label_box));
        self.result_nb
            .append_page(&tab.results.widget, Some(&tab.result_label));
        self.query_nb.set_current_page(Some(idx));
        self.result_nb.set_current_page(Some(idx));
        tab.editor.view.grab_focus();
        tab
    }

    pub fn close_tab(self: &Rc<Self>, tab: &Rc<QueryTab>) {
        let Some(idx) = self.tabs.borrow().iter().position(|t| Rc::ptr_eq(t, tab)) else {
            return;
        };
        tab.stop();
        self.tabs.borrow_mut().remove(idx);
        self.query_nb.remove_page(Some(idx as u32));
        self.result_nb.remove_page(Some(idx as u32));
        if self.tabs.borrow().is_empty() {
            let ds = self.default_ds_for_new_tab();
            self.new_tab(ds, "", None);
        }
    }

    pub fn insert_in_editor(&self, text: &str) {
        if let Some(t) = self.current_tab() {
            let dialect = t.editor.dialect();
            let quoted = sqlutil::quote_ident(text, dialect);
            // gli identificatori semplici non vengono quotati
            let simple = text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !text.chars().next().is_some_and(|c| c.is_ascii_digit());
            t.editor
                .insert_at_cursor(if simple && dialect == Dialect::MySql {
                    text
                } else {
                    &quoted
                });
        }
    }

    // -----------------------------------------------------------------------
    // Persistenza
    // -----------------------------------------------------------------------

    pub fn save_settings(&self) {
        let mut s = self.settings.borrow_mut();
        let (w, h) = (self.window.width(), self.window.height());
        if w > 0 && h > 0 && !self.window.is_maximized() {
            s.window_width = w;
            s.window_height = h;
        }
        s.sidebar_width = self.hpaned.position();
        s.editor_height = self.vpaned.position();
        if let Some(t) = self.current_tab() {
            s.auto_scroll_pages = t.results.auto_scroll.is_active();
        }
        if let Err(e) = config::save_settings(&s) {
            eprintln!("lemure: impossibile salvare le impostazioni: {e}");
        }
    }

    fn save_session(&self) {
        let tabs: Vec<SavedTab> = self
            .tabs
            .borrow()
            .iter()
            .map(|t| SavedTab {
                title: t.title.borrow().clone(),
                datasource_id: t.ds_id.borrow().clone(),
                sql: t.editor.text(),
                path: t.path.borrow().as_ref().map(|p| p.display().to_string()),
                dirty: t.is_dirty(),
            })
            .collect();
        let session = Session {
            tabs,
            current: self.query_nb.current_page().unwrap_or(0) as usize,
        };
        if let Err(e) = config::save_session(&session) {
            eprintln!("lemure: impossibile salvare la sessione: {e}");
        }
    }

    fn restore_session(self: &Rc<Self>) {
        let session = config::load_session();
        for t in &session.tabs {
            let ds = t
                .datasource_id
                .clone()
                .filter(|id| self.datasource(id).is_some());
            let tab = self.new_tab(ds, &t.sql, Some(t.title.clone()));
            if let Some(path) = t.path.as_deref().map(std::path::PathBuf::from) {
                // Un file senza modifiche pendenti si rilegge dal disco (può essere
                // cambiato fuori da Lemure); altrimenti si tiene il testo della sessione.
                let mut dirty = t.dirty;
                if !dirty {
                    match std::fs::read_to_string(&path) {
                        Ok(text) => tab.editor.set_text(&text),
                        Err(_) => dirty = true,
                    }
                }
                tab.editor
                    .buffer
                    .place_cursor(&tab.editor.buffer.start_iter());
                tab.editor.buffer.set_modified(dirty);
                tab.set_path(Some(path));
            }
        }
        // la numerazione riparte dopo le schede "Query N" ripristinate
        let max_n = session
            .tabs
            .iter()
            .filter_map(|t| {
                t.title
                    .strip_prefix("Query ")
                    .and_then(|n| n.parse::<u32>().ok())
            })
            .max()
            .unwrap_or(0);
        self.tab_counter.set(self.tab_counter.get().max(max_n));
        if self.tabs.borrow().is_empty() {
            self.tab_counter.set(0);
            let ds = self.default_ds_for_new_tab();
            self.new_tab(ds, "", None);
        } else {
            let n = session.current.min(self.tabs.borrow().len() - 1) as u32;
            self.query_nb.set_current_page(Some(n));
        }
    }
}
