//! Piccole utilità condivise dalla UI.

use gtk::prelude::*;
use gtk::{gdk, gio, glib};

/// Mostra un menu contestuale in corrispondenza del punto (x, y) di `parent`.
/// Le azioni del menu vengono cercate nei gruppi di azioni di `parent` e dei suoi antenati.
pub fn popup_menu(parent: &impl IsA<gtk::Widget>, menu: &gio::Menu, x: f64, y: f64) {
    let pop = gtk::PopoverMenu::from_model(Some(menu));
    pop.set_parent(parent);
    pop.set_has_arrow(false);
    pop.set_halign(gtk::Align::Start);
    pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    pop.connect_closed(|p| {
        // L'unparent va rimandato: l'azione scelta viene attivata dopo "closed".
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });
    pop.popup();
}

pub fn copy_to_clipboard(widget: &impl IsA<gtk::Widget>, text: &str) {
    widget.clipboard().set_text(text);
}

pub fn root_window(widget: &impl IsA<gtk::Widget>) -> Option<gtk::Window> {
    widget.root().and_then(|r| r.downcast::<gtk::Window>().ok())
}

pub fn show_error(parent: Option<&gtk::Window>, title: &str, detail: &str) {
    let d = gtk::AlertDialog::builder()
        .message(title)
        .detail(detail)
        .modal(true)
        .build();
    d.show(parent);
}

pub async fn confirm(parent: Option<&gtk::Window>, title: &str, detail: &str, ok: &str) -> bool {
    let d = gtk::AlertDialog::builder()
        .message(title)
        .detail(detail)
        .modal(true)
        .buttons(["Annulla", ok])
        .cancel_button(0)
        .default_button(1)
        .build();
    matches!(d.choose_future(parent).await, Ok(1))
}

/// Testo da mostrare in una cella: una sola riga e lunghezza limitata, così
/// il rendering resta veloce anche con valori enormi (il valore completo
/// resta disponibile per la copia).
pub fn cell_display(s: &str) -> std::borrow::Cow<'_, str> {
    const MAX: usize = 400;
    let needs_cut = s.len() > MAX;
    let needs_fix = s.bytes().any(|b| b == b'\n' || b == b'\r' || b == b'\t');
    if !needs_cut && !needs_fix {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len().min(MAX + 4));
    for (i, ch) in s.chars().enumerate() {
        if i >= MAX {
            out.push('…');
            break;
        }
        out.push(match ch {
            '\n' => '↵',
            '\r' => ' ',
            '\t' => ' ',
            c => c,
        });
    }
    std::borrow::Cow::Owned(out)
}

/// Formatta un numero con il separatore delle migliaia italiano.
pub fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(ch);
    }
    out
}

pub fn fmt_duration(d: std::time::Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{:.2} s", ms / 1000.0)
    }
}

/// Bottone con sola icona e tooltip.
pub fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.set_tooltip_text(Some(tooltip));
    b.add_css_class("flat");
    b
}

/// Bottone con icona ed etichetta.
pub fn labeled_button(icon: &str, label: &str, tooltip: &str) -> gtk::Button {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    content.append(&gtk::Image::from_icon_name(icon));
    content.append(&gtk::Label::new(Some(label)));
    let b = gtk::Button::new();
    b.set_child(Some(&content));
    b.set_tooltip_text(Some(tooltip));
    b.add_css_class("flat");
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counts() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(1234567), "1.234.567");
        assert_eq!(fmt_count(500), "500");
    }
    #[test]
    fn display_cut() {
        assert_eq!(cell_display("a\nb"), "a↵b");
        assert!(cell_display(&"x".repeat(1000)).ends_with('…'));
    }
}
