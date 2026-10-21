//! Pannello dei risultati di una scheda query: barra di paginazione, griglia
//! dati, log dei messaggi e pulsante di esportazione.

use super::grid::DataGrid;
use super::util::{fmt_count, icon_button, labeled_button};
use gtk::glib;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub const PAGE_SIZE: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageNav {
    First,
    Prev,
    Next,
    Last,
    Goto(usize),
}

pub struct ResultsView {
    pub widget: gtk::Box,
    pager: gtk::Box,
    pub grid: DataGrid,
    first: gtk::Button,
    prev: gtk::Button,
    next: gtk::Button,
    last: gtk::Button,
    page_entry: gtk::Entry,
    page_total: gtk::Label,
    info: gtk::Label,
    spinner: gtk::Spinner,
    pub export: gtk::Button,
    pub auto_scroll: gtk::ToggleButton,
    stack: gtk::Stack,
    log: gtk::TextView,
    on_nav: RefCell<Option<Rc<dyn Fn(PageNav)>>>,
    on_export: RefCell<Option<Rc<dyn Fn()>>>,
    /// tick di scroll oltre il bordo, usati per cambiare pagina scorrendo
    overscroll: Cell<f64>,
    has_next: Cell<bool>,
    has_prev: Cell<bool>,
    busy: Cell<bool>,
    last_known_total: Cell<Option<usize>>,
    pub scroll_to_bottom_after_load: Cell<bool>,
}

impl ResultsView {
    pub fn new(grid_font: f64, auto_scroll: bool) -> Rc<ResultsView> {
        let grid = DataGrid::new(grid_font);
        let first = icon_button("go-first-symbolic", "Prima pagina");
        let prev = icon_button("go-previous-symbolic", "Pagina precedente (Alt+←)");
        let next = icon_button("go-next-symbolic", "Pagina successiva (Alt+→)");
        let last = icon_button("go-last-symbolic", "Ultima pagina");
        let page_entry = gtk::Entry::new();
        page_entry.set_width_chars(4);
        page_entry.set_max_width_chars(6);
        EntryExt::set_alignment(&page_entry, 1.0);
        page_entry.set_tooltip_text(Some("Vai alla pagina (Invio)"));
        let page_total = gtk::Label::new(None);
        page_total.add_css_class("dim-label");
        let info = gtk::Label::new(Some("Nessun risultato"));
        info.set_xalign(0.0);
        info.set_hexpand(true);
        info.set_ellipsize(gtk::pango::EllipsizeMode::End);
        info.set_margin_start(8);
        let spinner = gtk::Spinner::new();
        let export = labeled_button(
            "document-save-symbolic",
            "Esporta CSV",
            "Esporta i risultati in un file CSV",
        );
        export.set_sensitive(false);
        let auto_scroll_btn = gtk::ToggleButton::new();
        auto_scroll_btn.set_icon_name("view-continuous-symbolic");
        auto_scroll_btn.add_css_class("flat");
        auto_scroll_btn.set_active(auto_scroll);
        auto_scroll_btn.set_tooltip_text(Some(
            "Scorrimento automatico delle pagine: continuando a scorrere oltre la fine (o l'inizio) \
             viene caricata la pagina successiva (o precedente)",
        ));

        let stack = gtk::Stack::new();
        stack.add_titled(&grid.scroller, Some("data"), "Risultati");
        let log = gtk::TextView::new();
        log.set_editable(false);
        log.set_cursor_visible(false);
        log.set_monospace(true);
        log.set_wrap_mode(gtk::WrapMode::WordChar);
        log.set_left_margin(8);
        log.set_top_margin(6);
        log.add_css_class("message-log");
        let err_tag = gtk::TextTag::builder()
            .name("err")
            .foreground("#e01b24")
            .build();
        let ok_tag = gtk::TextTag::builder()
            .name("ok")
            .foreground("#26a269")
            .build();
        let dim_tag = gtk::TextTag::builder()
            .name("dim")
            .foreground("#888888")
            .build();
        log.buffer().tag_table().add(&err_tag);
        log.buffer().tag_table().add(&ok_tag);
        log.buffer().tag_table().add(&dim_tag);
        let log_sc = gtk::ScrolledWindow::new();
        log_sc.set_child(Some(&log));
        stack.add_titled(&log_sc, Some("messages"), "Messaggi");
        stack.set_vexpand(true);
        let switcher = gtk::StackSwitcher::new();
        switcher.set_stack(Some(&stack));

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        bar.add_css_class("results-bar");
        bar.set_margin_start(4);
        bar.set_margin_end(4);
        bar.set_margin_top(2);
        bar.set_margin_bottom(2);
        let pager = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        pager.append(&first);
        pager.append(&prev);
        let pl = gtk::Label::new(Some("Pagina"));
        pl.set_margin_start(4);
        pager.append(&pl);
        pager.append(&page_entry);
        pager.append(&page_total);
        pager.append(&next);
        pager.append(&last);
        pager.append(&auto_scroll_btn);
        bar.append(&pager);
        bar.append(&info);
        bar.append(&spinner);
        bar.append(&export);
        bar.append(&switcher);

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&bar);
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(&stack);

        let rv = Rc::new(ResultsView {
            widget,
            pager,
            grid,
            first,
            prev,
            next,
            last,
            page_entry,
            page_total,
            info,
            spinner,
            export,
            auto_scroll: auto_scroll_btn,
            stack,
            log,
            on_nav: RefCell::default(),
            on_export: RefCell::default(),
            overscroll: Cell::new(0.0),
            has_next: Cell::new(false),
            has_prev: Cell::new(false),
            busy: Cell::new(false),
            last_known_total: Cell::new(None),
            scroll_to_bottom_after_load: Cell::new(false),
        });
        rv.connect_signals();
        rv.set_pager(None);
        rv
    }

    fn connect_signals(self: &Rc<Self>) {
        let nav = |rv: &Rc<ResultsView>, b: &gtk::Button, n: PageNav| {
            let w = Rc::downgrade(rv);
            b.connect_clicked(move |_| {
                if let Some(rv) = w.upgrade() {
                    rv.navigate(n);
                }
            });
        };
        nav(self, &self.first, PageNav::First);
        nav(self, &self.prev, PageNav::Prev);
        nav(self, &self.next, PageNav::Next);
        nav(self, &self.last, PageNav::Last);
        let w = Rc::downgrade(self);
        self.page_entry.connect_activate(move |e| {
            if let (Some(rv), Ok(n)) = (w.upgrade(), e.text().trim().parse::<usize>())
                && n > 0
            {
                rv.navigate(PageNav::Goto(n - 1));
            }
        });
        let w = Rc::downgrade(self);
        self.export.connect_clicked(move |_| {
            let cb = w.upgrade().and_then(|rv| rv.on_export.borrow().clone());
            if let Some(cb) = cb {
                cb();
            }
        });

        // Scorrimento automatico tra le pagine
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        // In fase "capture" l'evento arriva prima che lo ScrolledWindow lo consumi.
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        let w = Rc::downgrade(self);
        scroll.connect_scroll(move |_, _dx, dy| {
            let Some(rv) = w.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if !rv.auto_scroll.is_active() || rv.busy.get() || dy == 0.0 {
                return glib::Propagation::Proceed;
            }
            let adj = rv.grid.scroller.vadjustment();
            let at_bottom = adj.value() + adj.page_size() >= adj.upper() - 1.0;
            let at_top = adj.value() <= adj.lower() + 0.5;
            if dy > 0.0 && at_bottom && rv.has_next.get() {
                rv.bump_overscroll(dy, PageNav::Next);
            } else if dy < 0.0 && at_top && rv.has_prev.get() {
                rv.bump_overscroll(dy, PageNav::Prev);
            } else {
                rv.overscroll.set(0.0);
            }
            glib::Propagation::Proceed
        });
        self.grid.scroller.add_controller(scroll);

        let keys = gtk::EventControllerKey::new();
        let w = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(rv) = w.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if state.contains(gtk::gdk::ModifierType::ALT_MASK) {
                match key {
                    gtk::gdk::Key::Right => {
                        rv.navigate(PageNav::Next);
                        return glib::Propagation::Stop;
                    }
                    gtk::gdk::Key::Left => {
                        rv.navigate(PageNav::Prev);
                        return glib::Propagation::Stop;
                    }
                    _ => {}
                }
            }
            glib::Propagation::Proceed
        });
        self.widget.add_controller(keys);
    }

    /// Serve continuare a scorrere oltre il bordo (circa 3 scatti di rotella,
    /// o l'equivalente su touchpad) per cambiare pagina, così arrivare in fondo
    /// non salta subito alla pagina successiva.
    fn bump_overscroll(&self, dy: f64, nav: PageNav) {
        let v = self.overscroll.get();
        let v = if v.signum() == dy.signum() {
            v + dy
        } else {
            dy
        };
        if v.abs() >= 3.0 {
            self.overscroll.set(0.0);
            self.scroll_to_bottom_after_load.set(nav == PageNav::Prev);
            self.navigate(nav);
        } else {
            self.overscroll.set(v);
        }
    }

    /// Porta la griglia al bordo e simula tick di rotella (usato dai devtools).
    #[cfg(feature = "devtools")]
    pub fn dev_scroll(&self, ticks: i32) {
        let adj = self.grid.scroller.vadjustment();
        adj.set_value(if ticks > 0 { adj.upper() } else { adj.lower() });
        if let Some(c) = self
            .grid
            .scroller
            .observe_controllers()
            .into_iter()
            .filter_map(|c| c.ok().and_downcast::<gtk::EventControllerScroll>())
            .find(|c| c.propagation_phase() == gtk::PropagationPhase::Capture)
        {
            for _ in 0..ticks.abs() {
                c.emit_by_name::<bool>("scroll", &[&0.0f64, &(ticks.signum() as f64)]);
            }
        }
    }

    pub fn navigate(&self, n: PageNav) {
        if self.busy.get() {
            return;
        }
        let cb = self.on_nav.borrow().clone();
        if let Some(cb) = cb {
            cb(n);
        }
    }

    pub fn set_on_nav(&self, f: impl Fn(PageNav) + 'static) {
        *self.on_nav.borrow_mut() = Some(Rc::new(f));
    }

    pub fn set_on_export(&self, f: impl Fn() + 'static) {
        *self.on_export.borrow_mut() = Some(Rc::new(f));
    }

    pub fn set_busy(&self, busy: bool, text: Option<&str>) {
        self.busy.set(busy);
        if busy {
            self.spinner.start();
        } else {
            self.spinner.stop();
        }
        self.spinner.set_visible(busy);
        if let Some(t) = text {
            self.info.set_text(t);
        }
        for b in [&self.first, &self.prev, &self.next, &self.last] {
            if busy {
                b.set_sensitive(false);
            }
        }
        if !busy {
            self.update_nav_sensitivity();
        }
    }

    #[cfg(feature = "devtools")]
    pub fn dev_info(&self) -> String {
        self.info.text().to_string()
    }

    pub fn set_info(&self, text: &str) {
        self.info.set_text(text);
        self.info.set_tooltip_text(Some(text));
    }

    /// Aggiorna la barra di paginazione: `None` = nessun risultato paginato.
    /// `total_pages` = None se il numero totale non è (ancora) noto.
    pub fn set_pager(&self, state: Option<(usize, Option<usize>, bool)>) {
        match state {
            None => {
                self.page_entry.set_text("");
                self.page_total.set_text("");
                self.has_next.set(false);
                self.has_prev.set(false);
                self.page_entry.set_sensitive(false);
            }
            Some((page, total_pages, has_more)) => {
                self.page_entry.set_text(&(page + 1).to_string());
                self.page_total.set_text(&match total_pages {
                    Some(t) => format!("di {}", fmt_count(t as u64)),
                    None if has_more => "di …".to_string(),
                    None => format!("di {}", page + 1),
                });
                self.has_next.set(has_more);
                self.has_prev.set(page > 0);
                self.page_entry.set_sensitive(has_more || page > 0);
            }
        }
        self.pager.set_visible(state.is_some());
        self.last_known_total.set(state.and_then(|s| s.1));
        self.update_nav_sensitivity();
    }

    fn update_nav_sensitivity(&self) {
        self.first.set_sensitive(self.has_prev.get());
        self.prev.set_sensitive(self.has_prev.get());
        self.next.set_sensitive(self.has_next.get());
        self.last
            .set_sensitive(self.has_next.get() && self.last_known_total.get().is_some());
    }

    pub fn show_data(&self) {
        self.stack.set_visible_child_name("data");
    }

    pub fn show_messages(&self) {
        self.stack.set_visible_child_name("messages");
        self.scroll_log_to_end();
    }

    /// Lo scroll va fatto dopo l'allocazione: se il log era nascosto la sua
    /// dimensione non è ancora quella definitiva.
    fn scroll_log_to_end(&self) {
        let log = self.log.clone();
        glib::idle_add_local_once(move || {
            if let Some(adj) = log.vadjustment() {
                adj.set_value(adj.upper() - adj.page_size());
            }
        });
    }

    pub fn log(&self, text: &str, kind: Option<&str>) {
        let buf = self.log.buffer();
        let mut end = buf.end_iter();
        let now = glib::DateTime::now_local()
            .ok()
            .and_then(|d| d.format("%H:%M:%S").ok());
        buf.insert_with_tags_by_name(
            &mut end,
            &format!("[{}] ", now.as_deref().unwrap_or("")),
            &["dim"],
        );
        match kind {
            Some(tag) => buf.insert_with_tags_by_name(&mut end, text, &[tag]),
            None => buf.insert(&mut end, text),
        }
        buf.insert(&mut end, "\n");
        self.scroll_log_to_end();
    }
}
