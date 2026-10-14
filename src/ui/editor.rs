//! Editor SQL: `GtkTextView` con evidenziazione della sintassi (lexer
//! proprio), controllo sintattico in background con `sqlparser`, numeri di
//! riga, auto-indentazione, commento/decommento delle righe e
//! autocompletamento di tabelle, colonne e parole chiave.

use gtk::prelude::*;
use gtk::{gdk, gio, glib, pango};
use lemure::completion::{self, Completion, Item, ItemKind, SchemaMeta};
use lemure::sqlutil::{self, Dialect, Tok, WordClass};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

struct Tags {
    keyword: gtk::TextTag,
    typ: gtk::TextTag,
    func: gtk::TextTag,
    string: gtk::TextTag,
    number: gtk::TextTag,
    comment: gtk::TextTag,
    ident: gtk::TextTag,
    param: gtk::TextTag,
    error: gtk::TextTag,
    executed: gtk::TextTag,
}

/// Fornisce all'editor i metadati del datasource corrente.
pub struct CompletionSource {
    /// Metadati dello schema, se già caricati.
    pub meta: Box<dyn Fn() -> Option<Rc<RefCell<SchemaMeta>>>>,
    /// Carica le colonne delle tabelle indicate (schema, tabella) e poi chiama la callback.
    pub load_columns: Box<dyn Fn(Vec<(String, String)>, Box<dyn FnOnce()>)>,
}

struct Popup {
    popover: gtk::Popover,
    list: gtk::ListView,
    selection: gtk::SingleSelection,
    store: gio::ListStore,
}

pub struct EditorInner {
    pub widget: gtk::Box,
    pub view: gtk::TextView,
    pub buffer: gtk::TextBuffer,
    status: gtk::Label,
    gutter: gtk::DrawingArea,
    dialect: Cell<Dialect>,
    tags: Tags,
    hl_source: RefCell<Option<glib::SourceId>>,
    check_source: RefCell<Option<glib::SourceId>>,
    flash_source: RefCell<Option<glib::SourceId>>,
    font_size: Cell<f64>,
    dark: Cell<Option<bool>>,
    source: RefCell<Option<Rc<CompletionSource>>>,
    popup: Popup,
    completion: RefCell<Option<Completion>>,
    /// Impostato dal tasto premuto: la modifica successiva del buffer aggiorna il completamento.
    armed: Cell<bool>,
    comp_timer: RefCell<Option<glib::SourceId>>,
    /// Tentativi residui mentre i metadati del datasource vengono caricati.
    meta_retries: Cell<u32>,
}

impl Drop for EditorInner {
    fn drop(&mut self) {
        // vedi hide_completion: la TextView non deve sopravvivere col popover attaccato
        if self.popup.popover.parent().is_some() {
            self.popup.popover.unparent();
        }
    }
}

#[derive(Clone)]
pub struct QueryEditor(Rc<EditorInner>);

impl std::ops::Deref for QueryEditor {
    type Target = EditorInner;
    fn deref(&self) -> &EditorInner {
        &self.0
    }
}

fn tag(name: &str) -> gtk::TextTag {
    gtk::TextTag::builder().name(name).build()
}

impl QueryEditor {
    pub fn new(dialect: Dialect, font_size: f64) -> QueryEditor {
        let buffer = gtk::TextBuffer::new(None);
        let tags = Tags {
            keyword: tag("kw"),
            typ: tag("type"),
            func: tag("func"),
            string: tag("str"),
            number: tag("num"),
            comment: tag("comment"),
            ident: tag("ident"),
            param: tag("param"),
            error: gtk::TextTag::builder()
                .name("error")
                .underline(pango::Underline::Error)
                .underline_rgba(&gdk::RGBA::new(0.88, 0.11, 0.14, 1.0))
                .build(),
            executed: tag("executed"),
        };
        tags.keyword.set_weight(700);
        tags.comment.set_style(pango::Style::Italic);
        let table = buffer.tag_table();
        for t in [
            &tags.keyword,
            &tags.typ,
            &tags.func,
            &tags.string,
            &tags.number,
            &tags.comment,
            &tags.ident,
            &tags.param,
            &tags.error,
            &tags.executed,
        ] {
            table.add(t);
        }

        let view = gtk::TextView::with_buffer(&buffer);
        view.set_monospace(true);
        view.add_css_class("sql-editor");
        view.set_left_margin(6);
        view.set_top_margin(4);
        view.set_bottom_margin(4);
        view.set_wrap_mode(gtk::WrapMode::None);
        view.set_input_hints(gtk::InputHints::NO_SPELLCHECK | gtk::InputHints::NO_EMOJI);

        let gutter = gtk::DrawingArea::new();
        gutter.add_css_class("line-numbers");
        view.set_gutter(gtk::TextWindowType::Left, Some(&gutter));

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_child(Some(&view));
        scroller.set_vexpand(true);

        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.set_ellipsize(pango::EllipsizeMode::End);
        status.add_css_class("syntax-status");
        status.set_margin_start(6);
        status.set_margin_end(6);

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&scroller);
        widget.append(&status);

        let popup = build_popup();

        let ed = QueryEditor(Rc::new(EditorInner {
            widget,
            view,
            buffer,
            status,
            gutter,
            dialect: Cell::new(dialect),
            tags,
            hl_source: RefCell::default(),
            check_source: RefCell::default(),
            flash_source: RefCell::default(),
            font_size: Cell::new(font_size),
            dark: Cell::new(None),
            source: RefCell::default(),
            popup,
            completion: RefCell::default(),
            armed: Cell::new(false),
            comp_timer: RefCell::default(),
            meta_retries: Cell::new(0),
        }));
        ed.setup_gutter(&scroller);
        ed.setup_signals();
        ed.setup_keys();
        ed.setup_completion(&scroller);
        ed
    }

    fn weak(&self) -> Weak<EditorInner> {
        Rc::downgrade(&self.0)
    }

    fn up(w: &Weak<EditorInner>) -> Option<QueryEditor> {
        w.upgrade().map(QueryEditor)
    }

    pub fn text(&self) -> String {
        let (s, e) = self.buffer.bounds();
        self.buffer.text(&s, &e, true).to_string()
    }

    pub fn set_text(&self, text: &str) {
        self.buffer.set_text(text);
        self.buffer.place_cursor(&self.buffer.end_iter());
    }

    pub fn insert_at_cursor(&self, text: &str) {
        self.buffer.insert_at_cursor(text);
        self.view.grab_focus();
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect.get()
    }

    pub fn set_dialect(&self, d: Dialect) {
        if self.dialect.replace(d) != d {
            self.schedule_highlight(0);
            self.schedule_check(0);
        }
    }

    pub fn hide_status(&self) {
        self.status.set_visible(false);
    }

    pub fn set_font_size(&self, size: f64) {
        self.font_size.set(size);
        self.gutter.queue_draw();
    }

    /// Statement da eseguire: la selezione (eventualmente più statement),
    /// tutto lo script, oppure lo statement sotto il cursore.
    /// Ritorna gli statement con il loro intervallo (in caratteri) nel buffer.
    pub fn statements_to_run(&self, whole_script: bool) -> Vec<(String, i32, i32)> {
        let d = self.dialect.get();
        let (text, base) = match self.buffer.selection_bounds() {
            Some((s, e)) if !whole_script => {
                (self.buffer.text(&s, &e, true).to_string(), s.offset())
            }
            _ => (self.text(), 0),
        };
        let toks = sqlutil::lex(&text, d);
        let stmts = sqlutil::split_statements(&toks, true, &text);
        let selected = self.buffer.has_selection() && !whole_script;
        let chosen: Vec<sqlutil::Stmt> = if whole_script || selected {
            stmts
        } else {
            let cursor = self.buffer.iter_at_mark(&self.buffer.get_insert()).offset() as usize;
            sqlutil::statement_at(&stmts, cursor).into_iter().collect()
        };
        chosen
            .into_iter()
            .map(|s| {
                (
                    text[s.start..s.end].to_string(),
                    base + s.cstart as i32,
                    base + s.cend as i32,
                )
            })
            .collect()
    }

    /// Evidenzia brevemente lo statement eseguito.
    pub fn flash_range(&self, start: i32, end: i32) {
        let (s, e) = self.buffer.bounds();
        self.buffer.remove_tag(&self.tags.executed, &s, &e);
        self.buffer.apply_tag(
            &self.tags.executed,
            &self.buffer.iter_at_offset(start),
            &self.buffer.iter_at_offset(end),
        );
        if let Some(id) = self.flash_source.take() {
            id.remove();
        }
        let weak = self.weak();
        let id = glib::timeout_add_local_once(Duration::from_millis(700), move || {
            if let Some(ed) = QueryEditor::up(&weak) {
                ed.flash_source.take();
                let (s, e) = ed.buffer.bounds();
                ed.buffer.remove_tag(&ed.tags.executed, &s, &e);
            }
        });
        *self.flash_source.borrow_mut() = Some(id);
    }

    fn setup_signals(&self) {
        let weak = self.weak();
        self.buffer.connect_changed(move |_| {
            if let Some(ed) = QueryEditor::up(&weak) {
                ed.schedule_highlight(40);
                ed.schedule_check(450);
                ed.gutter.queue_draw();
                if ed.armed.replace(false) {
                    ed.schedule_completion();
                }
            }
        });
        let weak = self.weak();
        self.view.connect_realize(move |_| {
            if let Some(ed) = QueryEditor::up(&weak) {
                ed.update_palette();
            }
        });
        if let Some(settings) = gtk::Settings::default() {
            let weak = self.weak();
            settings.connect_gtk_application_prefer_dark_theme_notify(move |_| {
                if let Some(ed) = QueryEditor::up(&weak) {
                    ed.dark.set(None);
                    ed.update_palette();
                }
            });
            let weak = self.weak();
            settings.connect_gtk_theme_name_notify(move |_| {
                if let Some(ed) = QueryEditor::up(&weak) {
                    ed.dark.set(None);
                    ed.update_palette();
                }
            });
        }
    }

    /// Colori adatti a tema chiaro o scuro, in base al colore del testo.
    fn update_palette(&self) {
        let fg = self.view.color();
        let dark = (fg.red() + fg.green() + fg.blue()) / 3.0 > 0.5;
        if self.dark.get() == Some(dark) {
            return;
        }
        self.dark.set(Some(dark));
        let t = &self.tags;
        let set = |tag: &gtk::TextTag, light: &str, dark_c: &str| {
            tag.set_foreground(Some(if dark { dark_c } else { light }));
        };
        set(&t.keyword, "#0033b3", "#6cb6ff");
        set(&t.typ, "#1a7f7f", "#4ec9b0");
        set(&t.func, "#7a3e9d", "#dcdcaa");
        set(&t.string, "#067d17", "#ce9178");
        set(&t.number, "#1750eb", "#b5cea8");
        set(&t.comment, "#8c8c8c", "#7f9f7f");
        set(&t.ident, "#871094", "#c3a6ff");
        set(&t.param, "#b85c00", "#e5c07b");
        t.executed
            .set_background(Some(if dark { "#3a3d1e" } else { "#fff3b0" }));
    }

    fn schedule_highlight(&self, ms: u64) {
        if let Some(id) = self.hl_source.take() {
            id.remove();
        }
        let weak = self.weak();
        let id = glib::timeout_add_local_once(Duration::from_millis(ms), move || {
            if let Some(ed) = QueryEditor::up(&weak) {
                ed.hl_source.take();
                ed.highlight();
            }
        });
        *self.hl_source.borrow_mut() = Some(id);
    }

    fn schedule_check(&self, ms: u64) {
        if let Some(id) = self.check_source.take() {
            id.remove();
        }
        let weak = self.weak();
        let id = glib::timeout_add_local_once(Duration::from_millis(ms), move || {
            if let Some(ed) = QueryEditor::up(&weak) {
                ed.check_source.take();
                ed.check();
            }
        });
        *self.check_source.borrow_mut() = Some(id);
    }

    fn highlight(&self) {
        let buf = &self.buffer;
        let (start, end) = buf.bounds();
        let text = buf.text(&start, &end, true);
        let t = &self.tags;
        for tag in [
            &t.keyword, &t.typ, &t.func, &t.string, &t.number, &t.comment, &t.ident, &t.param,
        ] {
            buf.remove_tag(tag, &start, &end);
        }
        let toks = sqlutil::lex(&text, self.dialect.get());
        let apply = |tag: &gtk::TextTag, s: usize, e: usize| {
            buf.apply_tag(
                tag,
                &buf.iter_at_offset(s as i32),
                &buf.iter_at_offset(e as i32),
            );
        };
        for (i, tok) in toks.iter().enumerate() {
            let tag = match tok.kind {
                Tok::Comment => &t.comment,
                Tok::Str => &t.string,
                Tok::Number => &t.number,
                Tok::QuotedIdent => &t.ident,
                Tok::Param => &t.param,
                Tok::Word => {
                    let next_paren = toks[i + 1..]
                        .iter()
                        .find(|n| n.kind != Tok::Space)
                        .is_some_and(|n| n.kind == Tok::OpenParen);
                    // Le parole dopo un punto (es. t.select) sono nomi, non keyword.
                    let after_dot = i > 0 && &text[toks[i - 1].start..toks[i - 1].end] == ".";
                    if after_dot {
                        continue;
                    }
                    match sqlutil::classify_word(&text[tok.start..tok.end], next_paren) {
                        WordClass::Keyword => &t.keyword,
                        WordClass::Type => &t.typ,
                        WordClass::Function => &t.func,
                        WordClass::Plain => continue,
                    }
                }
                _ => continue,
            };
            apply(tag, tok.cstart, tok.cend);
        }
    }

    fn check(&self) {
        let buf = &self.buffer;
        let (start, end) = buf.bounds();
        buf.remove_tag(&self.tags.error, &start, &end);
        let text = self.text();
        if text.trim().is_empty() {
            self.status.set_text("");
            return;
        }
        match sqlutil::check_syntax(&text, self.dialect.get()) {
            None => {
                self.status.set_text("✓ Sintassi valida");
                self.status.remove_css_class("syntax-error");
            }
            Some(issue) => {
                let line = issue.line.saturating_sub(1) as i32;
                let col = issue.column.saturating_sub(1) as i32;
                if let Some(mut s) = buf
                    .iter_at_line_offset(line, col)
                    .or_else(|| buf.iter_at_line(line))
                {
                    let mut e = s;
                    if !e.ends_word() || e == s {
                        e.forward_word_end();
                    }
                    if e.offset() <= s.offset() {
                        // a fine testo: sottolinea l'ultimo carattere
                        s.backward_char();
                        e = buf.end_iter();
                    }
                    buf.apply_tag(&self.tags.error, &s, &e);
                }
                self.status.set_text(&format!(
                    "⚠ Riga {}, colonna {}: {}",
                    issue.line, issue.column, issue.message
                ));
                self.status.add_css_class("syntax-error");
                self.status.set_tooltip_text(Some(&issue.message));
            }
        }
    }

    fn setup_gutter(&self, scroller: &gtk::ScrolledWindow) {
        let weak = self.weak();
        self.gutter.set_draw_func(move |area, cr, w, _h| {
            let Some(ed) = QueryEditor::up(&weak) else {
                return;
            };
            let view = &ed.view;
            let digits = ed.buffer.line_count().max(10).to_string().len() as i32;
            let layout = area.create_pango_layout(None);
            let desc =
                pango::FontDescription::from_string(&format!("Monospace {}", ed.font_size.get()));
            layout.set_font_description(Some(&desc));
            layout.set_text("0");
            let char_w = layout.pixel_size().0;
            let want = char_w * digits + 14;
            if area.width_request() != want {
                area.set_width_request(want);
            }
            let fg = area.color();
            cr.set_source_rgba(fg.red() as f64, fg.green() as f64, fg.blue() as f64, 0.45);
            let cursor_line = ed.buffer.iter_at_mark(&ed.buffer.get_insert()).line();
            let visible = view.visible_rect();
            let (mut iter, _) = view.line_at_y(visible.y());
            let bottom = visible.y() + visible.height();
            let mut last_drawn = -1;
            loop {
                let line = iter.line();
                if line != last_drawn {
                    let (y, _height) = view.line_yrange(&iter);
                    if y > bottom {
                        break;
                    }
                    let (_, wy) = view.buffer_to_window_coords(gtk::TextWindowType::Left, 0, y);
                    layout.set_text(&(line + 1).to_string());
                    let tw = layout.pixel_size().0;
                    let alpha = if line == cursor_line { 0.9 } else { 0.45 };
                    cr.set_source_rgba(fg.red() as f64, fg.green() as f64, fg.blue() as f64, alpha);
                    cr.move_to((w - tw - 6) as f64, wy as f64);
                    pangocairo::functions::show_layout(cr, &layout);
                    last_drawn = line;
                }
                if !iter.forward_line() {
                    // l'ultima riga vuota non viene raggiunta da forward_line
                    if iter.line() == last_drawn {
                        break;
                    }
                }
            }
        });
        let g = self.gutter.clone();
        scroller
            .vadjustment()
            .connect_value_changed(move |_| g.queue_draw());
        let g = self.gutter.clone();
        self.buffer.connect_mark_set(move |_, _, mark| {
            if mark.name().as_deref() == Some("insert") {
                g.queue_draw();
            }
        });
    }

    fn setup_keys(&self) {
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = self.weak();
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(ed) = QueryEditor::up(&weak) else {
                return glib::Propagation::Proceed;
            };
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            let alt = state.contains(gdk::ModifierType::ALT_MASK);
            if ed.popup.popover.is_visible() && !ctrl && !alt {
                match key {
                    gdk::Key::Down | gdk::Key::KP_Down => return ed.move_selection(1),
                    gdk::Key::Up | gdk::Key::KP_Up => return ed.move_selection(-1),
                    gdk::Key::Page_Down => return ed.move_selection(8),
                    gdk::Key::Page_Up => return ed.move_selection(-8),
                    gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::Tab if !shift => {
                        ed.accept_completion();
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::Escape => {
                        ed.hide_completion();
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::Left | gdk::Key::Right | gdk::Key::Home | gdk::Key::End => {
                        ed.hide_completion();
                    }
                    _ => {}
                }
            }
            if ctrl && key == gdk::Key::space {
                ed.trigger_completion(true);
                return glib::Propagation::Stop;
            }
            if !ctrl && !alt {
                match key.to_unicode() {
                    Some(c)
                        if c.is_alphanumeric()
                            || matches!(c, '_' | '.' | '`' | '"' | ' ' | ',' | '$') =>
                    {
                        ed.armed.set(true)
                    }
                    Some(_) => ed.hide_completion(),
                    None if key == gdk::Key::BackSpace && ed.popup.popover.is_visible() => {
                        ed.armed.set(true)
                    }
                    None => {}
                }
            }
            match key {
                gdk::Key::Return | gdk::Key::KP_Enter if ctrl => {
                    let action = if shift { "win.run-script" } else { "win.run" };
                    let _ = ed.view.activate_action(action, None);
                    glib::Propagation::Stop
                }
                gdk::Key::x | gdk::Key::X if alt => {
                    let _ = ed.view.activate_action("win.run-script", None);
                    glib::Propagation::Stop
                }
                gdk::Key::Return | gdk::Key::KP_Enter if !ctrl && !alt => {
                    ed.newline_with_indent();
                    glib::Propagation::Stop
                }
                gdk::Key::Tab if !ctrl && !ed.buffer.has_selection() => {
                    ed.buffer.insert_at_cursor("    ");
                    glib::Propagation::Stop
                }
                gdk::Key::slash if ctrl => {
                    ed.toggle_comment();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.view.add_controller(keys);
    }

    /// Simula la digitazione carattere per carattere (devtools).
    #[cfg(feature = "devtools")]
    pub fn dev_type(&self, text: &str) {
        for ch in text.chars() {
            self.armed.set(true);
            self.buffer.insert_at_cursor(&ch.to_string());
        }
    }

    /// Simula i tasti gestiti dal popup (devtools).
    #[cfg(feature = "devtools")]
    pub fn dev_key(&self, key: &str) {
        match key {
            "down" => {
                self.move_selection(1);
            }
            "up" => {
                self.move_selection(-1);
            }
            "enter" => self.accept_completion(),
            "escape" => self.hide_completion(),
            "ctrl-space" => self.trigger_completion(true),
            _ => {}
        }
    }

    #[cfg(feature = "devtools")]
    pub fn dev_popup_visible(&self) -> bool {
        self.popup.popover.is_visible()
    }

    pub fn set_completion_source(&self, source: Rc<CompletionSource>) {
        *self.source.borrow_mut() = Some(source);
    }

    fn setup_completion(&self, scroller: &gtk::ScrolledWindow) {
        let weak = self.weak();
        self.popup.list.connect_activate(move |_, pos| {
            if let Some(ed) = QueryEditor::up(&weak) {
                ed.popup.selection.set_selected(pos);
                ed.accept_completion();
            }
        });
        // Clic nel testo o scorrimento: il popup non è più pertinente.
        let click = gtk::GestureClick::new();
        let weak = self.weak();
        click.connect_pressed(move |_, _, _, _| {
            if let Some(ed) = QueryEditor::up(&weak) {
                ed.hide_completion();
            }
        });
        self.view.add_controller(click);
        let weak = self.weak();
        scroller.vadjustment().connect_value_changed(move |_| {
            if let Some(ed) = QueryEditor::up(&weak) {
                ed.hide_completion();
            }
        });
    }

    fn schedule_completion(&self) {
        if let Some(id) = self.comp_timer.take() {
            id.remove();
        }
        let weak = self.weak();
        let id = glib::timeout_add_local_once(Duration::from_millis(60), move || {
            if let Some(ed) = QueryEditor::up(&weak) {
                ed.comp_timer.take();
                ed.trigger_completion(false);
            }
        });
        *self.comp_timer.borrow_mut() = Some(id);
    }

    /// Calcola i suggerimenti per la posizione del cursore. Senza `explicit`
    /// (Ctrl+Spazio) il popup appare solo dopo `alias.`, dopo FROM/JOIN o con
    /// almeno due caratteri digitati.
    pub fn trigger_completion(&self, explicit: bool) {
        let Some(source) = self.source.borrow().clone() else {
            return;
        };
        if self.buffer.has_selection() {
            self.hide_completion();
            return;
        }
        let cursor = self.buffer.iter_at_mark(&self.buffer.get_insert()).offset() as usize;
        let Some(meta) = (source.meta)() else {
            self.hide_completion();
            self.retry_when_meta_ready(cursor, explicit);
            return;
        };
        self.meta_retries.set(0);
        let text = self.text();
        let comp =
            completion::complete(&text, cursor, self.dialect.get(), &meta.borrow(), explicit);
        let Some(comp) = comp else {
            self.hide_completion();
            return;
        };
        if !comp.missing.is_empty() {
            // Schemi grandi: colonne caricate su richiesta, poi si ricalcola.
            let weak = self.weak();
            (source.load_columns)(
                comp.missing.clone(),
                Box::new(move || {
                    if let Some(ed) = QueryEditor::up(&weak) {
                        let pos = ed.buffer.iter_at_mark(&ed.buffer.get_insert()).offset() as usize;
                        if pos == cursor {
                            ed.trigger_completion(explicit);
                        }
                    }
                }),
            );
        }
        let show = explicit
            || comp.context != completion::Context::General
            || comp.prefix.chars().count() >= 2;
        if !show || comp.items.is_empty() {
            self.hide_completion();
            return;
        }
        let objs: Vec<glib::BoxedAnyObject> = comp
            .items
            .iter()
            .cloned()
            .map(glib::BoxedAnyObject::new)
            .collect();
        let p = &self.popup;
        p.store.splice(0, p.store.n_items(), &objs);
        p.selection.set_selected(0);
        p.list.scroll_to(0, gtk::ListScrollFlags::NONE, None);
        let iter = self.buffer.iter_at_offset(comp.start as i32);
        let loc = self.view.iter_location(&iter);
        let (x, y) =
            self.view
                .buffer_to_window_coords(gtk::TextWindowType::Widget, loc.x(), loc.y());
        p.popover
            .set_pointing_to(Some(&gdk::Rectangle::new(x, y, 1, loc.height().max(1))));
        *self.completion.borrow_mut() = Some(comp);
        if p.popover.parent().is_none() {
            p.popover.set_parent(&self.view);
        }
        if !p.popover.is_visible() {
            p.popover.popup();
        }
    }

    /// I metadati si caricano in background alla prima digitazione: si riprova
    /// per qualche secondo, finché il cursore resta dov'era.
    fn retry_when_meta_ready(&self, cursor: usize, explicit: bool) {
        let left = self.meta_retries.get();
        let left = if left == 0 { 25 } else { left - 1 };
        if left == 0 {
            return;
        }
        self.meta_retries.set(left);
        let weak = self.weak();
        let id = glib::timeout_add_local_once(Duration::from_millis(200), move || {
            if let Some(ed) = QueryEditor::up(&weak) {
                ed.comp_timer.take();
                let pos = ed.buffer.iter_at_mark(&ed.buffer.get_insert()).offset() as usize;
                if pos == cursor {
                    ed.trigger_completion(explicit);
                } else {
                    ed.meta_retries.set(0);
                }
            }
        });
        if let Some(old) = self.comp_timer.replace(Some(id)) {
            old.remove();
        }
    }

    fn move_selection(&self, delta: i32) -> glib::Propagation {
        let p = &self.popup;
        let n = p.store.n_items() as i32;
        if n > 0 {
            let cur = p.selection.selected() as i32;
            let next = if delta.abs() == 1 {
                (cur + delta).rem_euclid(n)
            } else {
                (cur + delta).clamp(0, n - 1)
            };
            p.selection.set_selected(next as u32);
            p.list
                .scroll_to(next as u32, gtk::ListScrollFlags::NONE, None);
        }
        glib::Propagation::Stop
    }

    fn accept_completion(&self) {
        let Some(comp) = self.completion.take() else {
            self.hide_completion();
            return;
        };
        let idx = self.popup.selection.selected() as usize;
        self.hide_completion();
        let Some(item) = comp.items.get(idx) else {
            return;
        };
        let buf = &self.buffer;
        let cursor = buf.iter_at_mark(&buf.get_insert()).offset() as usize;
        // Il testo può essere cambiato dopo il calcolo: si sostituisce fino al cursore.
        let end = cursor.max(comp.start);
        let mut s = buf.iter_at_offset(comp.start as i32);
        let mut e = buf.iter_at_offset(end as i32);
        // Uno schema viene completato con il punto e si propongono subito le sue tabelle.
        let schema = item.kind == ItemKind::Schema;
        let text = if schema {
            format!("{}.", item.insert)
        } else {
            item.insert.clone()
        };
        buf.begin_user_action();
        buf.delete(&mut s, &mut e);
        buf.insert(&mut s, &text);
        buf.end_user_action();
        self.view.grab_focus();
        if schema {
            self.trigger_completion(false);
        }
    }

    pub fn hide_completion(&self) {
        if let Some(id) = self.comp_timer.take() {
            id.remove();
        }
        self.completion.borrow_mut().take();
        let pop = &self.popup.popover;
        if pop.is_visible() {
            pop.popdown();
        }
        // Il popover resta figlio della TextView solo mentre è aperto: GtkTextView,
        // quando viene distrutta, non sa staccare figli che non gestisce lei e
        // resterebbe in un ciclo infinito. L'unparent è rimandato perché si può
        // arrivare qui da un evento del popover stesso.
        if pop.parent().is_some() {
            let pop = pop.downgrade();
            glib::idle_add_local_once(move || {
                if let Some(p) = pop.upgrade()
                    && !p.is_visible()
                    && p.parent().is_some()
                {
                    p.unparent();
                }
            });
        }
    }

    fn newline_with_indent(&self) {
        let buf = &self.buffer;
        buf.begin_user_action();
        buf.delete_selection(true, true);
        let cursor = buf.iter_at_mark(&buf.get_insert());
        let mut line_start = cursor;
        line_start.set_line_offset(0);
        let line_text = buf.text(&line_start, &cursor, false);
        let indent: String = line_text
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        buf.insert_at_cursor(&format!("\n{indent}"));
        buf.end_user_action();
        self.view.scroll_mark_onscreen(&buf.get_insert());
    }

    /// Commenta/decommenta con `-- ` le righe selezionate (Ctrl+/).
    fn toggle_comment(&self) {
        let buf = &self.buffer;
        let (s, e) = buf.selection_bounds().unwrap_or_else(|| {
            let c = buf.iter_at_mark(&buf.get_insert());
            (c, c)
        });
        let (first, mut last) = (s.line(), e.line());
        if last > first && e.line_offset() == 0 {
            last -= 1;
        }
        let lines: Vec<String> = (first..=last)
            .map(|l| {
                let a = buf.iter_at_line(l).unwrap();
                let mut b = a;
                if !b.ends_line() {
                    b.forward_to_line_end();
                }
                buf.text(&a, &b, false).to_string()
            })
            .collect();
        let all_commented = lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .all(|l| l.trim_start().starts_with("--"));
        buf.begin_user_action();
        for (i, text) in lines.iter().enumerate() {
            let l = first + i as i32;
            if text.trim().is_empty() {
                continue;
            }
            if all_commented {
                let ws = text.len() - text.trim_start().len();
                let ws_chars = text[..ws].chars().count() as i32;
                let rest = &text.trim_start()[2..];
                let n = if rest.starts_with(' ') { 3 } else { 2 };
                let mut a = buf.iter_at_line_offset(l, ws_chars).unwrap();
                let mut b = buf.iter_at_line_offset(l, ws_chars + n).unwrap();
                buf.delete(&mut a, &mut b);
            } else {
                let mut a = buf.iter_at_line(l).unwrap();
                buf.insert(&mut a, "-- ");
            }
        }
        buf.end_user_action();
    }
}

fn kind_badge(kind: ItemKind) -> (&'static str, &'static str, &'static str) {
    match kind {
        ItemKind::Column => ("C", "comp-column", "colonna"),
        ItemKind::Table => ("T", "comp-table", "tabella"),
        ItemKind::View => ("V", "comp-view", "vista"),
        ItemKind::Schema => ("S", "comp-schema", "schema"),
        ItemKind::Keyword => ("K", "comp-keyword", "parola chiave"),
    }
}

/// Popup dei suggerimenti: non prende il focus, la tastiera resta all'editor.
fn build_popup() -> Popup {
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let selection = gtk::SingleSelection::new(Some(store.clone()));
    selection.set_autoselect(true);
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let badge = gtk::Label::new(None);
        badge.add_css_class("comp-badge");
        badge.set_width_chars(2);
        let name = gtk::Label::new(None);
        name.set_xalign(0.0);
        name.add_css_class("comp-name");
        let detail = gtk::Label::new(None);
        detail.set_xalign(1.0);
        detail.set_hexpand(true);
        detail.set_ellipsize(pango::EllipsizeMode::Start);
        detail.add_css_class("comp-detail");
        row.append(&badge);
        row.append(&name);
        row.append(&detail);
        item.set_child(Some(&row));
        item.set_focusable(false);
    });
    factory.connect_bind(|_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let Some(data) = item.item().and_downcast::<glib::BoxedAnyObject>() else {
            return;
        };
        let it = data.borrow::<Item>();
        let row = item.child().and_downcast::<gtk::Box>().unwrap();
        let badge = row.first_child().and_downcast::<gtk::Label>().unwrap();
        let name = badge.next_sibling().and_downcast::<gtk::Label>().unwrap();
        let detail = name.next_sibling().and_downcast::<gtk::Label>().unwrap();
        let (letter, class, tip) = kind_badge(it.kind);
        badge.set_text(letter);
        badge.set_tooltip_text(Some(tip));
        badge.set_css_classes(&["comp-badge", class]);
        name.set_text(&it.label);
        detail.set_text(&it.detail);
    });
    let list = gtk::ListView::new(Some(selection.clone()), Some(factory));
    list.set_single_click_activate(true);
    list.set_can_focus(false);
    list.add_css_class("completion-list");
    let sc = gtk::ScrolledWindow::new();
    sc.set_child(Some(&list));
    sc.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    sc.set_propagate_natural_height(true);
    sc.set_max_content_height(280);
    sc.set_width_request(460);
    let popover = gtk::Popover::new();
    popover.set_child(Some(&sc));
    popover.set_autohide(false);
    popover.set_has_arrow(false);
    popover.set_can_focus(false);
    popover.set_position(gtk::PositionType::Bottom);
    popover.set_halign(gtk::Align::Start);
    popover.add_css_class("completion-popup");
    Popup {
        popover,
        list,
        selection,
        store,
    }
}
