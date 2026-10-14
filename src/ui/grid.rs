//! Griglia dati basata su `GtkColumnView` (virtualizzata: vengono creati solo
//! i widget delle righe visibili). Supporta colonne ridimensionabili, copia
//! della singola cella, copia della riga con separatore `|`, selezione
//! multipla e visualizzazione del valore completo con doppio clic.

use super::util::{cell_display, copy_to_clipboard, popup_menu};
use gtk::prelude::*;
use gtk::{gdk, gio, glib, graphene};
use lemure::db::RowSet;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

pub struct Inner {
    pub scroller: gtk::ScrolledWindow,
    view: gtk::ColumnView,
    selection: gtk::MultiSelection,
    store: gio::ListStore,
    data: RefCell<Rc<RowSet>>,
    offset: Cell<usize>,
    columns: RefCell<Vec<gtk::ColumnViewColumn>>,
    current: Cell<Option<(u32, usize)>>,
    current_label: RefCell<glib::WeakRef<gtk::Label>>,
    font_size: Cell<f64>,
}

#[derive(Clone)]
pub struct DataGrid(Rc<Inner>);

impl std::ops::Deref for DataGrid {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

const NULL_TEXT: &str = "NULL";

fn is_numeric_type(t: &str) -> bool {
    let t = t.to_ascii_uppercase();
    [
        "INT", "DECIMAL", "NUMERIC", "FLOAT", "DOUBLE", "REAL", "SERIAL", "MONEY",
    ]
    .iter()
    .any(|k| t.contains(k))
        && !t.contains("INTERVAL")
        && !t.contains("POINT")
}

impl DataGrid {
    pub fn new(font_size: f64) -> DataGrid {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let selection = gtk::MultiSelection::new(Some(store.clone()));
        let view = gtk::ColumnView::new(Some(selection.clone()));
        view.set_show_column_separators(true);
        view.set_show_row_separators(true);
        view.set_reorderable(true);
        view.add_css_class("data-grid");
        let scroller = gtk::ScrolledWindow::new();
        scroller.set_child(Some(&view));
        scroller.set_vexpand(true);
        scroller.set_hexpand(true);

        let grid = DataGrid(Rc::new(Inner {
            scroller,
            view,
            selection,
            store,
            data: RefCell::new(Rc::new(RowSet::default())),
            offset: Cell::new(0),
            columns: RefCell::default(),
            current: Cell::new(None),
            current_label: RefCell::default(),
            font_size: Cell::new(font_size),
        }));
        grid.install_actions();
        grid.install_keys();
        grid
    }

    fn weak(&self) -> Weak<Inner> {
        Rc::downgrade(&self.0)
    }

    fn from_weak(w: &Weak<Inner>) -> Option<DataGrid> {
        w.upgrade().map(DataGrid)
    }

    pub fn rowset(&self) -> Rc<RowSet> {
        self.data.borrow().clone()
    }

    pub fn row_count(&self) -> usize {
        self.data.borrow().rows.len()
    }

    pub fn set_font_size(&self, size: f64) {
        self.font_size.set(size);
    }

    /// Imposta i dati da mostrare. Se le colonne sono le stesse della pagina
    /// precedente vengono mantenute (e con esse le larghezze scelte dall'utente).
    pub fn set_data(&self, rs: Rc<RowSet>, offset: usize) {
        let same_columns = {
            let old = self.data.borrow();
            !self.columns.borrow().is_empty()
                && old.columns.len() == rs.columns.len()
                && old
                    .columns
                    .iter()
                    .zip(&rs.columns)
                    .all(|(a, b)| a.name == b.name)
        };
        self.current.set(None);
        *self.data.borrow_mut() = rs.clone();
        self.offset.set(offset);
        if !same_columns {
            self.rebuild_columns(&rs);
        }
        let objs: Vec<glib::BoxedAnyObject> = (0..rs.rows.len() as u32)
            .map(glib::BoxedAnyObject::new)
            .collect();
        self.store.splice(0, self.store.n_items(), &objs);
        self.selection.unselect_all();
        self.scroller.vadjustment().set_value(0.0);
        if !same_columns {
            self.scroller.hadjustment().set_value(0.0);
        }
    }

    fn char_width(&self) -> f64 {
        let layout = self.view.create_pango_layout(Some("0123456789"));
        let desc = gtk::pango::FontDescription::from_string(&format!(
            "Monospace {}",
            self.font_size.get()
        ));
        layout.set_font_description(Some(&desc));
        layout.pixel_size().0 as f64 / 10.0
    }

    fn rebuild_columns(&self, rs: &RowSet) {
        for c in self.columns.borrow_mut().drain(..) {
            self.view.remove_column(&c);
        }
        if rs.columns.is_empty() {
            return;
        }
        let cw = self.char_width();
        let mut cols = Vec::with_capacity(rs.columns.len() + 1);

        // Colonna con il numero di riga
        let digits = (self.offset.get() + rs.rows.len().max(1))
            .to_string()
            .len()
            .max(3);
        let num_col = gtk::ColumnViewColumn::new(Some("#"), Some(self.row_number_factory()));
        num_col.set_fixed_width((digits as f64 * cw + 18.0) as i32);
        num_col.set_resizable(true);
        cols.push(num_col);

        let sample = rs.rows.len().min(300);
        for (ci, meta) in rs.columns.iter().enumerate() {
            let mut chars = meta.name.chars().count() + 2;
            for row in &rs.rows[..sample] {
                let n = match &row[ci] {
                    Some(s) => s.len().min(60), // byte, approssimazione veloce
                    None => NULL_TEXT.len(),
                };
                chars = chars.max(n);
            }
            let width = (chars as f64 * cw + 20.0).clamp(60.0, 520.0);
            let numeric = is_numeric_type(&meta.type_name);
            let col =
                gtk::ColumnViewColumn::new(Some(&meta.name), Some(self.cell_factory(ci, numeric)));
            col.set_resizable(true);
            col.set_fixed_width(width as i32);
            let menu = gio::Menu::new();
            if !meta.type_name.is_empty() {
                menu.append(Some(&format!("Tipo: {}", meta.type_name)), None);
            }
            let item = gio::MenuItem::new(Some("Copia nome colonna"), None);
            item.set_action_and_target_value(
                Some("grid.copy-column-name"),
                Some(&(ci as u32).to_variant()),
            );
            menu.append_item(&item);
            let item = gio::MenuItem::new(Some("Copia valori della colonna"), None);
            item.set_action_and_target_value(
                Some("grid.copy-column-values"),
                Some(&(ci as u32).to_variant()),
            );
            menu.append_item(&item);
            col.set_header_menu(Some(&menu));
            cols.push(col);
        }
        for c in &cols {
            self.view.append_column(c);
        }
        *self.columns.borrow_mut() = cols;
    }

    fn row_number_factory(&self) -> gtk::SignalListItemFactory {
        let f = gtk::SignalListItemFactory::new();
        f.connect_setup(|_, obj| {
            let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let label = gtk::Label::new(None);
            label.set_xalign(1.0);
            label.add_css_class("row-number");
            item.set_child(Some(&label));
        });
        let weak = self.weak();
        f.connect_bind(move |_, obj| {
            let Some(g) = DataGrid::from_weak(&weak) else {
                return;
            };
            let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let label = item.child().and_downcast::<gtk::Label>().unwrap();
            label.set_text(&(g.offset.get() + item.position() as usize + 1).to_string());
        });
        f
    }

    fn cell_factory(&self, ci: usize, numeric: bool) -> gtk::SignalListItemFactory {
        let f = gtk::SignalListItemFactory::new();
        let weak = self.weak();
        f.connect_setup(move |_, obj| {
            let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let label = gtk::Label::new(None);
            label.set_xalign(if numeric { 1.0 } else { 0.0 });
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            label.set_single_line_mode(true);
            label.add_css_class("cell");
            let click = gtk::GestureClick::new();
            click.set_button(0);
            let weak = weak.clone();
            let item_weak = item.downgrade();
            let label_weak = label.downgrade();
            click.connect_pressed(move |gesture, n_press, x, y| {
                let (Some(g), Some(item), Some(label)) = (
                    DataGrid::from_weak(&weak),
                    item_weak.upgrade(),
                    label_weak.upgrade(),
                ) else {
                    return;
                };
                let row = item.position();
                g.set_current_cell(row, ci, &label);
                match gesture.current_button() {
                    gdk::BUTTON_SECONDARY => {
                        if !g.selection.is_selected(row) {
                            g.selection.select_item(row, true);
                        }
                        let p = label
                            .compute_point(&g.view, &graphene::Point::new(x as f32, y as f32))
                            .unwrap_or_else(|| graphene::Point::new(0.0, 0.0));
                        g.show_context_menu(p.x() as f64, p.y() as f64);
                    }
                    gdk::BUTTON_PRIMARY if n_press == 2 => g.show_value_viewer(row, ci),
                    _ => {}
                }
            });
            label.add_controller(click);
            item.set_child(Some(&label));
        });
        let weak = self.weak();
        f.connect_bind(move |_, obj| {
            let Some(g) = DataGrid::from_weak(&weak) else {
                return;
            };
            let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let label = item.child().and_downcast::<gtk::Label>().unwrap();
            let pos = item.position();
            let data = g.data.borrow();
            match data.rows.get(pos as usize).and_then(|r| r.get(ci)) {
                Some(Some(v)) => {
                    label.set_text(&cell_display(v));
                    label.remove_css_class("null-value");
                }
                _ => {
                    label.set_text(NULL_TEXT);
                    label.add_css_class("null-value");
                }
            }
            if g.current.get() == Some((pos, ci)) {
                label.add_css_class("current-cell");
            } else {
                label.remove_css_class("current-cell");
            }
        });
        f
    }

    /// Selezione di riga e cella senza interazione (usato dai devtools).
    #[cfg(feature = "devtools")]
    pub fn dev_select(&self, row: u32, col: usize, extend: bool) {
        self.selection.select_item(row, !extend);
        self.current.set(Some((row, col)));
    }

    #[cfg(feature = "devtools")]
    pub fn dev_context(&self) {
        self.show_context_menu(120.0, 80.0);
    }

    #[cfg(feature = "devtools")]
    pub fn dev_action(&self, name: &str) {
        let _ = WidgetExt::activate_action(&self.view, name, None);
    }

    fn set_current_cell(&self, row: u32, col: usize, label: &gtk::Label) {
        if let Some(prev) = self.current_label.borrow().upgrade() {
            prev.remove_css_class("current-cell");
        }
        label.add_css_class("current-cell");
        self.current.set(Some((row, col)));
        self.current_label.borrow().set(Some(label));
    }

    fn show_context_menu(&self, x: f64, y: f64) {
        let item = |label: &str, action: &str, accel: Option<&str>| {
            let i = gio::MenuItem::new(Some(label), Some(action));
            if let Some(a) = accel {
                i.set_attribute_value("accel", Some(&a.to_variant()));
            }
            i
        };
        let menu = gio::Menu::new();
        menu.append_item(&item(
            "Copia valore cella",
            "grid.copy-cell",
            Some("<Control>c"),
        ));
        menu.append_item(&item(
            "Copia riga (valori separati da |)",
            "grid.copy-rows",
            Some("<Control><Shift>c"),
        ));
        menu.append_item(&item(
            "Copia riga con intestazioni",
            "grid.copy-rows-header",
            None,
        ));
        let s2 = gio::Menu::new();
        s2.append_item(&item("Mostra valore completo…", "grid.view-value", None));
        s2.append_item(&item(
            "Seleziona tutto",
            "grid.select-all",
            Some("<Control>a"),
        ));
        menu.append_section(None, &s2);
        popup_menu(&self.view, &menu, x, y);
    }

    fn selected_rows(&self) -> Vec<usize> {
        let set = self.selection.selection();
        let mut out = Vec::with_capacity(set.size() as usize);
        if let Some((iter, first)) = gtk::BitsetIter::init_first(&set) {
            out.push(first as usize);
            for v in iter {
                out.push(v as usize);
            }
        }
        if out.is_empty()
            && let Some((r, _)) = self.current.get()
        {
            out.push(r as usize);
        }
        out
    }

    /// Cella attiva: quella cliccata, oppure la stessa colonna nella riga selezionata.
    fn current_value(&self) -> Option<Option<String>> {
        let (row, col) = self.current.get()?;
        let rows = self.selected_rows();
        let row = if rows.len() == 1 {
            rows[0]
        } else {
            row as usize
        };
        self.data
            .borrow()
            .rows
            .get(row)
            .and_then(|r| r.get(col))
            .cloned()
    }

    fn rows_text(&self, with_header: bool) -> String {
        let data = self.data.borrow();
        let mut out = String::new();
        if with_header {
            let names: Vec<&str> = data.columns.iter().map(|c| c.name.as_str()).collect();
            out.push_str(&names.join("|"));
            out.push('\n');
        }
        for r in self.selected_rows() {
            if let Some(row) = data.rows.get(r) {
                let vals: Vec<&str> = row
                    .iter()
                    .map(|v| v.as_deref().unwrap_or(NULL_TEXT))
                    .collect();
                out.push_str(&vals.join("|"));
                out.push('\n');
            }
        }
        if out.ends_with('\n') {
            out.pop();
        }
        out
    }

    fn install_actions(&self) {
        let group = gio::SimpleActionGroup::new();
        let add = |name: &str,
                   f: Box<dyn Fn(&DataGrid, Option<&glib::Variant>)>,
                   param: Option<&glib::VariantTy>| {
            let a = gio::SimpleAction::new(name, param);
            let weak = self.weak();
            a.connect_activate(move |_, p| {
                if let Some(g) = DataGrid::from_weak(&weak) {
                    f(&g, p);
                }
            });
            group.add_action(&a);
        };
        add(
            "copy-cell",
            Box::new(|g, _| {
                if let Some(v) = g.current_value() {
                    copy_to_clipboard(&g.view, v.as_deref().unwrap_or(NULL_TEXT));
                }
            }),
            None,
        );
        add(
            "copy-rows",
            Box::new(|g, _| copy_to_clipboard(&g.view, &g.rows_text(false))),
            None,
        );
        add(
            "copy-rows-header",
            Box::new(|g, _| copy_to_clipboard(&g.view, &g.rows_text(true))),
            None,
        );
        add(
            "view-value",
            Box::new(|g, _| {
                if let Some((r, c)) = g.current.get() {
                    g.show_value_viewer(r, c);
                }
            }),
            None,
        );
        add(
            "select-all",
            Box::new(|g, _| {
                g.selection.select_all();
            }),
            None,
        );
        add(
            "copy-column-name",
            Box::new(|g, p| {
                let ci = p.and_then(|v| v.get::<u32>()).unwrap_or(0) as usize;
                if let Some(c) = g.data.borrow().columns.get(ci) {
                    copy_to_clipboard(&g.view, &c.name);
                }
            }),
            Some(glib::VariantTy::UINT32),
        );
        add(
            "copy-column-values",
            Box::new(|g, p| {
                let ci = p.and_then(|v| v.get::<u32>()).unwrap_or(0) as usize;
                let data = g.data.borrow();
                let vals: Vec<&str> = data
                    .rows
                    .iter()
                    .map(|r| r.get(ci).and_then(|v| v.as_deref()).unwrap_or(NULL_TEXT))
                    .collect();
                copy_to_clipboard(&g.view, &vals.join("\n"));
            }),
            Some(glib::VariantTy::UINT32),
        );
        self.view.insert_action_group("grid", Some(&group));
    }

    fn install_keys(&self) {
        let keys = gtk::EventControllerKey::new();
        let weak = self.weak();
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(g) = DataGrid::from_weak(&weak) else {
                return glib::Propagation::Proceed;
            };
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            if ctrl && matches!(key, gdk::Key::c | gdk::Key::C) {
                let action = if shift || g.current.get().is_none() {
                    "grid.copy-rows"
                } else {
                    "grid.copy-cell"
                };
                let _ = g.view.activate_action(action, None);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.view.add_controller(keys);
    }

    /// Finestra con il valore completo della cella (utile per testi lunghi o JSON).
    fn show_value_viewer(&self, row: u32, col: usize) {
        let data = self.data.borrow();
        let Some(value) = data
            .rows
            .get(row as usize)
            .and_then(|r| r.get(col))
            .cloned()
        else {
            return;
        };
        let col_name = data
            .columns
            .get(col)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        drop(data);
        let win = gtk::Window::builder()
            .title(format!(
                "Valore di {col_name} (riga {})",
                self.offset.get() + row as usize + 1
            ))
            .default_width(640)
            .default_height(420)
            .modal(true)
            .build();
        if let Some(parent) = super::util::root_window(&self.view) {
            win.set_transient_for(Some(&parent));
        }
        let text = gtk::TextView::new();
        text.set_monospace(true);
        text.set_wrap_mode(gtk::WrapMode::WordChar);
        text.set_editable(false);
        text.set_left_margin(8);
        text.set_top_margin(8);
        let shown = value.clone().unwrap_or_else(|| NULL_TEXT.into());
        // Il JSON viene indentato per leggibilità.
        let pretty = serde_json::from_str::<serde_json::Value>(&shown)
            .ok()
            .filter(|v| v.is_object() || v.is_array())
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or(shown.clone());
        text.buffer().set_text(&pretty);
        let sc = gtk::ScrolledWindow::new();
        sc.set_child(Some(&text));
        sc.set_vexpand(true);
        let copy = gtk::Button::with_label("Copia");
        let close = gtk::Button::with_label("Chiudi");
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.set_halign(gtk::Align::End);
        bar.set_margin_top(6);
        bar.set_margin_bottom(6);
        bar.set_margin_end(6);
        let info = gtk::Label::new(Some(&format!("{} caratteri", shown.chars().count())));
        info.add_css_class("dim-label");
        info.set_hexpand(true);
        info.set_xalign(0.0);
        info.set_margin_start(8);
        let outer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        outer.append(&info);
        bar.append(&copy);
        bar.append(&close);
        outer.append(&bar);
        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
        vbox.append(&sc);
        vbox.append(&outer);
        win.set_child(Some(&vbox));
        let w = win.clone();
        copy.connect_clicked(move |b| {
            copy_to_clipboard(b, &shown);
            w.close();
        });
        let w = win.clone();
        close.connect_clicked(move |_| w.close());
        let esc = gtk::EventControllerKey::new();
        let w = win.clone();
        esc.connect_key_pressed(move |_, k, _, _| {
            if k == gdk::Key::Escape {
                w.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        win.add_controller(esc);
        win.present();
    }
}
