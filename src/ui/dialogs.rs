//! Finestre di dialogo: configurazione datasource, opzioni di esportazione
//! CSV e analisi della tabella.

use super::app::App;
use super::editor::QueryEditor;
use super::grid::DataGrid;
use gtk::prelude::*;
use gtk::{gdk, glib};
use lemure::config::{DataSource, DbKind};
use lemure::db::{self, ColumnMeta, CsvOptions, ForeignKeyInfo, RowSet, TableAnalysis};
use lemure::worker::Worker;
use std::rc::Rc;

fn dialog_window(parent: &impl IsA<gtk::Window>, title: &str, modal: bool) -> gtk::Window {
    let w = gtk::Window::builder()
        .title(title)
        .transient_for(parent)
        .modal(modal)
        .destroy_with_parent(true)
        .build();
    let esc = gtk::EventControllerKey::new();
    let ww = w.clone();
    esc.connect_key_pressed(move |_, k, _, _| {
        if k == gdk::Key::Escape {
            ww.close();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    w.add_controller(esc);
    w
}

fn field_label(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(1.0);
    l.add_css_class("dim-label");
    l
}

fn button_row(buttons: &[&gtk::Button]) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    b.set_halign(gtk::Align::End);
    for x in buttons {
        b.append(*x);
    }
    b
}

// ---------------------------------------------------------------------------
// Datasource
// ---------------------------------------------------------------------------

pub fn datasource_dialog(app: &Rc<App>, existing: Option<DataSource>) {
    let is_new = existing.is_none();
    let ds = existing.unwrap_or_else(|| DataSource {
        id: DataSource::new_id(),
        name: String::new(),
        kind: DbKind::MySql,
        host: "127.0.0.1".into(),
        port: 3306,
        user: "root".into(),
        password: String::new(),
        database: String::new(),
    });
    let win = dialog_window(
        &app.window,
        if is_new {
            "Nuovo datasource"
        } else {
            "Modifica datasource"
        },
        true,
    );
    win.set_default_width(460);

    let name = gtk::Entry::builder()
        .text(&ds.name)
        .placeholder_text("es. Sviluppo locale")
        .hexpand(true)
        .build();
    let kind = gtk::DropDown::from_strings(&["MySQL", "PostgreSQL"]);
    kind.set_selected(if ds.kind == DbKind::MySql { 0 } else { 1 });
    let host = gtk::Entry::builder().text(&ds.host).build();
    let port = gtk::SpinButton::with_range(1.0, 65535.0, 1.0);
    port.set_value(ds.port as f64);
    let user = gtk::Entry::builder().text(&ds.user).build();
    let password = gtk::PasswordEntry::builder()
        .text(&ds.password)
        .show_peek_icon(true)
        .build();
    let database = gtk::Entry::builder().text(&ds.database).build();
    let set_hint = {
        let database = database.clone();
        move |mysql: bool| {
            database.set_placeholder_text(Some(if mysql {
                "(vuoto = mostra tutti i database)"
            } else {
                "(vuoto = database con il nome dell'utente)"
            }));
        }
    };
    set_hint(ds.kind == DbKind::MySql);
    {
        let port = port.clone();
        kind.connect_selected_notify(move |dd| {
            let mysql = dd.selected() == 0;
            let (old, new) = if mysql {
                (5432.0, 3306.0)
            } else {
                (3306.0, 5432.0)
            };
            if port.value() == old {
                port.set_value(new);
            }
            set_hint(mysql);
        });
    }

    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .build();
    let rows: [(&str, gtk::Widget); 7] = [
        ("Nome", name.clone().upcast()),
        ("Tipo", kind.clone().upcast()),
        ("Host", host.clone().upcast()),
        ("Porta", port.clone().upcast()),
        ("Utente", user.clone().upcast()),
        ("Password", password.clone().upcast()),
        ("Database", database.clone().upcast()),
    ];
    for (i, (l, w)) in rows.iter().enumerate() {
        grid.attach(&field_label(l), 0, i as i32, 1, 1);
        grid.attach(w, 1, i as i32, 1, 1);
    }
    let note = gtk::Label::new(Some(
        "La password viene salvata in chiaro in ~/.config/lemure (uso solo in sviluppo).",
    ));
    note.add_css_class("dim-label");
    note.add_css_class("caption");
    note.set_wrap(true);
    note.set_xalign(0.0);

    let test_result = gtk::Label::new(None);
    test_result.set_wrap(true);
    test_result.set_xalign(0.0);
    test_result.set_selectable(true);
    let test = gtk::Button::with_label("Test connessione");
    let cancel = gtk::Button::with_label("Annulla");
    let save = gtk::Button::with_label("Salva");
    save.add_css_class("suggested-action");
    let spinner = gtk::Spinner::new();

    let read_form = {
        let (name, kind, host, port, user, password, database) = (
            name.clone(),
            kind.clone(),
            host.clone(),
            port.clone(),
            user.clone(),
            password.clone(),
            database.clone(),
        );
        let id = ds.id.clone();
        move || DataSource {
            id: id.clone(),
            name: name.text().trim().to_string(),
            kind: if kind.selected() == 0 {
                DbKind::MySql
            } else {
                DbKind::Postgres
            },
            host: host.text().trim().to_string(),
            port: port.value() as u16,
            user: user.text().trim().to_string(),
            password: password.text().to_string(),
            database: database.text().trim().to_string(),
        }
    };

    {
        let read_form = read_form.clone();
        let test_result = test_result.clone();
        let spinner = spinner.clone();
        test.connect_clicked(move |btn| {
            let ds = read_form();
            let (btn, test_result, spinner) = (btn.clone(), test_result.clone(), spinner.clone());
            btn.set_sensitive(false);
            spinner.start();
            test_result.set_text("Connessione in corso…");
            test_result.remove_css_class("error-text");
            test_result.remove_css_class("success-text");
            glib::spawn_future_local(async move {
                match Worker::connect(ds).await {
                    Ok(w) => {
                        test_result
                            .set_text(&format!("✓ Connessione riuscita — {}", w.server_version));
                        test_result.add_css_class("success-text");
                    }
                    Err(e) => {
                        test_result.set_text(&format!("✗ {e}"));
                        test_result.add_css_class("error-text");
                    }
                }
                spinner.stop();
                btn.set_sensitive(true);
            });
        });
    }
    {
        let w = win.clone();
        cancel.connect_clicked(move |_| w.close());
    }
    {
        let w = win.clone();
        let app = app.clone();
        let name = name.clone();
        let host = host.clone();
        save.connect_clicked(move |_| {
            let ds = read_form();
            if ds.name.is_empty() {
                name.grab_focus();
                name.add_css_class("error");
                return;
            }
            if ds.host.is_empty() {
                host.grab_focus();
                host.add_css_class("error");
                return;
            }
            app.save_datasource(ds, is_new);
            w.close();
        });
    }

    let test_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    test_box.append(&test);
    test_box.append(&spinner);
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 12);
    vbox.set_margin_top(18);
    vbox.set_margin_bottom(18);
    vbox.set_margin_start(18);
    vbox.set_margin_end(18);
    vbox.append(&grid);
    vbox.append(&note);
    vbox.append(&test_box);
    vbox.append(&test_result);
    vbox.append(&button_row(&[&cancel, &save]));
    win.set_child(Some(&vbox));
    win.set_default_widget(Some(&save));
    name.set_activates_default(true);
    win.present();
    name.grab_focus();
}

// ---------------------------------------------------------------------------
// Esportazione CSV
// ---------------------------------------------------------------------------

const SEPARATORS: [(&str, &str); 5] = [
    ("; (punto e virgola)", ";"),
    (", (virgola)", ","),
    ("Tab", "\t"),
    ("| (barra verticale)", "|"),
    ("Personalizzato…", ""),
];

pub struct ExportChoice {
    pub options: CsvOptions,
    pub all_rows: bool,
}

/// Chiede le opzioni di esportazione. `all_label` descrive l'intero risultato.
pub async fn export_options(
    app: &Rc<App>,
    all_label: &str,
    page_label: Option<&str>,
) -> Option<ExportChoice> {
    let settings = app.settings.borrow().clone();
    let win = dialog_window(&app.window, "Esporta CSV", true);
    win.set_default_width(420);

    let names: Vec<&str> = SEPARATORS.iter().map(|s| s.0).collect();
    let sep = gtk::DropDown::from_strings(&names);
    let custom = gtk::Entry::builder()
        .max_length(5)
        .width_chars(5)
        .sensitive(false)
        .build();
    let idx = SEPARATORS
        .iter()
        .position(|s| s.1 == settings.csv_separator && !s.1.is_empty());
    match idx {
        Some(i) => sep.set_selected(i as u32),
        None => {
            sep.set_selected(4);
            custom.set_text(&settings.csv_separator);
            custom.set_sensitive(true);
        }
    }
    {
        let custom = custom.clone();
        sep.connect_selected_notify(move |d| {
            custom.set_sensitive(d.selected() == 4);
            if d.selected() == 4 {
                custom.grab_focus();
            }
        });
    }
    let header = gtk::CheckButton::with_label("Includi riga di intestazione");
    header.set_active(settings.csv_header);
    let quote_all = gtk::CheckButton::with_label("Racchiudi tutti i valori tra virgolette");
    let all = gtk::CheckButton::with_label(all_label);
    all.set_active(true);
    let page = gtk::CheckButton::with_label(page_label.unwrap_or(""));
    page.set_group(Some(&all));
    page.set_visible(page_label.is_some());

    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .build();
    grid.attach(&field_label("Separatore"), 0, 0, 1, 1);
    let sep_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    sep_box.append(&sep);
    sep_box.append(&custom);
    grid.attach(&sep_box, 1, 0, 1, 1);
    grid.attach(&header, 1, 1, 1, 1);
    grid.attach(&quote_all, 1, 2, 1, 1);
    grid.attach(&field_label("Righe"), 0, 3, 1, 1);
    let scope = gtk::Box::new(gtk::Orientation::Vertical, 4);
    scope.append(&all);
    scope.append(&page);
    grid.attach(&scope, 1, 3, 1, 1);

    let cancel = gtk::Button::with_label("Annulla");
    let ok = gtk::Button::with_label("Scegli file…");
    ok.add_css_class("suggested-action");
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 16);
    {
        let m = 18;
        vbox.set_margin_top(m);
        vbox.set_margin_bottom(m);
        vbox.set_margin_start(m);
        vbox.set_margin_end(m);
    }
    vbox.append(&grid);
    vbox.append(&button_row(&[&cancel, &ok]));
    win.set_child(Some(&vbox));
    win.set_default_widget(Some(&ok));

    let (tx, rx) = async_channel::bounded::<Option<ExportChoice>>(1);
    {
        let tx = tx.clone();
        let w = win.clone();
        cancel.connect_clicked(move |_| {
            let _ = tx.try_send(None);
            w.close();
        });
    }
    {
        let tx = tx.clone();
        let w = win.clone();
        ok.connect_clicked(move |_| {
            let separator = match sep.selected() as usize {
                i if i < 4 => SEPARATORS[i].1.to_string(),
                _ => custom.text().to_string(),
            };
            if separator.is_empty() {
                custom.add_css_class("error");
                return;
            }
            let _ = tx.try_send(Some(ExportChoice {
                options: CsvOptions {
                    separator,
                    header: header.is_active(),
                    quote_all: quote_all.is_active(),
                },
                all_rows: all.is_active(),
            }));
            w.close();
        });
    }
    win.connect_close_request(move |_| {
        let _ = tx.try_send(None);
        glib::Propagation::Proceed
    });
    win.present();
    let choice = rx.recv().await.ok().flatten();
    if let Some(c) = &choice {
        let mut s = app.settings.borrow_mut();
        s.csv_separator = c.options.separator.clone();
        s.csv_header = c.options.header;
    }
    choice
}

// ---------------------------------------------------------------------------
// Analisi tabella
// ---------------------------------------------------------------------------

fn rowset(headers: &[&str], rows: Vec<Vec<String>>) -> RowSet {
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
                r.into_iter()
                    .map(|v| if v.is_empty() { None } else { Some(v) })
                    .collect()
            })
            .collect(),
    }
}

fn fk_rowset(fks: &[ForeignKeyInfo], incoming: bool) -> RowSet {
    let headers: &[&str] = if incoming {
        &[
            "Vincolo",
            "Tabella",
            "Colonne",
            "→ Colonne di questa tabella",
            "ON UPDATE",
            "ON DELETE",
        ]
    } else {
        &[
            "Vincolo",
            "Colonne",
            "→ Tabella riferita",
            "Colonne riferite",
            "ON UPDATE",
            "ON DELETE",
        ]
    };
    rowset(
        headers,
        fks.iter()
            .map(|f| {
                if incoming {
                    vec![
                        f.name.clone(),
                        f.table.clone(),
                        f.columns.clone(),
                        f.ref_columns.clone(),
                        f.on_update.clone(),
                        f.on_delete.clone(),
                    ]
                } else {
                    vec![
                        f.name.clone(),
                        f.columns.clone(),
                        f.ref_table.clone(),
                        f.ref_columns.clone(),
                        f.on_update.clone(),
                        f.on_delete.clone(),
                    ]
                }
            })
            .collect(),
    )
}

fn grid_with(rs: RowSet, font: f64) -> gtk::ScrolledWindow {
    let g = DataGrid::new(font);
    g.set_data(Rc::new(rs), 0);
    let sc = g.scroller.clone();
    // la griglia vive finché vive il suo widget
    unsafe {
        sc.set_data("grid", g);
    }
    sc
}

fn section_title(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("heading");
    l.set_xalign(0.0);
    l.set_margin_top(8);
    l.set_margin_bottom(4);
    l.set_margin_start(4);
    l
}

pub fn show_analyze(app: &Rc<App>, ds_id: String, schema: String, table: String) {
    let Some(ds) = app.datasource(&ds_id) else {
        return;
    };
    let win = dialog_window(&app.window, &format!("Analizza tabella — {table}"), false);
    win.set_default_size(900, 600);
    let spinner = gtk::Spinner::new();
    spinner.start();
    spinner.set_size_request(32, 32);
    spinner.set_valign(gtk::Align::Center);
    spinner.set_halign(gtk::Align::Center);
    win.set_child(Some(&spinner));
    win.present();

    let app = app.clone();
    glib::spawn_future_local(async move {
        let res = async {
            let worker = app.ensure_connected(&ds_id).await?;
            let (s, t) = (schema.clone(), table.clone());
            worker.run(move |c| c.analyze_table(&s, &t)).await?
        }
        .await;
        match res {
            Ok(a) => {
                let font = app.settings.borrow().grid_font_size;
                let editor_font = app.settings.borrow().editor_font_size;
                win.set_child(Some(&analysis_view(
                    &a,
                    db::dialect_of(ds.kind),
                    font,
                    editor_font,
                )));
            }
            Err(e) => {
                let l = gtk::Label::new(Some(&format!("Errore durante l'analisi:\n{e}")));
                l.add_css_class("error-text");
                l.set_wrap(true);
                l.set_selectable(true);
                win.set_child(Some(&l));
            }
        }
    });
}

fn analysis_view(
    a: &TableAnalysis,
    dialect: lemure::sqlutil::Dialect,
    font: f64,
    editor_font: f64,
) -> gtk::Widget {
    let nb = gtk::Notebook::new();

    // --- Generale: proprietà, chiave primaria e riepilogo
    let general = gtk::Box::new(gtk::Orientation::Vertical, 6);
    general.set_margin_top(12);
    general.set_margin_start(16);
    general.set_margin_end(16);
    general.set_margin_bottom(12);
    general.append(&section_title("Chiave"));
    let key = gtk::Label::new(Some(&a.key_description));
    key.set_xalign(0.0);
    key.set_wrap(true);
    key.set_selectable(true);
    key.add_css_class("key-description");
    general.append(&key);
    if let Some(pk) = &a.primary_key {
        let l = gtk::Label::new(Some(&format!(
            "Indice: {}   Colonne: {}",
            pk.name, pk.columns
        )));
        l.set_xalign(0.0);
        l.set_selectable(true);
        l.add_css_class("dim-label");
        general.append(&l);
    }
    let uniques: Vec<String> = a
        .indexes
        .iter()
        .filter(|i| i.unique && !i.primary)
        .map(|i| format!("{} ({})", i.name, i.columns))
        .collect();
    if !uniques.is_empty() {
        let l = gtk::Label::new(Some(&format!("Chiavi univoche: {}", uniques.join(", "))));
        l.set_xalign(0.0);
        l.set_wrap(true);
        l.set_selectable(true);
        general.append(&l);
    }
    let summary = gtk::Label::new(Some(&format!(
        "{} colonne · {} indici · {} chiavi esterne · referenziata da {} vincoli",
        a.columns.rows.len(),
        a.indexes.len(),
        a.foreign_keys.len(),
        a.referenced_by.len()
    )));
    summary.set_xalign(0.0);
    summary.add_css_class("dim-label");
    general.append(&summary);
    general.append(&section_title("Proprietà"));
    let props = gtk::Grid::builder()
        .row_spacing(4)
        .column_spacing(16)
        .build();
    for (i, (k, v)) in a.properties.iter().enumerate() {
        props.attach(&field_label(k), 0, i as i32, 1, 1);
        let val = gtk::Label::new(Some(v));
        val.set_xalign(0.0);
        val.set_selectable(true);
        val.set_wrap(true);
        props.attach(&val, 1, i as i32, 1, 1);
    }
    general.append(&props);
    let gsc = gtk::ScrolledWindow::new();
    gsc.set_child(Some(&general));
    nb.append_page(&gsc, Some(&gtk::Label::new(Some("Generale"))));

    // --- Colonne
    nb.append_page(
        &grid_with(a.columns.clone(), font),
        Some(&gtk::Label::new(Some("Colonne"))),
    );

    // --- Indici
    let idx = rowset(
        &["Nome", "Tipo", "Metodo", "Colonne"],
        a.indexes
            .iter()
            .map(|i| {
                let kind = if i.primary {
                    "PRIMARY KEY"
                } else if i.unique {
                    "UNIQUE"
                } else {
                    "INDEX"
                };
                vec![
                    i.name.clone(),
                    kind.into(),
                    i.method.clone(),
                    i.columns.clone(),
                ]
            })
            .collect(),
    );
    nb.append_page(
        &grid_with(idx, font),
        Some(&gtk::Label::new(Some(&format!(
            "Indici ({})",
            a.indexes.len()
        )))),
    );

    // --- Chiavi esterne (uscenti e entranti)
    let fk_box = gtk::Paned::new(gtk::Orientation::Vertical);
    let out_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    out_box.append(&section_title("Chiavi esterne di questa tabella"));
    out_box.append(&grid_with(fk_rowset(&a.foreign_keys, false), font));
    let in_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    in_box.append(&section_title("Tabelle che referenziano questa tabella"));
    in_box.append(&grid_with(fk_rowset(&a.referenced_by, true), font));
    fk_box.set_start_child(Some(&out_box));
    fk_box.set_end_child(Some(&in_box));
    fk_box.set_position(260);
    nb.append_page(
        &fk_box,
        Some(&gtk::Label::new(Some(&format!(
            "Chiavi esterne ({} / {})",
            a.foreign_keys.len(),
            a.referenced_by.len()
        )))),
    );

    // --- DDL con evidenziazione della sintassi
    let ddl = QueryEditor::new(dialect, editor_font);
    ddl.set_text(&a.ddl);
    ddl.view.set_editable(false);
    ddl.hide_status();
    let ddl_widget = ddl.widget.clone();
    unsafe {
        ddl_widget.set_data("editor", ddl);
    }
    nb.append_page(&ddl_widget, Some(&gtk::Label::new(Some("DDL"))));
    nb.upcast()
}
