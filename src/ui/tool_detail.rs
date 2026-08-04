//! What a tool chip is hiding.
//!
//! A chip has room for a tool's name and one argument, which for `web_search`
//! is most of the story and for `run_python` is almost none of it. The rule the
//! chips exist to serve — "done" must not be able to mask a no-op or an error —
//! only half works if the result itself is never visible. So the chip is a
//! button, and this is what it opens.
//!
//! Deliberately not a JSON viewer. The model sends an object and nobody wants
//! to read one: [`ToolCall::fields`] unpacks it into labelled values, short
//! ones become rows, and only a script or a page of Markdown gets a block of
//! monospace. The result is shown as it was handed to the model, because that
//! is the thing being explained — what the model saw is the whole question
//! anyone opens this to answer.

use adw::prelude::*;
use gtk::glib;

use crate::model::turn::{Field, ToolCall, ToolOutcome};

/// How much of a result is put on screen.
///
/// A `read_file` over something enormous is already capped by the workspace,
/// but a fetched page is not, and a dialog is not where anybody reads 200 KB.
/// The cut is announced rather than silent — an explanation that quietly leaves
/// things out is worse than no explanation.
const MAX_SHOWN: usize = 20_000;

/// Present the detail for one call, over whatever it was invoked from.
pub fn present(call: &ToolCall, over: &impl IsA<gtk::Widget>) {
    dialog_for(call).present(Some(over));
}

/// The dialog, built but not shown.
///
/// Separate from [`present`] so that the widget tests and the preview can build
/// one without a compositor. Presenting is the only thing that needs a screen,
/// and it is the one thing that is not worth testing.
pub fn dialog_for(call: &ToolCall) -> adw::Dialog {
    let toasts = adw::ToastOverlay::new();
    let dialog = adw::Dialog::builder()
        .title("Tool Call")
        .content_width(560)
        .content_height(620)
        .child(&toasts)
        .build();

    let view = adw::ToolbarView::new();
    view.add_top_bar(&header(call));
    view.set_content(Some(&body(call, &toasts)));
    toasts.set_child(Some(&view));
    dialog
}

/// The header: which tool, and how it went.
fn header(call: &ToolCall) -> adw::HeaderBar {
    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new(&call.name, status_of(call));
    header.set_title_widget(Some(&title));
    header
}

/// What happened, in the two or three words a subtitle has room for.
fn status_of(call: &ToolCall) -> &'static str {
    match call.outcome.as_ref() {
        Some(ToolOutcome::Ok(_)) => "Finished",
        Some(ToolOutcome::Failed(_)) => "Failed",
        Some(ToolOutcome::Denied) => "Not run — you declined it",
        None => "Running",
    }
}

fn body(call: &ToolCall, toasts: &adw::ToastOverlay) -> gtk::ScrolledWindow {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(12);
    content.set_margin_bottom(24);
    content.set_margin_start(12);
    content.set_margin_end(12);

    let fields = call.fields();
    let (rows, blocks): (Vec<Field>, Vec<Field>) =
        fields.into_iter().partition(|field| !field.is_block());

    // The short arguments together in one boxed list, which is what a list of
    // labelled values is for.
    let asked = adw::PreferencesGroup::builder().title("Arguments").build();
    if rows.is_empty() && blocks.is_empty() {
        let none = adw::ActionRow::builder()
            .title("No arguments")
            .subtitle("This tool was called with nothing.")
            .build();
        none.add_css_class("dimmed");
        asked.add(&none);
    }
    for field in &rows {
        let row = adw::ActionRow::builder()
            .title(&field.key)
            .subtitle(&field.value)
            .subtitle_selectable(true)
            .build();
        // Long-ish values still fit here; `is_block` already sent the genuinely
        // long ones to a block of their own.
        row.set_subtitle_lines(0);
        asked.add(&row);
    }
    if !rows.is_empty() || blocks.is_empty() {
        content.append(&asked);
    }

    // A script or a page of Markdown gets its own titled block. Squeezing one
    // into a subtitle is how a twenty-line script becomes unreadable.
    for field in &blocks {
        let group = adw::PreferencesGroup::builder().title(&field.key).build();
        group.set_header_suffix(Some(&copy_button(&field.value, toasts)));
        group.add(&monospace(&field.value));
        content.append(&group);
    }

    content.append(&result(call, toasts));

    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&content)
        .build()
}

/// What came back, framed as what the model was handed.
fn result(call: &ToolCall, toasts: &adw::ToastOverlay) -> adw::PreferencesGroup {
    let (title, text) = match call.outcome.as_ref() {
        Some(ToolOutcome::Ok(said)) => ("Result", said.clone()),
        Some(ToolOutcome::Failed(why)) => ("Error", format!("Error: {why}")),
        Some(ToolOutcome::Denied) => ("Result", "The user declined to run this tool.".to_string()),
        None => ("Result", String::new()),
    };

    let group = adw::PreferencesGroup::builder()
        .title(title)
        .description(match call.outcome.as_ref() {
            // Worth saying once, here: this is not our summary of what
            // happened, it is the text the model read and answered from.
            Some(_) => "What the assistant was given back.",
            None => "This is still running.",
        })
        .build();

    if text.trim().is_empty() {
        let waiting = gtk::Label::new(Some(match call.outcome.as_ref() {
            Some(_) => "It returned nothing.",
            None => "Waiting for it to finish…",
        }));
        waiting.set_xalign(0.0);
        waiting.add_css_class("dimmed");
        waiting.set_margin_top(6);
        group.add(&waiting);
        return group;
    }

    let shown = elide(&text);
    group.set_header_suffix(Some(&copy_button(&text, toasts)));
    let block = monospace(&shown);
    // The chip is already red or amber; carrying that through means a glance at
    // the detail says how it went before a word of it has been read.
    match call.outcome.as_ref() {
        Some(ToolOutcome::Failed(_)) => block.add_css_class("tool-result-failed"),
        Some(ToolOutcome::Denied) => block.add_css_class("tool-result-denied"),
        _ => {}
    }
    group.add(&block);
    group
}

/// A block of text, styled as the code-ish thing it usually is.
///
/// A `GtkTextView` in a card rather than a wrapped label: the result can be
/// hundreds of lines, and a label that long makes the whole dialog one
/// unscrollable column.
fn monospace(text: &str) -> gtk::Widget {
    let view = gtk::TextView::new();
    view.buffer().set_text(text);
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.set_left_margin(10);
    view.set_right_margin(10);
    view.set_top_margin(8);
    view.set_bottom_margin(8);
    view.add_css_class("monospace");
    view.add_css_class("caption");

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        // Tall enough to read a stack trace, short enough that two blocks do
        // not turn the dialog into a scroll hunt.
        .max_content_height(280)
        .child(&view)
        .build();
    scroller.add_css_class("card");
    scroller.set_margin_top(6);
    scroller.upcast()
}

fn copy_button(text: &str, toasts: &adw::ToastOverlay) -> gtk::Button {
    let button = gtk::Button::from_icon_name("edit-copy-symbolic");
    button.set_tooltip_text(Some("Copy"));
    button.set_valign(gtk::Align::Center);
    button.add_css_class("flat");

    let text = text.to_string();
    let toasts = toasts.clone();
    button.connect_clicked(move |button| {
        button.clipboard().set_text(&text);
        toasts.add_toast(adw::Toast::new("Copied"));
    });
    button
}

/// Enough to explain the turn, and a note when there was more.
fn elide(text: &str) -> String {
    if text.chars().count() <= MAX_SHOWN {
        return text.to_string();
    }
    let kept: String = text.chars().take(MAX_SHOWN).collect();
    format!(
        "{kept}\n\n[…{} characters more]",
        text.chars().count() - MAX_SHOWN
    )
}

/// A chip that opens its own detail.
///
/// A `GtkButton` rather than a box with a click handler, because that is what
/// buys focus, Enter and Space, and a screen reader that says the thing is
/// activatable — none of which a `GtkGestureClick` on a box provides.
pub fn clickable(chip: &impl IsA<gtk::Widget>, call: &ToolCall) -> gtk::Button {
    let button = gtk::Button::builder()
        .child(chip)
        .tooltip_text("Show what this tool was asked and what it returned")
        .build();
    button.add_css_class("tool-chip-button");

    let call = call.clone();
    button.connect_clicked(glib::clone!(
        #[strong]
        call,
        move |button| present(&call, button)
    ));
    button
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_longer_than_the_dialog_says_how_much_was_left_out() {
        let long = "x".repeat(MAX_SHOWN + 500);
        let shown = elide(&long);
        assert!(shown.contains("500 characters more"), "{}", &shown[..80]);
        assert!(shown.chars().count() < long.chars().count());
        assert_eq!(elide("short"), "short");
    }

    #[test]
    fn every_outcome_has_something_to_say_for_itself() {
        let call = |outcome| ToolCall {
            id: "1".into(),
            name: "web_search".into(),
            arguments: r#"{"query":"x"}"#.into(),
            complete: true,
            outcome,
        };
        assert_eq!(status_of(&call(None)), "Running");
        assert_eq!(
            status_of(&call(Some(ToolOutcome::Ok("x".into())))),
            "Finished"
        );
        assert_eq!(
            status_of(&call(Some(ToolOutcome::Failed("x".into())))),
            "Failed"
        );
        assert!(status_of(&call(Some(ToolOutcome::Denied))).contains("declined"));
    }
}
