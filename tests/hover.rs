//! Pointing at an answer.
//!
//! Separate from `widgets.rs` because this one has to *present* a window: a
//! `GtkTextView` reports no size until it is mapped, and where a link is on the
//! screen is the whole question here.
//!
//! What it guards is an abort, not a wrong answer. `GtkTextView`'s own
//! pixel-to-character conversion calls `g_error` — "byte index off the end of
//! the line" — on a buffer that has invisible text in it, and every answer has
//! invisible text in it, because that is how the Markdown syntax is hidden.
//! Moving the pointer across a reply killed the app. So the hit-test asks where
//! the *links* are instead, and this points at every few pixels of an answer
//! shaped like the one that crashed.

use adw::prelude::*;
use familiar::model::thread::StoredTurn;
use familiar::ui::TurnView;
use gtk::glib;

/// Long lines, hidden syntax on nearly all of them, links at the ends: the
/// shape of a briefing, which is what was on the screen when it aborted.
fn briefing() -> String {
    let mut text = String::from("### Politics & Regulation\n\n");
    for (topic, slug) in [
        ("OpenAI's super PAC", "openai-pac"),
        ("EU AI model rules", "eu-ai-act"),
        ("The AI bubble", "bubble"),
    ] {
        text.push_str(&format!(
            "- **{topic}** — a long line of the shape a briefing actually has, with \
             a [source](https://example.com/{slug}) in the middle of it and enough \
             words either side to wrap two or three times at any width worth \
             testing, including *emphasis* and `code` along the way.\n"
        ));
    }
    text.push_str("\nSee https://example.com/everything for the rest.\n\n");
    text.push_str(
        "| Story | Source |\n|---|---|\n| AI Act | [EuroNews](https://euronews.com/ai) |\n",
    );
    text
}

#[test]
fn pointing_anywhere_at_an_answer_is_safe() {
    adw::init().expect("GTK and libadwaita initialise");

    for width in [420, 720] {
        let view = TurnView::replayed(
            &StoredTurn {
                user: "what happened?".into(),
                answer: briefing(),
                ..Default::default()
            },
            true,
        );

        let window = gtk::Window::new();
        window.set_default_size(width, 900);
        window.set_child(Some(view.widget()));
        window.present();

        let main = glib::MainLoop::new(None, false);
        let quit = main.clone();
        let widget = view.widget().clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(200), move || {
            let answer = answer_view(widget.clone().upcast()).expect("the answer view");
            let (width, height) = (answer.width(), answer.height());
            assert!(height > 0, "the answer was never laid out");

            let mut hits = 0;
            for y in (-4..height + 4).step_by(4) {
                for x in (-4..width + 4).step_by(4) {
                    if familiar::ui::link_at(&answer, f64::from(x), f64::from(y)).is_some() {
                        hits += 1;
                    }
                }
            }
            // Which also says the hit-test still finds what it is there for: a
            // sweep that never lands on a link would survive anything.
            assert!(hits > 0, "no point in {width}x{height} was on a link");
            quit.quit();
        });
        main.run();
        window.destroy();
    }
}

fn answer_view(widget: gtk::Widget) -> Option<gtk::TextView> {
    if let Ok(view) = widget.clone().downcast::<gtk::TextView>() {
        return Some(view);
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Some(found) = answer_view(current.clone()) {
            return Some(found);
        }
        child = current.next_sibling();
    }
    None
}
