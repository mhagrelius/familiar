//! The gate.
//!
//! A tool the model asked to run that changes something outside the vault
//! pauses the turn and asks. Cancel first, the specific verb last, destructive
//! appearance on the verb — and the arguments shown in full, because approving
//! something you cannot see is not approving it.
//!
//! Denying is not an error: the model is told it was declined and the turn
//! carries on, which is how you steer it rather than start again.

use adw::prelude::*;
use gtk::glib::clone;

/// What the person decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Deny,
}

/// Ask before running `name` with `arguments`.
pub fn ask<F>(parent: &impl IsA<gtk::Widget>, name: &str, arguments: &str, decide: F)
where
    F: Fn(Decision) + 'static,
{
    let dialog = adw::AlertDialog::new(
        Some(&format!("Run “{name}”?")),
        Some("The assistant wants to do something outside your notes."),
    );
    dialog.add_response("deny", "Don't Run");
    dialog.add_response("approve", "Run");
    dialog.set_response_appearance("approve", adw::ResponseAppearance::Destructive);
    // The safe one is the default, and Escape means the same as declining.
    dialog.set_default_response(Some("deny"));
    dialog.set_close_response("deny");

    let arguments = pretty(arguments);
    let view = gtk::TextView::new();
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_monospace(true);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.set_top_margin(8);
    view.set_bottom_margin(8);
    view.set_left_margin(8);
    view.set_right_margin(8);
    view.buffer().set_text(&arguments);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .max_content_height(220)
        .propagate_natural_height(true)
        .child(&view)
        .build();
    scroller.add_css_class("card");
    dialog.set_extra_child(Some(&scroller));

    dialog.connect_response(
        None,
        clone!(move |_: &adw::AlertDialog, response: &str| {
            decide(match response {
                "approve" => Decision::Approve,
                _ => Decision::Deny,
            });
        }),
    );

    dialog.present(Some(parent));
}

/// Arguments as something readable. They arrive as a JSON string, and a wall of
/// escaped quotes is not a thing anyone can consent to.
fn pretty(arguments: &str) -> String {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| arguments.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_shown_as_something_a_person_can_read() {
        let shown = pretty(r#"{"path":"/etc/hosts","contents":"x"}"#);
        assert!(shown.contains("\"path\": \"/etc/hosts\""), "{shown}");
        assert!(shown.contains('\n'), "{shown}");
    }

    #[test]
    fn half_written_arguments_are_shown_as_they_are() {
        // A stream cut off mid-call still has to be describable, or the dialog
        // would show nothing at the moment it matters most.
        assert_eq!(pretty("{\"path\":"), "{\"path\":");
    }
}
