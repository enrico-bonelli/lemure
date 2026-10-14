//! Scheda query: editor + risultati collegati. Gestisce l'esecuzione, la
//! paginazione automatica (500 righe per pagina) e l'esportazione.

use super::app::App;
use super::editor::QueryEditor;
use super::results::{PAGE_SIZE, PageNav, ResultsView};
use super::util::{fmt_count, fmt_duration, labeled_button, show_error};
use gtk::glib;
use gtk::prelude::*;
use lemure::config::DataSource;
use lemure::db::{self, RowSet, StmtResult};
use lemure::sqlutil::{self, Dialect};
use lemure::worker::{self, Worker};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Origine delle pagine del risultato corrente.
enum PageSource {
    /// La query viene rieseguita con `LIMIT/OFFSET` per ogni pagina.
    Server { base: String },
    /// Il risultato è già tutto in memoria (statement non paginabili).
    Client { all: Rc<RowSet> },
}

struct ResultState {
    sql: String,
    ds: DataSource,
    source: PageSource,
    page: usize,
    has_more: bool,
    total_rows: Option<u64>,
    elapsed: std::time::Duration,
}

impl ResultState {
    fn total_pages(&self) -> Option<usize> {
        self.total_rows
            .map(|t| (t as usize).div_ceil(PAGE_SIZE).max(1))
    }
}

pub struct QueryTab {
    app: Weak<App>,
    pub page: gtk::Box,
    pub editor: QueryEditor,
    pub results: Rc<ResultsView>,
    pub title: RefCell<String>,
    /// File su disco associato alla scheda.
    pub path: RefCell<Option<std::path::PathBuf>>,
    pub tab_label: gtk::Label,
    pub result_label: gtk::Label,
    ds_dropdown: gtk::DropDown,
    pub ds_id: RefCell<Option<String>>,
    syncing_dropdown: Cell<bool>,
    worker: RefCell<Option<(String, Rc<Worker>)>>,
    run_btn: gtk::Button,
    script_btn: gtk::Button,
    stop_btn: gtk::Button,
    conn_label: gtk::Label,
    running: Cell<bool>,
    state: RefCell<Option<ResultState>>,
    export_stop: RefCell<Option<Arc<AtomicBool>>>,
    count_generation: Cell<u64>,
}

impl QueryTab {
    pub fn new(app: &Rc<App>, title: &str, ds_id: Option<String>, sql: &str) -> Rc<QueryTab> {
        let settings = app.settings.borrow().clone();
        let dialect = ds_id
            .as_deref()
            .and_then(|id| app.ds_kind(id))
            .map(db::dialect_of)
            .unwrap_or(Dialect::MySql);
        let editor = QueryEditor::new(dialect, settings.editor_font_size);
        editor.set_text(sql);
        let results = ResultsView::new(settings.grid_font_size, settings.auto_scroll_pages);

        let ds_dropdown = gtk::DropDown::new(Some(app.ds_names.clone()), None::<gtk::Expression>);
        ds_dropdown.set_tooltip_text(Some("Datasource su cui eseguire le query"));
        let run_btn = labeled_button(
            "media-playback-start-symbolic",
            "Esegui",
            "Esegui lo statement sotto il cursore o la selezione (Ctrl+Invio)",
        );
        let script_btn = labeled_button(
            "media-seek-forward-symbolic",
            "Script",
            "Esegui tutto lo script (Ctrl+Shift+Invio / Alt+X)",
        );
        let stop_btn = labeled_button(
            "media-playback-stop-symbolic",
            "Stop",
            "Interrompi la query in esecuzione",
        );
        stop_btn.set_sensitive(false);
        let conn_label = gtk::Label::new(None);
        conn_label.add_css_class("dim-label");
        conn_label.set_hexpand(true);
        conn_label.set_xalign(1.0);
        conn_label.set_ellipsize(gtk::pango::EllipsizeMode::Start);

        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        toolbar.add_css_class("query-toolbar");
        toolbar.set_margin_start(4);
        toolbar.set_margin_end(6);
        toolbar.set_margin_top(3);
        toolbar.set_margin_bottom(3);
        toolbar.append(&ds_dropdown);
        toolbar.append(&run_btn);
        toolbar.append(&script_btn);
        toolbar.append(&stop_btn);
        toolbar.append(&conn_label);

        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.append(&toolbar);
        page.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        page.append(&editor.widget);

        let tab = Rc::new(QueryTab {
            app: Rc::downgrade(app),
            page,
            editor,
            results,
            title: RefCell::new(title.to_string()),
            path: RefCell::default(),
            tab_label: gtk::Label::new(Some(title)),
            result_label: gtk::Label::new(Some(title)),
            ds_dropdown,
            ds_id: RefCell::new(ds_id),
            syncing_dropdown: Cell::new(false),
            worker: RefCell::default(),
            run_btn,
            script_btn,
            stop_btn,
            conn_label,
            running: Cell::new(false),
            state: RefCell::default(),
            export_stop: RefCell::default(),
            count_generation: Cell::new(0),
        });
        tab.sync_dropdown();
        tab.connect_signals();
        tab.setup_completion();
        tab.editor.buffer.set_modified(false);
        let w = Rc::downgrade(&tab);
        tab.editor.buffer.connect_modified_changed(move |_| {
            if let Some(t) = w.upgrade() {
                t.refresh_title();
            }
        });
        tab
    }

    fn connect_signals(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        self.run_btn.connect_clicked(move |_| {
            if let Some(t) = w.upgrade() {
                t.run(false);
            }
        });
        let w = Rc::downgrade(self);
        self.script_btn.connect_clicked(move |_| {
            if let Some(t) = w.upgrade() {
                t.run(true);
            }
        });
        let w = Rc::downgrade(self);
        self.stop_btn.connect_clicked(move |_| {
            if let Some(t) = w.upgrade() {
                t.stop();
            }
        });
        let w = Rc::downgrade(self);
        self.ds_dropdown.connect_selected_notify(move |dd| {
            let Some(t) = w.upgrade() else { return };
            if t.syncing_dropdown.get() {
                return;
            }
            let Some(app) = t.app.upgrade() else { return };
            if app.refreshing_ds.get() {
                return;
            }
            let id = app
                .datasources
                .borrow()
                .get(dd.selected() as usize)
                .map(|d| d.id.clone());
            t.set_datasource(id);
        });
        let w = Rc::downgrade(self);
        self.results.set_on_nav(move |n| {
            if let Some(t) = w.upgrade() {
                t.navigate(n);
            }
        });
        let w = Rc::downgrade(self);
        self.results.set_on_export(move || {
            if let Some(t) = w.upgrade() {
                t.export();
            }
        });
    }

    /// L'autocompletamento usa i metadati del datasource selezionato nella scheda.
    fn setup_completion(self: &Rc<Self>) {
        let (app_w, tab_w) = (self.app.clone(), Rc::downgrade(self));
        let ds_of = move || -> Option<(Rc<App>, String)> {
            let tab = tab_w.upgrade()?;
            let id = tab.ds_id.borrow().clone()?;
            Some((app_w.upgrade()?, id))
        };
        let ds_of2 = ds_of.clone();
        self.editor
            .set_completion_source(Rc::new(super::editor::CompletionSource {
                meta: Box::new(move || {
                    let (app, id) = ds_of()?;
                    app.schema_meta(&id)
                }),
                load_columns: Box::new(move |tables, done| {
                    if let Some((app, id)) = ds_of2() {
                        app.load_columns(&id, tables, done);
                    }
                }),
            }));
    }

    pub fn set_datasource(&self, id: Option<String>) {
        self.editor.hide_completion();
        if *self.ds_id.borrow() == id {
            return;
        }
        *self.ds_id.borrow_mut() = id.clone();
        // La connessione della scheda è legata al datasource: va riaperta.
        self.worker.borrow_mut().take();
        self.conn_label.set_text("");
        if let Some(app) = self.app.upgrade()
            && let Some(kind) = id.as_deref().and_then(|i| app.ds_kind(i))
        {
            self.editor.set_dialect(db::dialect_of(kind));
        }
        self.sync_dropdown();
    }

    /// Allinea la selezione del menu a tendina al datasource della scheda.
    pub fn sync_dropdown(&self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let idx = self
            .ds_id
            .borrow()
            .as_ref()
            .and_then(|id| app.datasources.borrow().iter().position(|d| &d.id == id));
        self.syncing_dropdown.set(true);
        self.ds_dropdown
            .set_selected(idx.map(|i| i as u32).unwrap_or(gtk::INVALID_LIST_POSITION));
        self.syncing_dropdown.set(false);
    }

    /// Chiude la connessione della scheda (es. datasource disconnesso o modificato).
    pub fn drop_connection(&self, ds_id: &str) {
        let matches = self
            .worker
            .borrow()
            .as_ref()
            .is_some_and(|(id, _)| id == ds_id);
        if matches {
            self.worker.borrow_mut().take();
            self.conn_label.set_text("");
        }
    }

    pub fn set_title(&self, title: &str) {
        *self.title.borrow_mut() = title.to_string();
        self.refresh_title();
    }

    /// Nome mostrato: il nome del file se la scheda è salvata su disco.
    pub fn display_name(&self) -> String {
        match self.path.borrow().as_ref().and_then(|p| p.file_name()) {
            Some(name) => name.to_string_lossy().into_owned(),
            None => self.title.borrow().clone(),
        }
    }

    /// Vero se il file associato ha modifiche non salvate.
    pub fn is_dirty(&self) -> bool {
        self.path.borrow().is_some() && self.editor.buffer.is_modified()
    }

    pub fn set_path(&self, path: Option<std::path::PathBuf>) {
        *self.path.borrow_mut() = path;
        self.refresh_title();
    }

    pub fn refresh_title(&self) {
        let name = self.display_name();
        let shown = if self.is_dirty() {
            format!("● {name}")
        } else {
            name.clone()
        };
        self.tab_label.set_text(&shown);
        self.result_label.set_text(&name);
        let tip = self.path.borrow().as_ref().map(|p| {
            let state = if self.is_dirty() {
                "\nModifiche non salvate"
            } else {
                ""
            };
            format!("{}{state}", p.display())
        });
        self.tab_label.set_tooltip_text(tip.as_deref());
    }

    fn set_running(&self, running: bool) {
        self.running.set(running);
        self.run_btn.set_sensitive(!running);
        self.script_btn.set_sensitive(!running);
        self.stop_btn.set_sensitive(running);
        self.results
            .export
            .set_sensitive(!running && self.state.borrow().is_some());
        if running {
            self.tab_label.add_css_class("running");
        } else {
            self.tab_label.remove_css_class("running");
        }
    }

    pub fn stop(&self) {
        if let Some(flag) = self.export_stop.borrow().as_ref() {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some((_, w)) = self.worker.borrow().as_ref()
            && self.running.get()
        {
            w.cancel();
            self.results
                .log("Richiesta di interruzione inviata", Some("dim"));
        }
    }

    async fn ensure_worker(&self, app: &Rc<App>, ds_id: &str) -> Result<Rc<Worker>, String> {
        // La connessione principale (albero) viene aperta per prima: conferma
        // che il datasource sia raggiungibile e popola il pannello di sinistra.
        app.ensure_connected(ds_id).await?;
        if let Some((id, w)) = self.worker.borrow().as_ref()
            && id == ds_id
        {
            return Ok(w.clone());
        }
        let ds = app.datasource(ds_id).ok_or("Datasource non trovato")?;
        let w = Rc::new(Worker::connect(ds.clone()).await?);
        self.conn_label
            .set_text(&format!("{} · {}", ds.summary(), w.server_version));
        *self.worker.borrow_mut() = Some((ds_id.to_string(), w.clone()));
        Ok(w)
    }

    /// Esegue lo statement corrente (o la selezione) oppure l'intero script.
    pub fn run(self: &Rc<Self>, whole_script: bool) {
        if self.running.get() {
            return;
        }
        let stmts = self.editor.statements_to_run(whole_script);
        if stmts.is_empty() {
            return;
        }
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let Some(ds_id) = self.ds_id.borrow().clone() else {
            show_error(
                Some(app.window.upcast_ref()),
                "Nessun datasource",
                "Seleziona il datasource su cui eseguire la query.",
            );
            return;
        };
        let tab = self.clone();
        glib::spawn_future_local(async move {
            tab.run_statements(app, ds_id, stmts).await;
        });
    }

    async fn run_statements(
        self: Rc<Self>,
        app: Rc<App>,
        ds_id: String,
        stmts: Vec<(String, i32, i32)>,
    ) {
        self.set_running(true);
        self.results.set_busy(true, Some("Connessione…"));
        let worker = match self.ensure_worker(&app, &ds_id).await {
            Ok(w) => w,
            Err(e) => {
                self.results
                    .log(&format!("Connessione fallita: {e}"), Some("err"));
                self.results.set_info("Connessione fallita");
                self.results.show_messages();
                self.results.set_busy(false, None);
                self.set_running(false);
                return;
            }
        };
        let Some(ds) = app.datasource(&ds_id) else {
            self.results
                .set_busy(false, Some("Datasource non più disponibile"));
            self.set_running(false);
            return;
        };
        let dialect = db::dialect_of(ds.kind);
        let total = stmts.len();
        let script_start = Instant::now();
        let mut shown_rows = false;
        let mut failed = false;
        let mut last_affected: Option<String> = None;
        for (i, (sql, start, end)) in stmts.into_iter().enumerate() {
            self.editor.flash_range(start, end);
            let progress = if total > 1 {
                format!("Esecuzione {}/{}…", i + 1, total)
            } else {
                "Esecuzione…".into()
            };
            self.results.set_busy(true, Some(&progress));
            let started = Instant::now();
            let base = sqlutil::pageable_base(&sql, dialect);
            let q = match &base {
                Some(b) => sqlutil::paged_sql(b, PAGE_SIZE + 1, 0),
                None => sql.clone(),
            };
            let res = worker.run(move |c| c.execute(&q)).await.and_then(|r| r);
            let elapsed = started.elapsed();
            let short = summarize_sql(&sql);
            match res {
                Err(e) => {
                    self.results.log(&format!("✗ {short}\n{e}"), Some("err"));
                    self.results
                        .set_info(&format!("Errore: {}", e.lines().next().unwrap_or("")));
                    self.results.show_messages();
                    failed = true;
                    break;
                }
                Ok(StmtResult::Affected { rows, message }) => {
                    let msg = format!(
                        "✓ {short} — {} righe interessate{} ({})",
                        fmt_count(rows),
                        if message.is_empty() {
                            String::new()
                        } else {
                            format!(" · {message}")
                        },
                        fmt_duration(elapsed)
                    );
                    self.results.log(&msg, Some("ok"));
                    last_affected = Some(msg);
                    // DDL: l'albero potrebbe essere cambiato
                    let kw = sqlutil::first_keyword(&sql, dialect);
                    if matches!(kw.as_str(), "CREATE" | "DROP" | "ALTER" | "RENAME") {
                        app.schema_changed(&ds_id);
                    }
                }
                Ok(StmtResult::Rows(mut rs)) => {
                    let state = match base {
                        Some(base) => {
                            let has_more = rs.rows.len() > PAGE_SIZE;
                            rs.rows.truncate(PAGE_SIZE);
                            let total_rows = (!has_more).then_some(rs.rows.len() as u64);
                            ResultState {
                                sql: sql.clone(),
                                ds: ds.clone(),
                                source: PageSource::Server { base },
                                page: 0,
                                has_more,
                                total_rows,
                                elapsed,
                            }
                        }
                        None => {
                            let n = rs.rows.len();
                            let all = Rc::new(rs);
                            rs = page_slice(&all, 0);
                            ResultState {
                                sql: sql.clone(),
                                ds: ds.clone(),
                                source: PageSource::Client { all },
                                page: 0,
                                has_more: n > PAGE_SIZE,
                                total_rows: Some(n as u64),
                                elapsed,
                            }
                        }
                    };
                    let rows_label = match state.total_rows {
                        Some(t) => format!("{} righe", fmt_count(t)),
                        None => format!("più di {} righe, paginata", PAGE_SIZE),
                    };
                    self.results.log(
                        &format!("✓ {short} — {rows_label} ({})", fmt_duration(elapsed)),
                        Some("ok"),
                    );
                    let need_count = state.total_rows.is_none();
                    self.show_page(state, Rc::new(rs));
                    shown_rows = true;
                    if need_count {
                        self.start_count();
                    }
                }
            }
        }
        if !failed {
            if !shown_rows {
                if let Some(msg) = last_affected {
                    self.results.set_info(msg.trim_start_matches("✓ "));
                }
                self.results.show_messages();
            } else {
                self.results.show_data();
            }
            if total > 1 {
                self.results.log(
                    &format!(
                        "Script completato: {total} statement in {}",
                        fmt_duration(script_start.elapsed())
                    ),
                    Some("dim"),
                );
            }
        }
        self.results.set_busy(false, None);
        self.set_running(false);
    }

    /// Mostra una pagina e aggiorna barra di paginazione e testo informativo.
    fn show_page(&self, state: ResultState, rows: Rc<RowSet>) {
        let offset = state.page * PAGE_SIZE;
        self.results.grid.set_data(rows.clone(), offset);
        if self.results.scroll_to_bottom_after_load.replace(false) {
            let adj = self.results.grid.scroller.vadjustment();
            glib::idle_add_local_once(move || adj.set_value(adj.upper()));
        }
        *self.state.borrow_mut() = Some(state);
        self.update_pager_info(rows.rows.len());
        self.results.show_data();
        self.results.export.set_sensitive(true);
    }

    fn update_pager_info(&self, page_rows: usize) {
        let st = self.state.borrow();
        let Some(st) = st.as_ref() else { return };
        let paged = st.has_more || st.page > 0;
        self.results
            .set_pager(paged.then(|| (st.page, st.total_pages(), st.has_more)));
        let offset = st.page * PAGE_SIZE;
        let mut info = match st.total_rows {
            Some(t) if paged => format!(
                "Righe {}–{} di {}",
                fmt_count(offset as u64 + 1),
                fmt_count((offset + page_rows) as u64),
                fmt_count(t)
            ),
            Some(t) => format!("{} righe", fmt_count(t)),
            None => format!(
                "Righe {}–{} di … (conteggio in corso)",
                fmt_count(offset as u64 + 1),
                fmt_count((offset + page_rows) as u64)
            ),
        };
        if page_rows == 0 && paged {
            info = "Nessuna riga in questa pagina".into();
        }
        if !st.elapsed.is_zero() {
            info.push_str(&format!(" · {}", fmt_duration(st.elapsed)));
        }
        if let PageSource::Client { .. } = st.source
            && paged
        {
            info.push_str(" · paginazione in memoria");
        }
        self.results.set_info(&info);
    }

    /// Conta le righe totali su una connessione separata, senza bloccare la scheda.
    fn start_count(self: &Rc<Self>) {
        let (ds, base) = {
            let st = self.state.borrow();
            let Some(st) = st.as_ref() else { return };
            let PageSource::Server { base } = &st.source else {
                return;
            };
            (st.ds.clone(), base.clone())
        };
        let generation = self.count_generation.get() + 1;
        self.count_generation.set(generation);
        let dialect = db::dialect_of(ds.kind);
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let sql = sqlutil::count_sql(&base, dialect);
            let res = worker::run_detached(ds, move |c| {
                if let db::DbConn::Postgres(_) = c {
                    let _ = c.execute("SET statement_timeout = 15000");
                }
                c.query_scalar(&sql)
            })
            .await;
            let Some(tab) = w.upgrade() else { return };
            if tab.count_generation.get() != generation {
                return;
            }
            let page_rows = tab.results.grid.row_count();
            match res {
                Ok(Some(v)) => {
                    if let Ok(n) = v.parse::<u64>() {
                        if let Some(st) = tab.state.borrow_mut().as_mut() {
                            st.total_rows = Some(n);
                        }
                        tab.update_pager_info(page_rows);
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tab.results.log(
                        &format!(
                            "Conteggio righe non disponibile: {}",
                            e.lines().next().unwrap_or("")
                        ),
                        Some("dim"),
                    );
                    let info = format!(
                        "Pagina {} · totale non disponibile",
                        tab.state.borrow().as_ref().map_or(1, |s| s.page + 1)
                    );
                    tab.results.set_info(&info);
                }
            }
        });
    }

    fn navigate(self: &Rc<Self>, nav: PageNav) {
        if self.running.get() {
            return;
        }
        let target = {
            let st = self.state.borrow();
            let Some(st) = st.as_ref() else { return };
            let last = st.total_pages().map(|t| t - 1);
            let t = match nav {
                PageNav::First => 0,
                PageNav::Prev => st.page.saturating_sub(1),
                PageNav::Next if st.has_more => st.page + 1,
                PageNav::Next => return,
                PageNav::Last => match last {
                    Some(l) => l,
                    None => return,
                },
                PageNav::Goto(n) => last.map_or(n, |l| n.min(l)),
            };
            if t == st.page {
                return;
            }
            t
        };
        let client_all = match &self.state.borrow().as_ref().unwrap().source {
            PageSource::Client { all } => Some(all.clone()),
            PageSource::Server { .. } => None,
        };
        if let Some(all) = client_all {
            let rows = page_slice(&all, target);
            let mut st = self.state.borrow_mut().take().unwrap();
            st.page = target;
            st.has_more = (target + 1) * PAGE_SIZE < all.rows.len();
            st.elapsed = std::time::Duration::ZERO;
            self.show_page(st, Rc::new(rows));
            return;
        }
        let tab = self.clone();
        glib::spawn_future_local(async move { tab.load_server_page(target).await });
    }

    async fn load_server_page(self: Rc<Self>, target: usize) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let (base, ds_id) = {
            let st = self.state.borrow();
            let Some(st) = st.as_ref() else { return };
            let PageSource::Server { base } = &st.source else {
                return;
            };
            (base.clone(), st.ds.id.clone())
        };
        self.set_running(true);
        self.results
            .set_busy(true, Some(&format!("Caricamento pagina {}…", target + 1)));
        let started = Instant::now();
        let res = match self.ensure_worker(&app, &ds_id).await {
            Ok(w) => {
                let q = sqlutil::paged_sql(&base, PAGE_SIZE + 1, target * PAGE_SIZE);
                w.run(move |c| c.execute(&q)).await.and_then(|r| r)
            }
            Err(e) => Err(e),
        };
        self.results.set_busy(false, None);
        self.set_running(false);
        match res {
            Ok(StmtResult::Rows(mut rs)) => {
                let has_more = rs.rows.len() > PAGE_SIZE;
                rs.rows.truncate(PAGE_SIZE);
                let Some(mut st) = self.state.borrow_mut().take() else {
                    return;
                };
                st.page = target;
                st.has_more = has_more;
                if !has_more && st.total_rows.is_none() {
                    st.total_rows = Some((target * PAGE_SIZE + rs.rows.len()) as u64);
                }
                st.elapsed = started.elapsed();
                self.show_page(st, Rc::new(rs));
            }
            Ok(_) => {}
            Err(e) => {
                self.results.log(
                    &format!("✗ Caricamento pagina {}: {e}", target + 1),
                    Some("err"),
                );
                self.results.show_messages();
            }
        }
    }

    fn export(self: &Rc<Self>) {
        if self.running.get() || self.state.borrow().is_none() {
            return;
        }
        let tab = self.clone();
        glib::spawn_future_local(async move { tab.export_async().await });
    }

    async fn export_async(self: Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let (all_label, page_label, title) = {
            let st = self.state.borrow();
            let Some(st) = st.as_ref() else { return };
            let all = match st.total_rows {
                Some(t) => format!("Tutte le righe ({})", fmt_count(t)),
                None => "Tutte le righe (la query viene rieseguita)".to_string(),
            };
            let page = (st.has_more || st.page > 0).then(|| {
                format!(
                    "Solo la pagina corrente ({} righe)",
                    fmt_count(self.results.grid.row_count() as u64)
                )
            });
            let name = self.display_name();
            let name = name.strip_suffix(".sql").unwrap_or(&name).to_string();
            (all, page, name)
        };
        let Some(choice) =
            super::dialogs::export_options(&app, &all_label, page_label.as_deref()).await
        else {
            return;
        };
        let file_dialog = gtk::FileDialog::builder()
            .title("Esporta CSV")
            .modal(true)
            .initial_name(format!("{}.csv", sanitize_filename(&title)))
            .build();
        let Ok(file) = file_dialog.save_future(Some(&app.window)).await else {
            return;
        };
        let Some(path) = file.path() else { return };
        app.save_settings();
        self.write_export(&app, choice, path).await;
    }

    /// Scrive il CSV: dalla memoria (pagina corrente o risultato non paginato)
    /// oppure rieseguendo la query in streaming sulla connessione della scheda.
    pub async fn write_export(
        &self,
        app: &Rc<App>,
        choice: super::dialogs::ExportChoice,
        path: std::path::PathBuf,
    ) {
        let started = Instant::now();
        let source_rows: Option<Rc<RowSet>> = {
            let st = self.state.borrow();
            let st = st.as_ref().unwrap();
            if !choice.all_rows {
                Some(self.results.grid.rowset())
            } else if let PageSource::Client { all } = &st.source {
                Some(all.clone())
            } else {
                None
            }
        };
        let result = match source_rows {
            Some(rs) => db::rowset_to_csv(&rs, &path, &choice.options),
            None => {
                let (sql, ds_id) = {
                    let st = self.state.borrow();
                    let st = st.as_ref().unwrap();
                    (st.sql.clone(), st.ds.id.clone())
                };
                self.set_running(true);
                self.results.set_busy(true, Some("Esportazione in corso…"));
                let stop = Arc::new(AtomicBool::new(false));
                *self.export_stop.borrow_mut() = Some(stop.clone());
                let (ptx, prx) = async_channel::unbounded::<u64>();
                let results = self.results.clone();
                glib::spawn_future_local(async move {
                    while let Ok(n) = prx.recv().await {
                        results.set_info(&format!("Esportazione in corso… {} righe", fmt_count(n)));
                    }
                });
                let res = match self.ensure_worker(app, &ds_id).await {
                    Ok(w) => {
                        let path = path.clone();
                        let opts = choice.options.clone();
                        w.run(move |c| {
                            c.export_csv(&sql, &path, &opts, &stop, &move |n| {
                                let _ = ptx.try_send(n);
                            })
                        })
                        .await
                        .and_then(|r| r)
                    }
                    Err(e) => Err(e),
                };
                self.export_stop.borrow_mut().take();
                self.results.set_busy(false, None);
                self.set_running(false);
                res
            }
        };
        match result {
            Ok(n) => {
                let msg = format!(
                    "✓ Esportate {} righe in {} ({})",
                    fmt_count(n),
                    path.display(),
                    fmt_duration(started.elapsed())
                );
                self.results.log(&msg, Some("ok"));
                self.results.set_info(msg.trim_start_matches("✓ "));
            }
            Err(e) => {
                self.results
                    .log(&format!("✗ Esportazione fallita: {e}"), Some("err"));
                self.results.show_messages();
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

fn page_slice(all: &RowSet, page: usize) -> RowSet {
    let start = (page * PAGE_SIZE).min(all.rows.len());
    let end = (start + PAGE_SIZE).min(all.rows.len());
    RowSet {
        columns: all.columns.clone(),
        rows: all.rows[start..end].to_vec(),
    }
}

/// Prima riga dello statement, accorciata, per il log.
fn summarize_sql(sql: &str) -> String {
    let one: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 90 {
        format!("{}…", one.chars().take(90).collect::<String>())
    } else {
        one
    }
}

fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.trim_matches('_').is_empty() {
        "risultati".into()
    } else {
        cleaned
    }
}
