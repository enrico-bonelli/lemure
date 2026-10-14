//! Pannello sinistro: portafoglio dei datasource e albero dello schema
//! (datasource → [database] → tabelle → colonne), caricato in modo lazy
//! all'espansione dei nodi.

use super::app::App;
use super::util::{icon_button, popup_menu};
use gtk::prelude::*;
use gtk::{gdk, gio, glib, graphene};
use lemure::config::{DataSource, DbKind};
use lemure::db::{ColumnInfo, TableEntry};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    DataSource,
    Database,
    Table,
    View,
    Column,
    Info,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadState {
    NotLoaded,
    Loading,
    Loaded,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DsStatus {
    Disconnected,
    Connecting,
    Connected(String),
    Error(String),
}

pub struct Node {
    pub kind: NodeKind,
    pub ds_id: String,
    pub schema: String,
    pub name: String,
    label: RefCell<String>,
    detail: RefCell<String>,
    key: String,
    is_error: bool,
    status: RefCell<DsStatus>,
    children: RefCell<Option<gio::ListStore>>,
    load: Cell<LoadState>,
    row: RefCell<glib::WeakRef<gtk::TreeListRow>>,
    widgets: RefCell<
        Option<(
            glib::WeakRef<gtk::Image>,
            glib::WeakRef<gtk::Label>,
            glib::WeakRef<gtk::Label>,
        )>,
    >,
}

impl Node {
    fn new(kind: NodeKind, ds_id: &str, schema: &str, name: &str, label: &str) -> Rc<Node> {
        Rc::new(Node::raw(kind, ds_id, schema, name, label))
    }

    fn raw(kind: NodeKind, ds_id: &str, schema: &str, name: &str, label: &str) -> Node {
        Node {
            kind,
            ds_id: ds_id.into(),
            schema: schema.into(),
            name: name.into(),
            label: RefCell::new(label.into()),
            detail: RefCell::default(),
            key: String::new(),
            is_error: false,
            status: RefCell::new(DsStatus::Disconnected),
            children: RefCell::default(),
            load: Cell::new(LoadState::NotLoaded),
            row: RefCell::default(),
            widgets: RefCell::default(),
        }
    }

    fn info(text: &str, is_error: bool) -> Rc<Node> {
        Rc::new(Node {
            is_error,
            ..Node::raw(NodeKind::Info, "", "", "", text)
        })
    }

    fn column(ds_id: &str, schema: &str, c: &ColumnInfo) -> Rc<Node> {
        let mut detail = c.data_type.clone();
        if !c.nullable {
            detail.push_str(" not null");
        }
        Rc::new(Node {
            key: c.key.clone(),
            detail: RefCell::new(detail),
            ..Node::raw(NodeKind::Column, ds_id, schema, &c.name, &c.name)
        })
    }

    fn expandable(&self) -> bool {
        matches!(
            self.kind,
            NodeKind::DataSource | NodeKind::Database | NodeKind::Table | NodeKind::View
        )
    }

    fn children(&self) -> gio::ListStore {
        self.children
            .borrow_mut()
            .get_or_insert_with(gio::ListStore::new::<glib::BoxedAnyObject>)
            .clone()
    }

    fn set_children(&self, nodes: Vec<Rc<Node>>) {
        let objs: Vec<glib::BoxedAnyObject> =
            nodes.into_iter().map(glib::BoxedAnyObject::new).collect();
        let store = self.children();
        store.splice(0, store.n_items(), &objs);
    }

    fn icon_name(&self) -> &'static str {
        match self.kind {
            NodeKind::DataSource => match &*self.status.borrow() {
                DsStatus::Error(_) => "dialog-error-symbolic",
                DsStatus::Connecting => "content-loading-symbolic",
                _ => "network-server-symbolic",
            },
            NodeKind::Database => "drive-harddisk-symbolic",
            NodeKind::Table => "x-office-spreadsheet-symbolic",
            NodeKind::View => "view-list-symbolic",
            NodeKind::Column => {
                if self.key == "PRI" {
                    "dialog-password-symbolic"
                } else {
                    "insert-text-symbolic"
                }
            }
            NodeKind::Info => {
                if self.is_error {
                    "dialog-error-symbolic"
                } else {
                    "content-loading-symbolic"
                }
            }
        }
    }

    /// Aggiorna i widget attualmente associati al nodo (se visibili).
    fn refresh_view(&self) {
        let Some((img, label, detail)) = self.widgets.borrow().clone() else {
            return;
        };
        let (Some(img), Some(label), Some(detail)) =
            (img.upgrade(), label.upgrade(), detail.upgrade())
        else {
            return;
        };
        self.fill(&img, &label, &detail);
    }

    fn fill(&self, img: &gtk::Image, label: &gtk::Label, detail: &gtk::Label) {
        img.set_icon_name(Some(self.icon_name()));
        label.set_text(&self.label.borrow());
        for c in ["ds-connected", "ds-error", "pk-column", "error-text"] {
            img.remove_css_class(c);
            label.remove_css_class(c);
        }
        let mut tip = None;
        let mut det = self.detail.borrow().clone();
        match self.kind {
            NodeKind::DataSource => {
                label.add_css_class("ds-name");
                match &*self.status.borrow() {
                    DsStatus::Connected(v) => {
                        img.add_css_class("ds-connected");
                        tip = Some(format!("{}\nConnesso: {v}", det));
                        det = String::new();
                    }
                    DsStatus::Error(e) => {
                        img.add_css_class("ds-error");
                        tip = Some(format!("{}\nErrore: {e}", det));
                        det = String::new();
                    }
                    DsStatus::Connecting => {
                        tip = Some(det.clone());
                        det = "connessione…".into();
                    }
                    DsStatus::Disconnected => {
                        tip = Some(det.clone());
                        det = String::new();
                    }
                }
            }
            NodeKind::Column if self.key == "PRI" => img.add_css_class("pk-column"),
            NodeKind::Info if self.is_error => {
                label.add_css_class("error-text");
                tip = Some(self.label.borrow().clone());
            }
            _ => {}
        }
        if self.kind == NodeKind::Column && !self.key.is_empty() {
            det = format!("{det}  {}", self.key);
        }
        detail.set_text(&det);
        detail.set_visible(!det.is_empty());
        label.set_tooltip_text(tip.as_deref());
    }
}

fn node_of(obj: &glib::Object) -> Option<Rc<Node>> {
    let row = obj.downcast_ref::<gtk::TreeListRow>()?;
    let item = row.item()?.downcast::<glib::BoxedAnyObject>().ok()?;
    let n = item.borrow::<Rc<Node>>().clone();
    Some(n)
}

pub struct Sidebar {
    pub widget: gtk::Box,
    root: gio::ListStore,
    list: gtk::ListView,
    selection: gtk::SingleSelection,
    app: Weak<App>,
    ctx: RefCell<Option<Rc<Node>>>,
}

impl Sidebar {
    pub fn new(app: Weak<App>) -> Rc<Sidebar> {
        let root = gio::ListStore::new::<glib::BoxedAnyObject>();
        let tree = gtk::TreeListModel::new(root.clone(), false, false, |obj| {
            let item = obj.downcast_ref::<glib::BoxedAnyObject>()?;
            let node = item.borrow::<Rc<Node>>();
            node.expandable().then(|| node.children().upcast())
        });
        let selection = gtk::SingleSelection::new(Some(tree));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        let factory = gtk::SignalListItemFactory::new();
        let list = gtk::ListView::new(Some(selection.clone()), Some(factory.clone()));
        list.add_css_class("schema-tree");
        list.add_css_class("navigation-sidebar");

        let add = icon_button("list-add-symbolic", "Nuovo datasource");
        let edit = icon_button("document-edit-symbolic", "Modifica datasource");
        let del = icon_button("user-trash-symbolic", "Elimina datasource");
        let connect = icon_button("network-wired-symbolic", "Connetti");
        let disconnect = icon_button("network-offline-symbolic", "Disconnetti");
        let refresh = icon_button("view-refresh-symbolic", "Aggiorna");
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        toolbar.add_css_class("toolbar");
        let title = gtk::Label::new(Some("Datasource"));
        title.add_css_class("heading");
        title.set_hexpand(true);
        title.set_xalign(0.0);
        title.set_margin_start(6);
        toolbar.append(&title);
        for b in [&add, &edit, &del, &connect, &disconnect, &refresh] {
            toolbar.append(b);
        }

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_child(Some(&list));
        scroller.set_vexpand(true);
        scroller.set_hscrollbar_policy(gtk::PolicyType::Automatic);

        let empty = gtk::Label::new(Some(
            "Nessun datasource configurato.\nPremi + per aggiungerne uno.",
        ));
        empty.add_css_class("dim-label");
        empty.set_justify(gtk::Justification::Center);
        empty.set_vexpand(true);
        empty.set_valign(gtk::Align::Start);
        empty.set_margin_top(24);
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&toolbar);
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(&empty);
        widget.append(&scroller);
        {
            let (empty, scroller) = (empty.clone(), scroller.clone());
            root.connect_items_changed(move |r, _, _, _| {
                empty.set_visible(r.n_items() == 0);
                scroller.set_visible(r.n_items() > 0);
            });
        }
        empty.set_visible(true);
        scroller.set_visible(false);
        widget.set_width_request(200);

        let sb = Rc::new(Sidebar {
            widget,
            root,
            list,
            selection,
            app,
            ctx: RefCell::default(),
        });
        sb.setup_factory(&factory);
        sb.setup_actions();

        let bind = |b: &gtk::Button, action: &'static str| {
            let w = Rc::downgrade(&sb);
            b.connect_clicked(move |_| {
                if let Some(sb) = w.upgrade() {
                    *sb.ctx.borrow_mut() = sb.selected_node();
                    if action == "new-ds" {
                        let _ = sb.list.activate_action("side.new-ds", None);
                    } else if sb.ctx.borrow().is_some() {
                        let _ = sb.list.activate_action(&format!("side.{action}"), None);
                    }
                }
            });
        };
        bind(&add, "new-ds");
        bind(&edit, "edit");
        bind(&del, "delete");
        bind(&connect, "connect");
        bind(&disconnect, "disconnect");
        bind(&refresh, "refresh");

        let w = Rc::downgrade(&sb);
        sb.list.connect_activate(move |_, pos| {
            let Some(sb) = w.upgrade() else { return };
            let Some(row) = sb.selection.item(pos).and_downcast::<gtk::TreeListRow>() else {
                return;
            };
            let is_ds = node_of(row.upcast_ref()).is_some_and(|n| n.kind == NodeKind::DataSource);
            if is_ds {
                // il clic singolo ha già espanso il nodo; il doppio clic non lo richiude
                row.set_expanded(true);
            } else if row.is_expandable() {
                row.set_expanded(!row.is_expanded());
            } else if let Some(n) = node_of(row.upcast_ref())
                && n.kind == NodeKind::Column
                && let Some(app) = sb.app.upgrade()
            {
                app.insert_in_editor(&n.name);
            }
        });
        sb
    }

    fn selected_node(&self) -> Option<Rc<Node>> {
        self.selection.selected_item().and_then(|o| node_of(&o))
    }

    fn setup_factory(self: &Rc<Self>, factory: &gtk::SignalListItemFactory) {
        let weak = Rc::downgrade(self);
        factory.connect_setup(move |_, obj| {
            let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let expander = gtk::TreeExpander::new();
            let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let img = gtk::Image::new();
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            let detail = gtk::Label::new(None);
            detail.add_css_class("node-detail");
            detail.set_xalign(0.0);
            hbox.append(&img);
            hbox.append(&label);
            hbox.append(&detail);
            expander.set_child(Some(&hbox));
            item.set_child(Some(&expander));

            let click = gtk::GestureClick::new();
            click.set_button(gdk::BUTTON_SECONDARY);
            let weak = weak.clone();
            let item_weak = item.downgrade();
            let exp_weak = expander.downgrade();
            click.connect_pressed(move |_, _, x, y| {
                let (Some(sb), Some(item), Some(exp)) =
                    (weak.upgrade(), item_weak.upgrade(), exp_weak.upgrade())
                else {
                    return;
                };
                let Some(node) = item.item().and_then(|o| node_of(&o)) else {
                    return;
                };
                sb.selection.set_selected(item.position());
                let p = exp
                    .compute_point(&sb.list, &graphene::Point::new(x as f32, y as f32))
                    .unwrap_or_else(|| graphene::Point::new(0.0, 0.0));
                sb.show_context_menu(node, p.x() as f64, p.y() as f64);
            });
            expander.add_controller(click);

            // Clic singolo su un datasource: connessione ed espansione.
            let primary = gtk::GestureClick::new();
            primary.set_button(gdk::BUTTON_PRIMARY);
            let item_weak = item.downgrade();
            primary.connect_released(move |_, n, _, _| {
                let Some(item) = item_weak.upgrade() else {
                    return;
                };
                let Some(row) = item.item().and_downcast::<gtk::TreeListRow>() else {
                    return;
                };
                let is_ds =
                    node_of(row.upcast_ref()).is_some_and(|n| n.kind == NodeKind::DataSource);
                if is_ds && n == 1 && !row.is_expanded() {
                    row.set_expanded(true);
                }
            });
            expander.add_controller(primary);
        });

        let weak = Rc::downgrade(self);
        factory.connect_bind(move |_, obj| {
            let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let Some(row) = item.item().and_downcast::<gtk::TreeListRow>() else {
                return;
            };
            let Some(node) = node_of(row.upcast_ref()) else {
                return;
            };
            let expander = item.child().and_downcast::<gtk::TreeExpander>().unwrap();
            expander.set_list_row(Some(&row));
            let hbox = expander.child().and_downcast::<gtk::Box>().unwrap();
            let img = hbox.first_child().and_downcast::<gtk::Image>().unwrap();
            let label = img.next_sibling().and_downcast::<gtk::Label>().unwrap();
            let detail = label.next_sibling().and_downcast::<gtk::Label>().unwrap();
            node.fill(&img, &label, &detail);
            *node.widgets.borrow_mut() =
                Some((img.downgrade(), label.downgrade(), detail.downgrade()));

            // Caricamento lazy dei figli all'espansione. Gli oggetti riga possono
            // essere ricreati da GTK, quindi si collega il segnale a ogni nuova riga.
            let known = node.row.borrow().upgrade();
            if known.as_ref() != Some(&row) {
                *node.row.borrow_mut() = row.downgrade();
                let weak = weak.clone();
                let node_w = Rc::downgrade(&node);
                row.connect_expanded_notify(move |r| {
                    if let (Some(sb), Some(n)) = (weak.upgrade(), node_w.upgrade())
                        && r.is_expanded()
                        && n.load.get() == LoadState::NotLoaded
                    {
                        sb.load_children(n);
                    }
                });
            }
        });
        factory.connect_unbind(|_, obj| {
            let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
            if let Some(node) = item.item().and_then(|o| node_of(&o)) {
                node.widgets.borrow_mut().take();
            }
        });
    }

    fn show_context_menu(&self, node: Rc<Node>, x: f64, y: f64) {
        let menu = gio::Menu::new();
        match node.kind {
            NodeKind::DataSource => {
                let connected = matches!(*node.status.borrow(), DsStatus::Connected(_));
                let s1 = gio::Menu::new();
                if connected {
                    s1.append(Some("Disconnetti"), Some("side.disconnect"));
                } else {
                    s1.append(Some("Connetti"), Some("side.connect"));
                }
                s1.append(Some("Nuova query"), Some("side.new-query"));
                s1.append(Some("Aggiorna"), Some("side.refresh"));
                menu.append_section(None, &s1);
                let s2 = gio::Menu::new();
                s2.append(Some("Modifica…"), Some("side.edit"));
                s2.append(Some("Duplica"), Some("side.duplicate"));
                s2.append(Some("Elimina"), Some("side.delete"));
                menu.append_section(None, &s2);
            }
            NodeKind::Database => {
                menu.append(
                    Some("Nuova query su questo database"),
                    Some("side.new-query"),
                );
                menu.append(Some("Aggiorna"), Some("side.refresh"));
                menu.append(Some("Copia nome"), Some("side.copy-name"));
            }
            NodeKind::Table | NodeKind::View => {
                let s1 = gio::Menu::new();
                s1.append(Some("Analizza tabella"), Some("side.analyze"));
                s1.append(Some("Mostra dati (SELECT *)"), Some("side.select-all"));
                s1.append(Some("Conta righe"), Some("side.count"));
                menu.append_section(None, &s1);
                let s2 = gio::Menu::new();
                s2.append(Some("Copia nome"), Some("side.copy-name"));
                s2.append(Some("Inserisci nell'editor"), Some("side.insert-name"));
                s2.append(Some("Aggiorna colonne"), Some("side.refresh"));
                menu.append_section(None, &s2);
            }
            NodeKind::Column => {
                menu.append(Some("Copia nome"), Some("side.copy-name"));
                menu.append(Some("Inserisci nell'editor"), Some("side.insert-name"));
            }
            NodeKind::Info => return,
        }
        *self.ctx.borrow_mut() = Some(node);
        popup_menu(&self.list, &menu, x, y);
    }

    fn setup_actions(self: &Rc<Self>) {
        let group = gio::SimpleActionGroup::new();
        let add = |name: &str, f: fn(&Rc<Sidebar>, &Rc<App>, Option<Rc<Node>>)| {
            let a = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            a.connect_activate(move |_, _| {
                let Some(sb) = weak.upgrade() else { return };
                let Some(app) = sb.app.upgrade() else { return };
                let node = sb.ctx.borrow().clone();
                f(&sb, &app, node);
            });
            group.add_action(&a);
        };
        add("new-ds", |_, app, _| app.edit_datasource(None));
        add("edit", |sb, app, n| {
            if let Some(ds) = n.and_then(|n| sb.ds_of(&n)) {
                app.edit_datasource(Some(ds));
            }
        });
        add("duplicate", |sb, app, n| {
            if let Some(mut ds) = n.and_then(|n| sb.ds_of(&n)) {
                ds.id = DataSource::new_id();
                ds.name = format!("{} (copia)", ds.name);
                app.save_datasource(ds, true);
            }
        });
        add("delete", |sb, app, n| {
            if let Some(ds) = n.and_then(|n| sb.ds_of(&n)) {
                app.delete_datasource(ds);
            }
        });
        add("connect", |sb, _, n| {
            if let Some(n) = n {
                let ds_node = sb.ds_node(&n.ds_id);
                if let Some(row) = ds_node.and_then(|d| d.row.borrow().upgrade()) {
                    row.set_expanded(true);
                }
            }
        });
        add("disconnect", |_, app, n| {
            if let Some(n) = n {
                app.disconnect(&n.ds_id);
            }
        });
        add("refresh", |sb, app, n| {
            if let Some(n) = n {
                app.load_meta(&n.ds_id);
                let target = if n.kind == NodeKind::Column {
                    sb.ds_node(&n.ds_id)
                } else {
                    Some(n)
                };
                if let Some(t) = target {
                    sb.reload(&t);
                }
            }
        });
        add("new-query", |_, app, n| {
            if let Some(n) = n {
                let sql = if n.kind == NodeKind::Database
                    && app.ds_kind(&n.ds_id) == Some(DbKind::MySql)
                {
                    format!(
                        "USE {};\n\n",
                        lemure::sqlutil::quote_ident(&n.name, lemure::sqlutil::Dialect::MySql)
                    )
                } else if n.kind == NodeKind::Database {
                    format!(
                        "SET search_path TO {};\n\n",
                        lemure::sqlutil::quote_ident(&n.name, lemure::sqlutil::Dialect::Postgres)
                    )
                } else {
                    String::new()
                };
                app.new_tab(Some(n.ds_id.clone()), &sql, None);
            }
        });
        add("analyze", |_, app, n| {
            if let Some(n) = n {
                super::dialogs::show_analyze(
                    app,
                    n.ds_id.clone(),
                    n.schema.clone(),
                    n.name.clone(),
                );
            }
        });
        add("select-all", |_, app, n| {
            if let Some(n) = n {
                let sql = app.select_sql(&n.ds_id, &n.schema, &n.name, false);
                let tab = app.new_tab(Some(n.ds_id.clone()), &sql, Some(n.name.clone()));
                tab.run(true);
            }
        });
        add("count", |_, app, n| {
            if let Some(n) = n {
                let sql = app.select_sql(&n.ds_id, &n.schema, &n.name, true);
                let tab = app.new_tab(
                    Some(n.ds_id.clone()),
                    &sql,
                    Some(format!("count {}", n.name)),
                );
                tab.run(true);
            }
        });
        add("copy-name", |sb, _, n| {
            if let Some(n) = n {
                super::util::copy_to_clipboard(&sb.list, &n.name);
            }
        });
        add("insert-name", |_, app, n| {
            if let Some(n) = n {
                app.insert_in_editor(&n.name);
            }
        });
        self.list.insert_action_group("side", Some(&group));
    }

    fn ds_of(&self, n: &Node) -> Option<DataSource> {
        self.app.upgrade()?.datasource(&n.ds_id)
    }

    fn ds_nodes(&self) -> Vec<Rc<Node>> {
        (0..self.root.n_items())
            .filter_map(|i| self.root.item(i).and_downcast::<glib::BoxedAnyObject>())
            .map(|o| o.borrow::<Rc<Node>>().clone())
            .collect()
    }

    fn ds_node(&self, ds_id: &str) -> Option<Rc<Node>> {
        self.ds_nodes().into_iter().find(|n| n.ds_id == ds_id)
    }

    /// Ricostruisce l'elenco dei datasource mantenendo lo stato dei nodi esistenti.
    pub fn set_datasources(&self, list: &[DataSource]) {
        let existing = self.ds_nodes();
        let mut sorted: Vec<&DataSource> = list.iter().collect();
        sorted.sort_by_cached_key(|d| d.name.to_lowercase());
        let objs: Vec<glib::BoxedAnyObject> = sorted
            .iter()
            .map(|ds| {
                let node = existing
                    .iter()
                    .find(|n| n.ds_id == ds.id)
                    .cloned()
                    .unwrap_or_else(|| {
                        Node::new(
                            NodeKind::DataSource,
                            &ds.id,
                            &ds.database,
                            &ds.name,
                            &ds.name,
                        )
                    });
                *node.label.borrow_mut() = ds.name.clone();
                *node.detail.borrow_mut() = ds.summary();
                node.refresh_view();
                glib::BoxedAnyObject::new(node)
            })
            .collect();
        // Aggiorna solo se l'insieme o l'ordine è cambiato, per non chiudere i nodi espansi.
        let same = existing.len() == objs.len()
            && existing
                .iter()
                .zip(&objs)
                .all(|(a, b)| Rc::ptr_eq(a, &b.borrow::<Rc<Node>>()));
        if !same {
            self.root.splice(0, self.root.n_items(), &objs);
        }
    }

    pub fn set_status(&self, ds_id: &str, status: DsStatus) {
        if let Some(n) = self.ds_node(ds_id) {
            *n.status.borrow_mut() = status;
            n.refresh_view();
        }
    }

    /// Chiude il nodo e ne svuota i figli (es. dopo una disconnessione).
    pub fn reset(&self, ds_id: &str) {
        if let Some(n) = self.ds_node(ds_id) {
            if let Some(row) = n.row.borrow().upgrade() {
                row.set_expanded(false);
            }
            n.load.set(LoadState::NotLoaded);
            n.set_children(Vec::new());
            *n.status.borrow_mut() = DsStatus::Disconnected;
            n.refresh_view();
        }
    }

    /// Espande il primo nodo visibile con l'etichetta indicata (usato dai devtools).
    #[cfg(feature = "devtools")]
    pub fn dev_expand(&self, label: &str) -> bool {
        for i in 0..self.selection.n_items() {
            let Some(row) = self.selection.item(i).and_downcast::<gtk::TreeListRow>() else {
                continue;
            };
            if node_of(row.upcast_ref()).is_some_and(|n| *n.label.borrow() == label) {
                row.set_expanded(true);
                self.selection.set_selected(i);
                return true;
            }
        }
        false
    }

    /// Apre il menu contestuale del primo nodo con l'etichetta indicata (devtools).
    #[cfg(feature = "devtools")]
    pub fn dev_context(&self, label: &str) {
        for i in 0..self.selection.n_items() {
            let Some(row) = self.selection.item(i).and_downcast::<gtk::TreeListRow>() else {
                continue;
            };
            if let Some(n) = node_of(row.upcast_ref()).filter(|n| *n.label.borrow() == label) {
                self.selection.set_selected(i);
                self.show_context_menu(n, 60.0, 40.0 + 26.0 * i as f64);
                return;
            }
        }
    }

    /// Ricarica il contenuto del datasource se era già stato caricato.
    pub fn refresh_loaded(self: &Rc<Self>, ds_id: &str) {
        if let Some(n) = self.ds_node(ds_id)
            && n.load.get() == LoadState::Loaded
        {
            self.reload(&n);
        }
    }

    fn reload(self: &Rc<Self>, node: &Rc<Node>) {
        if node.load.get() == LoadState::Loading {
            return;
        }
        node.load.set(LoadState::NotLoaded);
        let expanded = node.row.borrow().upgrade().is_some_and(|r| r.is_expanded());
        if expanded {
            self.load_children(node.clone());
        } else if let Some(row) = node.row.borrow().upgrade() {
            row.set_expanded(true);
        }
    }

    fn load_children(self: &Rc<Self>, node: Rc<Node>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        node.load.set(LoadState::Loading);
        if node.children().n_items() == 0 {
            node.set_children(vec![Node::info("Caricamento…", false)]);
        }
        glib::spawn_future_local(async move {
            let result = Self::fetch_children(&app, &node).await;
            match result {
                Ok(children) => {
                    if children.is_empty() {
                        node.set_children(vec![Node::info("(vuoto)", false)]);
                    } else {
                        node.set_children(children);
                    }
                    node.load.set(LoadState::Loaded);
                }
                Err(e) => {
                    node.set_children(vec![Node::info(&first_line(&e), true)]);
                    node.load.set(LoadState::NotLoaded);
                }
            }
        });
    }

    async fn fetch_children(app: &Rc<App>, node: &Rc<Node>) -> Result<Vec<Rc<Node>>, String> {
        let ds = app
            .datasource(&node.ds_id)
            .ok_or("Datasource non trovato")?;
        let worker = app.ensure_connected(&node.ds_id).await?;
        let ds_id = node.ds_id.clone();
        match node.kind {
            NodeKind::DataSource if ds.kind == DbKind::MySql && ds.database.trim().is_empty() => {
                let dbs = worker.run(|c| c.list_databases()).await??;
                Ok(dbs
                    .iter()
                    .map(|d| Node::new(NodeKind::Database, &ds_id, d, d, d))
                    .collect())
            }
            NodeKind::DataSource | NodeKind::Database => {
                let schema = if node.kind == NodeKind::Database {
                    node.name.clone()
                } else if ds.kind == DbKind::MySql {
                    ds.database.trim().to_string()
                } else {
                    String::new()
                };
                let tables = worker.run(move |c| c.list_tables(&schema)).await??;
                Ok(table_nodes(&ds_id, ds.kind, tables))
            }
            NodeKind::Table | NodeKind::View => {
                let (schema, table) = (node.schema.clone(), node.name.clone());
                let cols = worker
                    .run(move |c| c.list_columns(&schema, &table))
                    .await??;
                Ok(cols
                    .iter()
                    .map(|c| Node::column(&ds_id, &node.schema, c))
                    .collect())
            }
            _ => Ok(Vec::new()),
        }
    }
}

fn table_nodes(ds_id: &str, kind: DbKind, tables: Vec<TableEntry>) -> Vec<Rc<Node>> {
    // In PostgreSQL le tabelle fuori da "public" mostrano lo schema come prefisso.
    let mut nodes: Vec<(String, Rc<Node>)> = tables
        .into_iter()
        .map(|t| {
            let label = if kind == DbKind::Postgres && t.schema != "public" {
                format!("{}.{}", t.schema, t.name)
            } else {
                t.name.clone()
            };
            let k = if t.is_view {
                NodeKind::View
            } else {
                NodeKind::Table
            };
            (
                label.to_lowercase(),
                Node::new(k, ds_id, &t.schema, &t.name, &label),
            )
        })
        .collect();
    nodes.sort_by(|a, b| a.0.cmp(&b.0));
    nodes.into_iter().map(|(_, n)| n).collect()
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}
