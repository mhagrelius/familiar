use familiar::ui::Application;
use gtk::prelude::*;

fn main() -> gtk::glib::ExitCode {
    gtk::glib::set_application_name("Familiar");
    gtk::glib::set_prgname(Some(familiar::APP_ID));
    Application::new().run()
}
