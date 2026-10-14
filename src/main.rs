//! Lemure — client SQL desktop (GTK4) per MySQL e PostgreSQL.

mod ui;

use gtk::prelude::*;
use gtk::{gio, glib};

const APP_ID: &str = "it.lemure.Lemure";

fn main() -> glib::ExitCode {
    let application = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    application.connect_startup(|_| {
        // Icona installata in hicolor (vedi data/ e packaging/arch).
        gtk::Window::set_default_icon_name(APP_ID);
    });
    application.connect_activate(|application| {
        let app = ui::app::App::new(application);
        app.present();
        #[cfg(feature = "devtools")]
        ui::devtools::start(&app);
        // L'oggetto App vive quanto la finestra.
        let keep = app.clone();
        app.window.connect_destroy(move |_| {
            let _ = &keep;
        });
    });
    application.run()
}
