//! Automazione per i test manuali della UI (feature `devtools`).
//!
//! `LEMURE_DEVSCRIPT=file` esegue un comando per riga:
//!   sleep MS | expand ETICHETTA | ds NOME | sql TESTO | run | script
//!   nav first|prev|next|last | analyze DS SCHEMA TABELLA | newtab
//!   cell RIGA COL | snap FILE.png | snap-all PREFISSO | zoom +|-|0 | quit

use super::app::App;
use super::results::PageNav;
use gtk::prelude::*;
use gtk::{glib, graphene};
use std::rc::Rc;
use std::time::Duration;

pub fn start(app: &Rc<App>) {
    let Ok(path) = std::env::var("LEMURE_DEVSCRIPT") else {
        return;
    };
    let script = std::fs::read_to_string(&path).unwrap_or_default();
    let lines: Vec<String> = script
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect();
    let app = app.clone();
    glib::spawn_future_local(async move {
        glib::timeout_future(Duration::from_millis(600)).await;
        for line in lines {
            let (cmd, arg) = line.split_once(' ').unwrap_or((line.as_str(), ""));
            eprintln!("devtools: {line}");
            match cmd {
                "sleep" => {
                    glib::timeout_future(Duration::from_millis(arg.parse().unwrap_or(500))).await
                }
                "expand" => {
                    if !app.sidebar.dev_expand(arg) {
                        eprintln!("devtools: nodo '{arg}' non trovato");
                    }
                }
                "ds" => {
                    let id = app
                        .datasources
                        .borrow()
                        .iter()
                        .find(|d| d.name == arg)
                        .map(|d| d.id.clone());
                    if let Some(t) = app.current_tab() {
                        t.set_datasource(id);
                    }
                }
                "sql" => {
                    if let Some(t) = app.current_tab() {
                        t.editor.set_text(&arg.replace("\\n", "\n"));
                    }
                }
                "run" => {
                    let _ = WidgetExt::activate_action(&app.window, "win.run", None);
                }
                "script" => {
                    let _ = WidgetExt::activate_action(&app.window, "win.run-script", None);
                }
                "newtab" => {
                    let _ = WidgetExt::activate_action(&app.window, "win.new-tab", None);
                }
                "zoom" => {
                    let a = match arg {
                        "+" => "win.zoom-in",
                        "-" => "win.zoom-out",
                        _ => "win.zoom-reset",
                    };
                    let _ = WidgetExt::activate_action(&app.window, a, None);
                }
                "nav" => {
                    let n = match arg {
                        "first" => PageNav::First,
                        "prev" => PageNav::Prev,
                        "last" => PageNav::Last,
                        _ => PageNav::Next,
                    };
                    if let Some(t) = app.current_tab() {
                        t.results.navigate(n);
                    }
                }
                "analyze" => {
                    let parts: Vec<&str> = arg.split('|').collect();
                    let id = app
                        .datasources
                        .borrow()
                        .iter()
                        .find(|d| d.name == parts[0])
                        .map(|d| d.id.clone());
                    if let Some(id) = id {
                        super::dialogs::show_analyze(&app, id, parts[1].into(), parts[2].into());
                    }
                }
                "snap" => snap(app.window.upcast_ref(), arg),
                "snap-all" => {
                    for (i, w) in gtk::Window::list_toplevels().into_iter().enumerate() {
                        if let Ok(w) = w.downcast::<gtk::Window>()
                            && w.is_visible()
                        {
                            snap(&w, &format!("{arg}-{i}.png"));
                        }
                    }
                }
                "dialog-page" => {
                    // cambia scheda nei notebook delle finestre secondarie
                    for w in gtk::Window::list_toplevels() {
                        let Ok(w) = w.downcast::<gtk::Window>() else {
                            continue;
                        };
                        if w == *app.window.upcast_ref::<gtk::Window>() {
                            continue;
                        }
                        if let Some(nb) = w.child().and_downcast::<gtk::Notebook>() {
                            nb.set_current_page(arg.parse().ok());
                        }
                    }
                }
                "select" => {
                    // select RIGA COL [+]  (+ = aggiunge alla selezione)
                    let p: Vec<&str> = arg.split_whitespace().collect();
                    if let Some(t) = app.current_tab() {
                        t.results.grid.dev_select(
                            p[0].parse().unwrap(),
                            p[1].parse().unwrap(),
                            p.get(2) == Some(&"+"),
                        );
                    }
                }
                "grid-action" => {
                    if let Some(t) = app.current_tab() {
                        t.results.grid.dev_action(arg);
                    }
                }
                "print-clipboard" => {
                    let cb = app.window.clipboard();
                    match cb.read_text_future().await {
                        Ok(Some(t)) => println!("CLIPBOARD<<{t}>>"),
                        other => println!("CLIPBOARD vuota: {other:?}"),
                    }
                }
                "export" => {
                    // export FILE all|page SEPARATORE
                    let p: Vec<&str> = arg.splitn(3, ' ').collect();
                    if let Some(t) = app.current_tab() {
                        let choice = super::dialogs::ExportChoice {
                            options: lemure::db::CsvOptions {
                                separator: p.get(2).copied().unwrap_or(";").replace("\\t", "\t"),
                                header: true,
                                quote_all: false,
                            },
                            all_rows: p.get(1) == Some(&"all"),
                        };
                        t.write_export(&app, choice, p[0].into()).await;
                    }
                }
                "ctx" => app.sidebar.dev_context(arg),
                "grid-ctx" => {
                    if let Some(t) = app.current_tab() {
                        t.results.grid.dev_context();
                    }
                }
                "snap-popovers" => {
                    let mut found = Vec::new();
                    collect_popovers(app.window.upcast_ref(), &mut found);
                    for (i, p) in found.iter().enumerate() {
                        snap_widget(p.upcast_ref(), &format!("{arg}-{i}.png"));
                        p.popdown();
                    }
                }
                "rename-ds" => {
                    let (old, new) = arg.split_once('|').unwrap();
                    let ds = app
                        .datasources
                        .borrow()
                        .iter()
                        .find(|d| d.name == old)
                        .cloned();
                    if let Some(mut ds) = ds {
                        ds.name = new.into();
                        app.save_datasource(ds, false);
                    }
                }
                "print-tabs" => {
                    for t in app.dev_tabs() {
                        let ds = t
                            .ds_id
                            .borrow()
                            .clone()
                            .and_then(|id| app.datasource(&id))
                            .map(|d| d.name);
                        println!(
                            "TAB {} [{}] path={:?} dirty={} -> {:?}",
                            t.title.borrow(),
                            t.tab_label.text(),
                            t.path.borrow(),
                            t.is_dirty(),
                            ds
                        );
                    }
                }
                "open" => app.open_path(std::path::Path::new(arg)),
                "save" => {
                    if let Some(t) = app.current_tab() {
                        if !arg.is_empty() {
                            t.set_path(Some(arg.into()));
                        }
                        println!("SAVED {}", app.save_tab(&t, false).await);
                    }
                }
                "print-recent" => {
                    for r in app.recent_files() {
                        println!("RECENT {} ds={:?}", r.path, r.datasource_id);
                    }
                }
                "close-current" => {
                    if let Some(t) = app.current_tab() {
                        app.request_close_tab(&t);
                    }
                }
                "type" => {
                    if let Some(t) = app.current_tab() {
                        t.editor.view.grab_focus();
                        t.editor
                            .dev_type(&arg.replace("\\n", "\n").replace("\\s", " "));
                    }
                }
                "key" => {
                    if let Some(t) = app.current_tab() {
                        t.editor.dev_key(arg);
                    }
                }
                "print-editor" => {
                    if let Some(t) = app.current_tab() {
                        let focus = gtk::prelude::GtkWindowExt::focus(&app.window)
                            .map(|w| w.type_().name().to_string());
                        println!(
                            "EDITOR<<{}>> popup={} focus={:?}",
                            t.editor.text(),
                            t.editor.dev_popup_visible(),
                            focus
                        );
                    }
                }
                "stop" => {
                    let _ = WidgetExt::activate_action(&app.window, "win.stop", None);
                }
                "scroll" => {
                    // simula N tick di rotella (positivo = giù) sulla griglia
                    let n: i32 = arg.parse().unwrap_or(1);
                    if let Some(t) = app.current_tab() {
                        t.results.dev_scroll(n);
                    }
                }
                "new-ds" => {
                    let _ = WidgetExt::activate_action(&app.window, "win.new-datasource", None);
                }
                "quit" => {
                    app.window.close();
                    break;
                }
                other => eprintln!("devtools: comando sconosciuto {other}"),
            }
        }
    });
}

fn collect_popovers(w: &gtk::Widget, out: &mut Vec<gtk::Popover>) {
    let mut child = w.first_child();
    while let Some(c) = child {
        if let Some(p) = c.downcast_ref::<gtk::Popover>()
            && p.is_visible()
        {
            out.push(p.clone());
        }
        collect_popovers(&c, out);
        child = c.next_sibling();
    }
}

fn snap(w: &gtk::Window, path: &str) {
    snap_widget(w.upcast_ref(), path);
}

fn snap_widget(w: &gtk::Widget, path: &str) {
    let (width, height) = (w.width() as f32, w.height() as f32);
    let paintable = gtk::WidgetPaintable::new(Some(w));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, width as f64, height as f64);
    let Some(node) = snapshot.to_node() else {
        return;
    };
    let Some(renderer) = w.native().and_then(|n| n.renderer()) else {
        return;
    };
    let tex = renderer.render_texture(&node, Some(&graphene::Rect::new(0.0, 0.0, width, height)));
    if let Err(e) = tex.save_to_png(path) {
        eprintln!("devtools: salvataggio {path} fallito: {e}");
    }
}
