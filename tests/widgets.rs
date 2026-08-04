//! Widget tests.
//!
//! **Exactly one `#[test]` touches a widget, on purpose.** GTK is
//! thread-affine: it must be initialised on, and only ever touched from, one
//! thread. `cargo test` runs tests on separate threads and `--test-threads=1`
//! still does not guarantee they share one, so a second `#[test]` building a
//! widget is a crash waiting for a scheduler to find it. The cases are
//! therefore a hand-rolled runner inside `widgets`, reporting each by name.
//!
//! Windows are constructed and driven but never presented — mapping a window
//! needs a compositor, and none of these assertions need one.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use chrono::Utc;
use gtk::glib;

use familiar::model::project::{Project, ThreadSummary, DEFAULT_PROJECT};
use familiar::model::thread::{StoredTurn, ThreadId};
use familiar::model::turn::TurnMetrics;
use familiar::model::turn::{Event, Finish, TurnState};
use familiar::ui::file_tree::FileRow;
use familiar::ui::sidebar::Row;
use familiar::ui::{Chip, Composer, Sidebar, ToolChip, TurnView, Window};

/// Cases run in order; each gets a fresh window.
type Case = (&'static str, fn(&Window));

#[test]
fn widgets() {
    // GTK is initialised directly rather than by running the application: the
    // real `activate` presents a window and probes the server, neither of which
    // belongs in a test.
    //
    // The windows below are built with no application attached at all.
    // Attaching one that has not emitted `startup` earns a `Gtk-CRITICAL` per
    // window, and noise like that is what hides a real one.
    adw::init().expect("GTK and libadwaita initialise");

    let mut failures = Vec::<String>::new();
    for (name, case) in CASES {
        let window: Window = glib::Object::new();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| case(&window)));
        if let Err(panic) = result {
            let message = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "panicked".to_string());
            failures.push(format!("{name}: {message}"));
        }

        window.destroy();
    }

    assert!(failures.is_empty(), "\n  {}", failures.join("\n  "));
}

const CASES: &[Case] = &[
    ("a chat with no workflow shows no strip at all", |window| {
        // The commonest case by far. A capability that is switched on but
        // unused must cost nothing on screen.
        let bar = window.workflow_bar();
        bar.set_workflow(None);
        assert!(bar.workflow().is_none());
    }),
    (
        "a proposed workflow offers Start and a running one offers Stop",
        |window| {
            use familiar::model::workflow::{State, Workflow};

            let bar = window.workflow_bar();
            let mut flow = Workflow::proposed(
                "Quarterly comparison",
                vec!["read the figures".into(), "write it up".into()],
            )
            .expect("a workflow");

            bar.set_workflow(Some(&flow));
            assert_eq!(
                bar.workflow().as_ref().map(|f| f.goal.clone()).as_deref(),
                Some("Quarterly comparison")
            );
            assert!(bar.start_visible(), "a plan nobody greenlit offers Start");
            assert!(!bar.stop_visible());
            assert!(
                bar.summary_text().contains("2 steps proposed"),
                "{}",
                bar.summary_text()
            );

            // Started: Start would now repeat what already happened, and there
            // is at most one suggested action in a view.
            flow.advance(State::Done {
                outcome: "4 sheets".into(),
            })
            .expect("advance");
            bar.set_workflow(Some(&flow));
            assert!(!bar.start_visible());
            assert!(bar.stop_visible());
            assert_eq!(bar.summary_text(), "Step 2 of 2 · write it up");
        },
    ),
    (
        "a plan nobody has started shows no step as under way",
        |window| {
            use familiar::model::workflow::{State, Workflow};

            // Caught on screen, not in a test: `Workflow::current` is the first
            // unsettled step whether or not anybody said go, so step 1 of a
            // proposal wore the accent "doing this now" marker while nothing at
            // all was happening — the one thing a plan awaiting approval must
            // not look like.
            let bar = window.workflow_bar();
            let mut flow = Workflow::proposed(
                "Quarterly comparison",
                vec![
                    "read the figures".into(),
                    "write it up".into(),
                    "send it".into(),
                ],
            )
            .expect("a workflow");

            bar.set_workflow(Some(&flow));
            assert_eq!(
                bar.step_states(),
                ["Not started", "Not started", "Not started"]
            );

            flow.advance(State::Done {
                outcome: "4 sheets".into(),
            })
            .expect("advance");
            bar.set_workflow(Some(&flow));
            assert_eq!(bar.step_states(), ["Done", "Doing this now", "Not started"]);

            flow.advance(State::Stuck {
                why: "no access".into(),
            })
            .expect("advance");
            bar.set_workflow(Some(&flow));
            // Stuck stops the run, so nothing is under way behind it either.
            assert_eq!(bar.step_states(), ["Done", "Stuck", "Not started"]);
        },
    ),
    ("an image can be staged, seen, and taken back", |window| {
        let staging = window.composer().staging();
        assert!(staging.is_empty());
        assert!(!staging.get_visible(), "an empty staging area is not drawn");

        assert_eq!(staging.add(PNG.to_vec()), None, "a PNG should just attach");
        assert_eq!(staging.len(), 1);
        assert!(staging.get_visible());

        // Content-addressed: the same picture twice is the same picture.
        let again = staging.add(PNG.to_vec());
        assert!(again.is_some_and(|why| why.contains("already attached")));
        assert_eq!(staging.len(), 1);

        let taken = staging.take();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].media_type, "image/png");
        assert!(staging.is_empty(), "sending empties the staging area");
        assert!(!staging.get_visible());
    }),
    (
        "something that is not an image is refused with a reason",
        |window| {
            let staging = window.composer().staging();
            let refused = staging.add(b"this is a text file".to_vec());
            assert!(refused.is_some_and(|why| why.contains("not an image")));
            assert!(staging.is_empty());
        },
    ),
    ("an image with no words is still a question", |window| {
        // "What is this?" is implied. Requiring text as well would make
        // pasting a screenshot a two-step operation for no reason.
        let composer = window.composer();
        let sent = record_submissions(&composer);
        composer.staging().add(PNG.to_vec());

        assert!(send_button(&composer).is_sensitive(), "nothing to send?");
        send_button(&composer).emit_clicked();
        assert_eq!(sent.borrow().as_slice(), [""]);
    }),
    ("a turn shows the images it was asked with", |_| {
        let view = TurnView::replayed(&answered("what is this?", "a pixel"), true);
        let texture =
            gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from(PNG)).expect("a texture");
        view.set_images(std::slice::from_ref(&texture));
        assert_eq!(
            count::<gtk::Picture>(view.widget().clone().upcast()),
            1,
            "the image is not shown with its question"
        );
    }),
    ("every shipped icon actually loads", |_| {
        // A valid SVG is not enough. gdk-pixbuf sniffs roughly the first 256
        // bytes to decide a file is SVG, so a long header comment pushes the
        // <svg> element out of the window and the icon silently fails to load —
        // which looks like a blank tile in the app grid and nothing else.
        let icons = std::path::Path::new("data/icons");
        let mut checked = 0;
        for entry in walkdir(icons) {
            let path = entry.to_string_lossy().to_string();
            let texture = gtk::gdk::Texture::from_filename(&entry);
            assert!(texture.is_ok(), "{path}: {:?}", texture.err());
            checked += 1;

            let bytes = std::fs::read(&entry).expect("the icon");
            let at = bytes
                .windows(4)
                .position(|window| window == b"<svg")
                .unwrap_or(usize::MAX);
            assert!(
                at < 256,
                "{path}: <svg> starts at byte {at}, past the sniff window"
            );
        }
        assert!(checked >= 2, "no icons were checked");
    }),
    // -- the composer --------------------------------------------------------
    ("the composer offers a way to attach a file", |window| {
        // Pasting and dropping are invisible; this is the only thing on screen
        // that says a question can carry a file, so it is always there and
        // always live — attaching does not depend on the server.
        let attach =
            button_with_icon(&window.composer(), "list-add-symbolic").expect("the attach button");
        assert!(attach.is_sensitive());
        assert!(attach.tooltip_text().is_some(), "an icon with no tooltip");
    }),
    ("an empty composer cannot send", |window| {
        let composer = window.composer();
        let button = send_button(&composer);
        assert!(
            !button.is_sensitive(),
            "the button should be insensitive with nothing typed"
        );

        let sent = record_submissions(&composer);
        button.emit_clicked();
        assert!(sent.borrow().is_empty(), "{:?}", sent.borrow());
    }),
    (
        "typing enables the button and sending clears the entry",
        |window| {
            let composer = window.composer();
            let sent = record_submissions(&composer);

            set_text(&composer, "what is due?");
            assert!(send_button(&composer).is_sensitive());

            send_button(&composer).emit_clicked();
            assert_eq!(sent.borrow().as_slice(), ["what is due?"]);
            // Cleared before the signal, so the next question can be typed while
            // the answer streams.
            assert_eq!(composer.text(), "");
        },
    ),
    ("whitespace is not a question", |window| {
        let composer = window.composer();
        let sent = record_submissions(&composer);
        set_text(&composer, "   \n  ");
        send_button(&composer).emit_clicked();
        assert!(sent.borrow().is_empty(), "{:?}", sent.borrow());
    }),
    ("while busy the same button stops instead", |window| {
        let composer = window.composer();
        let stopped = Rc::new(RefCell::new(0));
        composer.connect_closure(
            "stop",
            false,
            glib::closure_local!(
                #[strong]
                stopped,
                move |_: Composer| *stopped.borrow_mut() += 1
            ),
        );

        composer.set_busy(true);
        let button = send_button(&composer);
        // Stopping is always available, even with nothing typed.
        assert!(button.is_sensitive());
        assert_eq!(
            button.icon_name().as_deref(),
            Some("media-playback-stop-symbolic")
        );

        button.emit_clicked();
        assert_eq!(*stopped.borrow(), 1);

        composer.set_busy(false);
        assert_eq!(
            button.icon_name().as_deref(),
            Some("document-send-symbolic")
        );
        assert!(
            !button.is_sensitive(),
            "nothing is typed, so there is nothing to send"
        );
    }),
    (
        "an unreachable server disables sending with a reason",
        |window| {
            let composer = window.composer();
            set_text(&composer, "hello");
            composer.set_reachable(false, Some("No llama-server to send to"));

            let button = send_button(&composer);
            assert!(!button.is_sensitive());
            // Present and explained, never latching while nothing happens.
            assert_eq!(
                button.tooltip_text().as_deref(),
                Some("No llama-server to send to")
            );

            composer.set_reachable(true, None);
            assert!(button.is_sensitive());
        },
    ),
    // -- the conversation ----------------------------------------------------
    ("a conversation starts empty and fills up", |window| {
        let conversation = window.conversation();
        assert_eq!(visible_page(&conversation), Some("empty".to_string()));

        let view = TurnView::replayed(&answered("why?", "because."), true);
        conversation.append(view.widget());
        assert_eq!(visible_page(&conversation), Some("turns".to_string()));

        conversation.clear();
        assert_eq!(visible_page(&conversation), Some("empty".to_string()));
    }),
    (
        "explaining a selection asks about it, in the words on screen",
        |window| {
            let asked = record_asks(window);
            let turn = shown(
                window,
                "why?",
                "The cache is reused for the **longest** prefix.",
            );

            let buffer = answer_buffer(&turn);
            buffer.select_range(&buffer.start_iter(), &buffer.end_iter());
            turn.activate_action("turn.explain", None)
                .expect("the turn's own action group");

            let question = asked.borrow().first().cloned().expect("nothing was asked");
            assert!(
                question.starts_with("> The cache is reused for the longest prefix."),
                "{question}"
            );
            // The markers are still in the buffer under an invisible tag, and
            // quoting them back would put asterisks in the question.
            assert!(!question.contains("**"), "{question}");
            assert!(question.contains("in more detail"), "{question}");
        },
    ),
    (
        "with nothing selected there is nothing to explain",
        |window| {
            let asked = record_asks(window);
            let turn = shown(window, "why?", "because.");

            // The item is in the menu whether or not a selection exists — it greys
            // out rather than vanishing — so activating it must be a no-op.
            turn.activate_action("turn.explain", None)
                .expect("the action");
            assert!(asked.borrow().is_empty(), "{:?}", asked.borrow());
        },
    ),
    ("the answer offers to explain what is selected", |window| {
        // Appended to the view's own Copy and Select All rather than replacing
        // them: `extra-menu` is what puts it there.
        let turn = shown(window, "why?", "because.");
        let menu = find::<gtk::TextView>(turn.clone().upcast())
            .expect("the answer view")
            .extra_menu()
            .expect("no menu on the answer at all");
        assert_eq!(menu.n_items(), 1);
        assert_eq!(
            menu.item_attribute_value(0, "label", None)
                .and_then(|label| label.str().map(str::to_string)),
            Some("Explain This".to_string())
        );
    }),
    (
        "the shortcut explains whichever answer the keyboard is in",
        |window| {
            let asked = record_asks(window);
            let first = shown(window, "one?", "the first answer.");
            let second = shown(window, "two?", "the second answer.");

            for turn in [&first, &second] {
                let buffer = answer_buffer(turn);
                buffer.select_range(&buffer.start_iter(), &buffer.end_iter());
            }
            // Two live selections, and the shortcut has to pick one. Focus is
            // what decides, because focus is where the reader just dragged.
            find::<gtk::TextView>(second.clone().upcast())
                .expect("the answer view")
                .grab_focus();
            gtk::prelude::WidgetExt::activate_action(window, "win.explain-selection", None)
                .expect("the window action");

            let question = asked.borrow().first().cloned().expect("nothing was asked");
            assert!(question.starts_with("> the second answer."), "{question}");
        },
    ),
    // -- a turn ---------------------------------------------------------------
    ("a tool chip says what ran and how it went", |_| {
        let view = TurnView::replayed(&answered("?", "done"), true);
        view.widget().set_tool_calls(&[
            chip("recall", r#"{"query":"scanner"}"#, ToolChip::Done),
            chip("write_file", r#"{"path":"/etc/hosts"}"#, ToolChip::Denied),
        ]);

        let texts = labels(view.widget().clone().upcast());
        // The argument is on the chip: "done" must not be able to mask which
        // call it was.
        assert!(texts.contains(&"recall · scanner".to_string()), "{texts:?}");
        assert!(
            texts.contains(&"write_file · /etc/hosts".to_string()),
            "{texts:?}"
        );

        // And they are replaced, not appended to, when the calls settle.
        view.widget()
            .set_tool_calls(&[chip("recall", "{}", ToolChip::Done)]);
        let texts = labels(view.widget().clone().upcast());
        assert!(!texts.iter().any(|t| t.contains("write_file")), "{texts:?}");
    }),
    (
        "a chip is a button, so its detail opens from the keyboard",
        |_| {
            // A box with a click gesture is not reachable by Tab and says nothing
            // to a screen reader. The pill has to be inside a real button.
            let view = TurnView::replayed(&answered("?", "done"), true);
            view.widget().set_tool_calls(&[chip(
                "recall",
                r#"{"query":"scanner"}"#,
                ToolChip::Done,
            )]);

            let button =
                find::<gtk::Button>(view.widget().clone().upcast()).expect("a chip is a button");
            assert!(button.is_focusable(), "the chip cannot be tabbed to");
            assert!(button.tooltip_text().is_some(), "no tooltip on the chip");
        },
    ),
    (
        "the detail shows the arguments as fields and the result whole",
        |_| {
            let call = familiar::model::turn::ToolCall {
                id: "1".into(),
                name: "remember".into(),
                arguments: r#"{"subject":"Roof","related_to":"Contractors"}"#.into(),
                complete: true,
                outcome: Some(familiar::model::turn::ToolOutcome::Ok(
                    "Saved to Familiar/Roof.md as a fact.".into(),
                )),
            };
            let dialog = familiar::ui::tool_detail::dialog_for(&call);
            let texts = labels(dialog.child().expect("the dialog's content"));

            // The keys are labels, not JSON.
            assert!(texts.contains(&"Subject".to_string()), "{texts:?}");
            assert!(texts.contains(&"Related to".to_string()), "{texts:?}");
            assert!(texts.contains(&"Roof".to_string()), "{texts:?}");
            // The tool and how it went are in the header.
            assert!(texts.contains(&"remember".to_string()), "{texts:?}");
            assert!(texts.contains(&"Finished".to_string()), "{texts:?}");
            // And the result is somewhere in the tree, in a text view.
            let shown = text_views(dialog.child().expect("the dialog's content"));
            assert!(
                shown
                    .iter()
                    .any(|t| t.contains("Saved to Familiar/Roof.md")),
                "the result is not shown: {shown:?}"
            );
        },
    ),
    (
        "a failed call shows the error rather than an empty result",
        |_| {
            let call = familiar::model::turn::ToolCall {
                id: "1".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"budget.md"}"#.into(),
                complete: true,
                outcome: Some(familiar::model::turn::ToolOutcome::Failed(
                    "budget.md is not there".into(),
                )),
            };
            let dialog = familiar::ui::tool_detail::dialog_for(&call);
            let texts = labels(dialog.child().expect("content"));
            assert!(texts.contains(&"Failed".to_string()), "{texts:?}");
            assert!(texts.contains(&"Error".to_string()), "{texts:?}");

            let shown = text_views(dialog.child().expect("content"));
            assert!(
                shown.iter().any(|t| t.contains("budget.md is not there")),
                "{shown:?}"
            );
        },
    ),
    (
        "a script gets a block of its own rather than a squeezed subtitle",
        |_| {
            // `run_python` is the case the row layout cannot serve: twenty lines
            // of Python in an AdwActionRow subtitle is unreadable.
            let call = familiar::model::turn::ToolCall {
                id: "1".into(),
                name: "run_python".into(),
                arguments: serde_json::json!({
                    "code": "import math\nfor n in range(5):\n    print(n, math.sqrt(n))"
                })
                .to_string(),
                complete: true,
                outcome: Some(familiar::model::turn::ToolOutcome::Ok("0 0.0".into())),
            };
            let dialog = familiar::ui::tool_detail::dialog_for(&call);
            let texts = labels(dialog.child().expect("content"));
            assert!(texts.contains(&"Code".to_string()), "{texts:?}");

            let shown = text_views(dialog.child().expect("content"));
            assert!(
                shown.iter().any(|t| t.contains("import math")),
                "the script is not shown whole: {shown:?}"
            );
        },
    ),
    (
        "a call still running says so rather than looking empty",
        |_| {
            let call = familiar::model::turn::ToolCall {
                id: "1".into(),
                name: "web_search".into(),
                arguments: r#"{"query":"zig release"}"#.into(),
                complete: true,
                outcome: None,
            };
            let dialog = familiar::ui::tool_detail::dialog_for(&call);
            let texts = labels(dialog.child().expect("content"));
            assert!(texts.contains(&"Running".to_string()), "{texts:?}");
            assert!(
                texts.iter().any(|t| t.contains("Waiting for it to finish")),
                "{texts:?}"
            );
        },
    ),
    ("a reopened turn still shows what it ran", |_| {
        // The chips used to exist only for as long as the window stayed open.
        let mut turn = answered("what did I budget?", "14,750.");
        turn.tool_calls = vec![familiar::model::thread::StoredToolCall {
            id: "1".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"budget-2026.md"}"#.into(),
            outcome: Some(familiar::model::turn::ToolOutcome::Ok("| Roof |".into())),
        }];
        let view = TurnView::replayed(&turn, true);
        let texts = labels(view.widget().clone().upcast());
        assert!(
            texts.contains(&"read_file · budget-2026.md".to_string()),
            "a reopened thread lost its chips: {texts:?}"
        );
    }),
    ("a turn with no tool calls draws no chips", |_| {
        let view = TurnView::replayed(&answered("?", "done"), true);
        view.widget().set_tool_calls(&[]);
        let wrap = find::<adw::WrapBox>(view.widget().clone().upcast()).expect("the chip row");
        assert!(!wrap.get_visible());
    }),
    ("a turn reports a height for its answer", |_| {
        // The answer is a GtkTextView, which measures zero height until its
        // buffer has been given content against a known width. If this ever
        // returns nothing, answers are invisible in the running app.
        let view = TurnView::replayed(
            &answered(
                "why?",
                "Because the scanner reports spans in character offsets, which is what a \
                 GtkTextBuffer takes, and that distinction is the whole reason it exists.",
            ),
            true,
        );
        let (_, natural, _, _) = view.widget().measure(gtk::Orientation::Vertical, 600);
        assert!(natural > 40, "a turn measured {natural}px tall");
    }),
    ("a replayed turn shows what was said", |_| {
        let view = TurnView::replayed(&answered("why?", "because."), true);
        assert_eq!(view.widget().answer_text(), "because.");
        assert!(labels(view.widget().clone().upcast()).contains(&"why?".to_string()));
        // The attribution is what stops the card reading as a text entry.
        assert!(labels(view.widget().clone().upcast()).contains(&"You".to_string()));
    }),
    ("markdown is styled and its syntax is hidden", |_| {
        let view = TurnView::replayed(
            &answered("?", "A **bold** claim and `code`.\n\n# Heading"),
            true,
        );
        let buffer = answer_buffer(view.widget());

        // The syntax characters are still in the buffer — hidden, not
        // removed — so every offset the scanner reported still lines up.
        assert!(view.widget().answer_text().contains("**bold**"));
        // And what a reader sees has none of them.
        let visible = buffer_text(&buffer);
        assert!(!visible.contains('*'), "{visible:?}");
        assert!(visible.contains("A bold claim"), "{visible:?}");

        // "A **bold** claim and `code`.\n\n# Heading"
        //   0123456789
        assert!(tag_at(&buffer, "md-bold", 5), "bold is not styled");
        assert!(tag_at(&buffer, "md-code", 23), "code is not styled");
        assert!(tag_at(&buffer, "md-h1", 32), "the heading is not styled");
        // The `**` opening the run is syntax; the word inside it is not.
        assert!(tag_at(&buffer, "md-marker", 2), "the marker is not hidden");
        assert!(
            !tag_at(&buffer, "md-marker", 5),
            "the word itself is hidden"
        );
    }),
    ("a link in an answer carries its destination", |_| {
        // Underlined and accented is what a link *looks* like; the tag naming
        // where it goes is the whole of what makes it one. Without it the
        // pointer stays an I-beam and a click does nothing, which is how this
        // shipped.
        let view = TurnView::replayed(
            &answered("what changed?", "See [EuroNews](https://euronews.com/ai)."),
            true,
        );
        let buffer = answer_buffer(view.widget());

        // "See [EuroNews](https://euronews.com/ai)."
        //  0123456789
        assert!(tag_at(&buffer, "md-link", 6), "the label is not styled");
        assert!(
            tag_at(&buffer, "md-target:https://euronews.com/ai", 6),
            "the label does not point anywhere"
        );
        // The URL itself is syntax: hidden, and not part of what is clicked.
        assert!(!buffer_text(&buffer).contains("https://"), "the URL shows");
    }),
    ("a table is drawn as a grid, not as pipes", |_| {
        let view = TurnView::replayed(
            &answered(
                "weather?",
                "Today:\n\n| Time | Temp |\n|---|---|\n| Morning | 67°F |\n\nBring a coat.",
            ),
            true,
        );
        let turn: gtk::Widget = view.widget().clone().upcast();

        let grid = find::<gtk::Grid>(turn).expect("the table grid");
        assert!(grid.child_at(0, 0).is_some(), "the header is missing");
        let cells = labels(grid.clone().upcast());
        assert!(cells.contains(&"Temp".to_string()), "{cells:?}");
        assert!(cells.contains(&"67°F".to_string()), "{cells:?}");
        assert!(!cells.iter().any(|cell| cell.contains('|')), "{cells:?}");

        // The source is hidden rather than removed: the prose around it still
        // reads, and the answer can still be read back as it was written.
        let buffer = answer_buffer(view.widget());
        let visible = buffer_text(&buffer);
        assert!(!visible.contains('|'), "{visible:?}");
        assert!(visible.contains("Bring a coat."), "{visible:?}");
        assert!(view.widget().answer_text().contains("| Morning |"));
    }),
    ("a link in a table cell is a link", |_| {
        // A cell is a GtkLabel, not part of the buffer, so the answer's own
        // click handling never reaches it. What makes the cell's link work is
        // the markup, and the label's own machinery under it.
        let view = TurnView::replayed(
            &answered(
                "sources?",
                "| Story | Source |\n|---|---|\n| AI Act | [EuroNews](https://euronews.com/ai) |",
            ),
            true,
        );
        let grid = find::<gtk::Grid>(view.widget().clone().upcast()).expect("the table grid");
        let cell = grid
            .child_at(1, 1)
            .and_downcast::<gtk::Label>()
            .expect("the source cell");

        assert!(cell.uses_markup(), "the cell is plain text");
        assert!(
            cell.label().contains("href=\"https://euronews.com/ai\""),
            "{}",
            cell.label()
        );
        // And the reader sees the words, not the URL.
        assert_eq!(cell.text(), "EuroNews");
    }),
    ("a hidden table leaves no gap where its source was", |_| {
        // The source stays in the buffer, invisible. GTK gives a line that is
        // invisible end to end no height at all — if that ever stopped being
        // true, every table would sit under a stack of blank lines.
        let mut rows = String::from("| a | b |\n|---|---|\n");
        for number in 0..6 {
            rows.push_str(&format!("| {number} | {number} |\n"));
        }
        let view = TurnView::replayed(&answered("?", rows.trim_end()), true);
        let turn: gtk::Widget = view.widget().clone().upcast();

        let answer = find::<gtk::TextView>(turn.clone()).expect("the answer view");
        let grid = find::<gtk::Grid>(turn).expect("the table grid");
        let (_, table_height, _, _) = grid.measure(gtk::Orientation::Vertical, -1);
        let (_, answer_height, _, _) = answer.measure(gtk::Orientation::Vertical, 600);

        // One line's slack: the anchor sits on a line of its own.
        assert!(
            answer_height < table_height + 40,
            "the table is {table_height}px but the answer is {answer_height}px"
        );
    }),
    (
        "the grid is anchored ahead of the source it replaces",
        |_| {
            // GTK reserves an anchored child's height wherever it sits, but it does
            // not *draw* one that trails an otherwise-invisible line — the table
            // came out as a blank band the right size and nothing in it. Anchoring
            // ahead of the hidden source puts the child first on the line, where it
            // draws. Nothing else in the widget tree says which side it is on, so
            // this reads the buffer.
            let view = TurnView::replayed(&answered("?", "| a | b |\n|---|---|\n| 1 | 2 |"), true);
            // A slice rather than the text: `get_text` drops anchors entirely,
            // `get_slice` reports each one as U+FFFC.
            let buffer = answer_buffer(view.widget());
            let text: String = buffer
                .slice(&buffer.start_iter(), &buffer.end_iter(), true)
                .into();
            let anchor = text
                .find('\u{fffc}')
                .expect("the anchor character is in the buffer");
            let pipe = text.find('|').expect("the source is still there");
            assert!(
                anchor < pipe,
                "the anchor is at {anchor}, after the source at {pipe}"
            );
        },
    ),
    ("a table's columns are as wide as what is in them", |_| {
        // A `GtkTextView` allocates an anchored child its *minimum* width, and
        // a wrapping label's minimum is one character — so a table whose cells
        // all wrapped came out as columns of stacked letters and a tall blank
        // band. A cell that fits must not wrap, which makes its minimum the
        // text.
        let view = TurnView::replayed(
            &answered(
                "benchmarks?",
                "| Model | Metric | BaseRT | MLX | llama.cpp |\n\
                 |---|---|---|---|---|\n\
                 | Qwen3 0.6B Q4 | Decode tok/s | 531 | 398 | 386 |\n\
                 | Gemma 4 E2B Q8 | Prefill tok/s | 16,264 | 12,355 | 2,547 |",
            ),
            true,
        );
        let turn: gtk::Widget = view.widget().clone().upcast();
        let grid = find::<gtk::Grid>(turn).expect("the table grid");

        let (minimum, natural, _, _) = grid.measure(gtk::Orientation::Horizontal, -1);
        assert_eq!(
            minimum, natural,
            "the table asks for {minimum}px but wants {natural}px, so the view \
             will squeeze it"
        );

        // Five columns of that content is a wide table, not a narrow one.
        assert!(minimum > 300, "the table is only {minimum}px wide");

        // And it is no taller than its rows: a collapsed column shows up here.
        let (_, height, _, _) = grid.measure(gtk::Orientation::Vertical, minimum);
        assert!(height < 160, "three rows should not be {height}px tall");
    }),
    (
        "a cell of prose wraps rather than run off the window",
        |_| {
            let view = TurnView::replayed(
                &answered(
                    "?",
                    "| Option | What it means |\n\
                 |---|---|\n\
                 | Wrap | A cell of prose still wraps, so one chatty column \
                 cannot push every other one off the side of the window. |",
                ),
                true,
            );
            let turn: gtk::Widget = view.widget().clone().upcast();
            let grid = find::<gtk::Grid>(turn).expect("the table grid");

            let (minimum, _, _, _) = grid.measure(gtk::Orientation::Horizontal, -1);
            assert!(
                minimum < 600,
                "a long cell widened the table to {minimum}px instead of wrapping"
            );
        },
    ),
    ("a table with many columns still fits the answer", |_| {
        // The view gives the grid its minimum and then clips at its own width,
        // so a table that asks for more than the answer's measure loses its
        // last columns off the side with nothing to say it happened. The column
        // budget is shared out, so more columns means a narrower cap.
        let mut wide = String::new();
        for row in 0..3 {
            for column in 0..8 {
                wide.push_str(&format!("| column {column} of some length "));
            }
            wide.push_str("|\n");
            if row == 0 {
                wide.push_str(&"|---".repeat(8));
                wide.push_str("|\n");
            }
        }
        let view = TurnView::replayed(&answered("?", wide.trim_end()), true);
        let turn: gtk::Widget = view.widget().clone().upcast();
        let grid = find::<gtk::Grid>(turn).expect("the table grid");

        let (minimum, _, _, _) = grid.measure(gtk::Orientation::Horizontal, -1);
        // The conversation gives an answer roughly 700px; a table has to live
        // inside that however many columns it has.
        assert!(minimum <= 700, "eight columns came to {minimum}px");
    }),
    ("a table streaming in ends up drawn once", |_| {
        // The answer is re-rendered on every flush while it streams, and each
        // render re-anchors. If a replaced anchor left its widget parented to
        // the view, a table would stack up a copy per flush behind the one on
        // screen.
        let answer = "Here:\n\n| Time | Temp |\n|---|---|\n| 9 AM | 67°F |\n| 10 AM | 70°F |";
        let view = TurnView::replayed(&answered("?", ""), true);
        for upto in 1..=answer.chars().count() {
            let prefix: String = answer.chars().take(upto).collect();
            view.widget().set_answer(&prefix);
        }

        let turn: gtk::Widget = view.widget().clone().upcast();
        assert_eq!(
            count::<gtk::Grid>(turn.clone()),
            1,
            "the flushes left more than one table behind"
        );

        let grid = find::<gtk::Grid>(turn).expect("the table grid");
        let cells = labels(grid.upcast());
        assert!(cells.contains(&"10 AM".to_string()), "{cells:?}");
        assert!(!cells.iter().any(|cell| cell.contains('|')), "{cells:?}");
    }),
    (
        "the thinking disclosure reports how long it thought",
        |_| {
            let mut turn = answered("why?", "because.");
            turn.thinking = "weighing it up".into();
            turn.metrics = Some(TurnMetrics {
                thinking_ms: Some(4_300),
                generated_tokens: 140,
                generation_per_second: Some(84.0),
                ..Default::default()
            });

            let view = TurnView::replayed(&turn, true);
            assert_eq!(view.widget().thinking_summary(), "Thought for 4s");
            assert_eq!(view.widget().thinking_text(), "weighing it up");
            let texts = labels(view.widget().clone().upcast());
            // The numbers are a caption under the turn.
            assert!(
                texts.iter().any(|t| t.contains("84 tok/s")),
                "no metrics line: {texts:?}"
            );
        },
    ),
    (
        "thinking is not drawn at all when the preference is off",
        |_| {
            let mut turn = answered("why?", "because.");
            turn.thinking = "weighing it up".into();
            let view = TurnView::replayed(&turn, false);
            assert!(
                !thinking_shown(view.widget()),
                "the disclosure is on screen with the preference off"
            );
            assert_eq!(view.widget().answer_text(), "because.");
        },
    ),
    ("a turn with no metrics draws no caption", |_| {
        let view = TurnView::replayed(&answered("why?", "because."), true);
        let texts = labels(view.widget().clone().upcast());
        assert!(!texts.iter().any(|t| t.contains("tok/s")), "{texts:?}");
    }),
    (
        "a settled turn shows the settled answer, not the raw one",
        |_| {
            let view = TurnView::new("what do I know?", true);
            for fragment in [
                "Let me look.\n",
                "<tool_call><function=recall>{}</function></tool_call>",
            ] {
                view.apply(&Event::Answer(fragment.to_string()));
            }

            // Mid-stream the throttle has not fired, so nothing is drawn yet; the
            // settle is what puts the finished answer on screen.
            let settled = TurnState {
                answer: "Let me look.".into(),
                finish: Some(Finish::Stop),
                ..Default::default()
            };
            view.settle(&settled);
            assert_eq!(view.widget().answer_text(), "Let me look.");
        },
    ),
    (
        "a failure sits under the answer rather than replacing it",
        |_| {
            let view = TurnView::new("why?", true);
            view.apply(&Event::Answer("half an ans".into()));
            view.settle(&TurnState {
                answer: "half an ans".into(),
                ..Default::default()
            });
            view.set_failure(Some("the connection went away"));

            assert_eq!(view.widget().answer_text(), "half an ans");
            assert!(labels(view.widget().clone().upcast())
                .contains(&"the connection went away".to_string()));
        },
    ),
    // -- the sidebar ----------------------------------------------------------
    ("a project shows only itself until it is opened", |window| {
        window.set_projects(&projects());
        let sidebar = sidebar_of(window);

        // Two projects, nothing under either: a tree that arrives fully
        // expanded is a flat list with extra indentation.
        assert_eq!(
            sidebar.rows(),
            vec![project_row(DEFAULT_PROJECT), project_row("planning")]
        );

        sidebar.open_project(DEFAULT_PROJECT);
        assert_eq!(
            sidebar.rows(),
            vec![
                project_row(DEFAULT_PROJECT),
                Row::NewChat {
                    slug: DEFAULT_PROJECT.into()
                },
                chat_row(DEFAULT_PROJECT, "thread-1"),
                chat_row(DEFAULT_PROJECT, "thread-2"),
                project_row("planning"),
            ]
        );
    }),
    (
        "clicking a chat reports it and clicking New Chat reports its project",
        |window| {
            window.set_projects(&projects());
            let sidebar = sidebar_of(window);

            let chosen = Rc::new(RefCell::new(Vec::<(String, String)>::new()));
            window.connect_closure(
                "thread-chosen",
                false,
                glib::closure_local!(
                    #[strong]
                    chosen,
                    move |_: Window, slug: String, id: String| chosen.borrow_mut().push((slug, id))
                ),
            );
            let started = Rc::new(RefCell::new(Vec::<String>::new()));
            window.connect_closure(
                "new-thread",
                false,
                glib::closure_local!(
                    #[strong]
                    started,
                    move |_: Window, slug: String| started.borrow_mut().push(slug)
                ),
            );

            sidebar.open_project("planning");
            click(
                &sidebar,
                &Row::NewChat {
                    slug: "planning".into(),
                },
            );
            assert_eq!(started.borrow().as_slice(), ["planning"]);

            sidebar.open_project(DEFAULT_PROJECT);
            click(&sidebar, &chat_row(DEFAULT_PROJECT, "thread-2"));
            assert_eq!(
                chosen.borrow().as_slice(),
                [(DEFAULT_PROJECT.to_string(), "thread-2".to_string())]
            );
        },
    ),
    (
        "every project offers a way to start a chat in it",
        |window| {
            // A project with no chats is still somewhere you can go, which is
            // what the New Chat row is for.
            window.set_projects(&projects());
            let sidebar = sidebar_of(window);
            for slug in [DEFAULT_PROJECT, "planning"] {
                sidebar.open_project(slug);
                assert!(
                    sidebar.rows().contains(&Row::NewChat { slug: slug.into() }),
                    "{slug} has no way to start a chat: {:?}",
                    sidebar.rows()
                );
            }
        },
    ),
    (
        "selecting a chat opens the project holding it and does not report a choice",
        |window| {
            window.set_projects(&projects());
            let sidebar = sidebar_of(window);

            let chosen = Rc::new(RefCell::new(Vec::<String>::new()));
            window.connect_closure(
                "thread-chosen",
                false,
                glib::closure_local!(
                    #[strong]
                    chosen,
                    move |_: Window, _slug: String, id: String| chosen.borrow_mut().push(id)
                ),
            );

            // The project is closed, so a chat inside it cannot be highlighted
            // without opening it first.
            window.select_thread(
                DEFAULT_PROJECT,
                Some(&ThreadId::from_stem("thread-2").expect("id")),
            );
            assert_eq!(
                sidebar.selected(),
                Some(chat_row(DEFAULT_PROJECT, "thread-2"))
            );

            // Selecting in code is not the user choosing: it must not reload
            // the conversation underneath them.
            window.select_thread(
                DEFAULT_PROJECT,
                Some(&ThreadId::from_stem("thread-1").expect("id")),
            );
            assert!(chosen.borrow().is_empty(), "{:?}", chosen.borrow());
        },
    ),
    ("a rebuild keeps open what was open", |window| {
        // The application rebuilds this after every turn. A tree that
        // collapsed each time would be unusable.
        window.set_projects(&projects());
        let sidebar = sidebar_of(window);
        sidebar.open_project("planning");

        window.set_projects(&projects());
        assert!(
            sidebar.rows().contains(&Row::NewChat {
                slug: "planning".into()
            }),
            "the open project closed on a rebuild: {:?}",
            sidebar.rows()
        );

        // A project that has gone stops being remembered, rather than the
        // set growing for the life of the application.
        window.set_projects(&projects()[..1]);
        assert_eq!(
            sidebar
                .rows()
                .iter()
                .filter(|row| matches!(row, Row::Project { .. }))
                .count(),
            1
        );
    }),
    ("clicking a project asks for its page", |window| {
        // Not for it to expand: the arrow does that, and the page is where a
        // project's folder, instructions and schedules are.
        window.set_projects(&projects());
        let asked = Rc::new(RefCell::new(Vec::<String>::new()));
        window.connect_closure(
            "project-action",
            false,
            glib::closure_local!(
                #[strong]
                asked,
                move |_: Window, action: String, slug: String| {
                    asked.borrow_mut().push(format!("{action} {slug}"));
                }
            ),
        );

        let sidebar = sidebar_of(window);
        click(&sidebar, &project_row("planning"));
        assert_eq!(asked.borrow().as_slice(), ["open planning"]);
    }),
    ("the sidebar holds no files at all", |window| {
        // They were here for a day and could not be read at this width. The
        // project page has them now.
        let folder = tempfile::tempdir().expect("a folder");
        std::fs::write(folder.path().join("read-me.md"), "hello").expect("a file");
        let mut project = Project::named("Planning");
        project.workspace = Some(folder.path().to_path_buf());
        window.set_projects(&[(project, Vec::new())]);

        let sidebar = sidebar_of(window);
        sidebar.open_project("planning");
        assert_eq!(
            sidebar.rows(),
            vec![
                project_row("planning"),
                Row::NewChat {
                    slug: "planning".into()
                }
            ]
        );
    }),
    (
        "every row menu item acts on the row it was opened on",
        |window| {
            // The items carry the row's key as their target, so activating the
            // action is what the pointer does — including on a row that is not
            // the selected one, which is the case a remembered-row menu gets
            // wrong.
            window.set_projects(&projects());

            let heard = Rc::new(RefCell::new(Vec::<String>::new()));
            for name in ["thread-rename", "thread-delete", "project-action"] {
                window.connect_closure(
                    name,
                    false,
                    glib::closure_local!(
                        #[strong]
                        heard,
                        move |_: Window, first: String, second: String| {
                            heard.borrow_mut().push(format!("{name} {first} {second}"));
                        }
                    ),
                );
            }
            window.connect_closure(
                "new-thread",
                false,
                glib::closure_local!(
                    #[strong]
                    heard,
                    move |_: Window, slug: String| {
                        heard.borrow_mut().push(format!("new-thread {slug}"));
                    }
                ),
            );
            let sidebar = sidebar_of(window);
            sidebar.open_project(DEFAULT_PROJECT);

            let chat = chat_row(DEFAULT_PROJECT, "thread-1");
            for (action, row) in [
                ("new-chat", project_row("planning")),
                ("edit-project", project_row("planning")),
                ("delete-project", project_row("planning")),
                ("rename-chat", chat.clone()),
                ("delete-chat", chat.clone()),
            ] {
                sidebar
                    .activate_action(&format!("row.{action}"), Some(&row.key().to_variant()))
                    .unwrap_or_else(|_| panic!("row.{action} is not an action of the sidebar"));
            }

            assert_eq!(
                heard.borrow().as_slice(),
                [
                    "new-thread planning",
                    "project-action edit planning",
                    "project-action delete planning",
                    "thread-rename default thread-1",
                    "thread-delete default thread-1",
                ]
            );
        },
    ),
    (
        "a project's settings edit its name, instructions and tools",
        |window| {
            let mut planning = Project::named("Planning");
            planning.instructions = Some("You help plan the week.".into());
            let saved = Rc::new(RefCell::new(Vec::<Project>::new()));
            familiar::ui::dialogs::edit_project(
                window,
                &planning,
                glib::clone!(
                    #[strong]
                    saved,
                    move |edited: Project| saved.borrow_mut().push(edited)
                ),
            );

            // Scoped to the dialog: the window behind it has a composer, and
            // the composer's entry is a `GtkTextView` too.
            let dialog = find::<adw::Dialog>(window.clone().upcast()).expect("the dialog");
            let name: adw::EntryRow = row(&dialog, "Name");
            assert_eq!(name.text(), "Planning");
            name.set_text("Planning 2026");

            let text = find::<gtk::TextView>(dialog.clone().upcast()).expect("the instructions");
            let buffer = text.buffer();
            assert_eq!(
                buffer.text(&buffer.start_iter(), &buffer.end_iter(), false),
                "You help plan the week."
            );
            buffer.set_text("Plan in weeks, not days.");

            let files: adw::SwitchRow = row(&dialog, "Files");
            assert!(!files.is_active());
            files.set_active(true);

            button_labelled(&dialog, "Save").emit_clicked();
            let edited = saved.borrow().first().cloned().expect("a saved project");
            assert_eq!(edited.name, "Planning 2026");
            assert_eq!(
                edited.instructions.as_deref(),
                Some("Plan in weeks, not days.")
            );
            assert!(edited.tools.workspace);
            assert_eq!(edited.slug, "planning", "the slug is the identity");
        },
    ),
    (
        "the default project has no name to change and keeps the one it has",
        |window| {
            // Its editor is how somebody customises ordinary behaviour, so
            // instructions are there and a name is not — and the name must
            // survive a save that never showed it.
            let saved = Rc::new(RefCell::new(Vec::<Project>::new()));
            familiar::ui::dialogs::edit_project(
                window,
                &Project::default_project(),
                glib::clone!(
                    #[strong]
                    saved,
                    move |edited: Project| saved.borrow_mut().push(edited)
                ),
            );

            let dialog = find::<adw::Dialog>(window.clone().upcast()).expect("the dialog");
            let titles = labels(dialog.clone().upcast());
            assert!(!titles.contains(&"Name".to_string()), "{titles:?}");
            assert!(titles.contains(&"Instructions".to_string()), "{titles:?}");

            let buffer = find::<gtk::TextView>(dialog.clone().upcast())
                .expect("the instructions")
                .buffer();
            buffer.set_text("Call me Matt.");
            button_labelled(&dialog, "Save").emit_clicked();

            let edited = saved.borrow().first().cloned().expect("a saved project");
            assert!(edited.is_default());
            assert_eq!(edited.name, familiar::model::project::DEFAULT_NAME);
            assert_eq!(edited.instructions.as_deref(), Some("Call me Matt."));
        },
    ),
    // -- the project page -----------------------------------------------------
    ("a project's page shows the files in its folder", |window| {
        let folder = tempfile::tempdir().expect("a folder");
        std::fs::create_dir(folder.path().join("notes")).expect("a subfolder");
        std::fs::write(folder.path().join("read-me.md"), "hello").expect("a file");
        // Hidden entries are left out: a checkout is mostly `.git`.
        std::fs::create_dir(folder.path().join(".git")).expect("a hidden one");

        let mut planning = Project::named("Planning");
        planning.workspace = Some(folder.path().to_path_buf());
        window.show_project_page(&planning, &[], &[]);
        assert!(window.showing_project());

        let page = window.project_view();
        let shown = page.file_rows();
        assert!(
            shown.contains(&FileRow::Folder(folder.path().join("notes"))),
            "{shown:?}"
        );
        assert!(
            shown.contains(&FileRow::File(folder.path().join("read-me.md"))),
            "{shown:?}"
        );
        assert!(
            !shown.iter().any(|row| row.path().ends_with(".git")),
            "{shown:?}"
        );
    }),
    (
        "a project with no folder says so instead of showing an empty box",
        |window| {
            // The bug that moved the files off the sidebar: an empty branch
            // reads as broken, and a folder that has moved reads the same.
            let mut planning = Project::named("Planning");
            window.show_project_page(&planning, &[], &[]);
            let titles = labels(window.project_view().upcast());
            assert!(
                titles.iter().any(|text| text.contains("Choose a folder")),
                "{titles:?}"
            );

            planning.workspace = Some(std::path::PathBuf::from("/nowhere/at/all"));
            window.show_project_page(&planning, &[], &[]);
            let titles = labels(window.project_view().upcast());
            assert!(
                titles
                    .iter()
                    .any(|text| text.contains("not there any more")),
                "{titles:?}"
            );
        },
    ),
    (
        "a project's page lists its chats and searches them",
        |window| {
            let planning = Project::named("Planning");
            let chats: Vec<ThreadSummary> = [
                "roof quotes",
                "roadmap",
                "reading list",
                "rates",
                "rent",
                "rota",
            ]
            .iter()
            .enumerate()
            .map(|(index, title)| ThreadSummary {
                id: ThreadId::from_stem(&format!("thread-{index}")).expect("id"),
                title: (*title).into(),
                updated: Utc::now(),
                turns: 1,
            })
            .collect();
            window.show_project_page(&planning, &chats, &[]);

            let page = window.project_view();
            assert_eq!(page.chat_titles().len(), 6);

            page.search("roa");
            assert_eq!(page.chat_titles(), ["roadmap"]);

            page.search("nothing like this");
            assert!(page.chat_titles().is_empty());

            page.search("");
            assert_eq!(page.chat_titles().len(), 6);
        },
    ),
    ("a project's page says what runs on its own", |window| {
        let planning = Project::named("Planning");
        let scheduled = [familiar::ui::dialogs::Scheduled {
            slug: "planning".into(),
            project: "Planning".into(),
            thread: "thread-1".into(),
            title: "Morning briefing".into(),
            schedule: "Weekdays at 07:00".into(),
            prompt: "what is due?".into(),
            enabled: true,
            status: "Last ran 2 hours ago".into(),
        }];
        window.show_project_page(&planning, &[], &scheduled);

        let titles = labels(window.project_view().upcast());
        assert!(
            titles.contains(&"Runs on Its Own".to_string()),
            "{titles:?}"
        );
        assert!(
            titles.contains(&"Morning briefing".to_string()),
            "{titles:?}"
        );
    }),
    ("the page reports what it was clicked for", |window| {
        let folder = tempfile::tempdir().expect("a folder");
        std::fs::write(folder.path().join("read-me.md"), "hello").expect("a file");
        let mut planning = Project::named("Planning");
        planning.workspace = Some(folder.path().to_path_buf());

        let heard = Rc::new(RefCell::new(Vec::<String>::new()));
        window.connect_closure(
            "page-project-action",
            false,
            glib::closure_local!(
                #[strong]
                heard,
                move |_: Window, action: String| heard.borrow_mut().push(action)
            ),
        );
        window.connect_closure(
            "file-action",
            false,
            glib::closure_local!(
                #[strong]
                heard,
                move |_: Window, action: String, path: String| {
                    let name = std::path::Path::new(&path)
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_default();
                    heard.borrow_mut().push(format!("file {action} {name}"));
                }
            ),
        );

        let chats = [ThreadSummary {
            id: ThreadId::from_stem("thread-1").expect("id"),
            title: "Q3 roadmap".into(),
            updated: Utc::now(),
            turns: 1,
        }];
        let opened = Rc::new(RefCell::new(Vec::<String>::new()));
        window.connect_closure(
            "page-chat-chosen",
            false,
            glib::closure_local!(
                #[strong]
                opened,
                move |_: Window, id: String| opened.borrow_mut().push(id)
            ),
        );

        window.show_project_page(&planning, &chats, &[]);
        let page = window.project_view();
        button_labelled(&page, "Project Settings…").emit_clicked();

        // A chat in the list is a way back into the conversation.
        row::<adw::ActionRow>(&page, "Q3 roadmap").emit_activate();
        assert_eq!(opened.borrow().as_slice(), ["thread-1"]);

        let file = FileRow::File(folder.path().join("read-me.md"));
        for action in ["open", "reveal", "rename", "trash"] {
            page.files()
                .activate_action(&format!("file.{action}"), Some(&file.key().to_variant()))
                .unwrap_or_else(|_| panic!("file.{action} is not an action of the tree"));
        }

        assert_eq!(
            heard.borrow().as_slice(),
            [
                "edit".to_string(),
                "file open read-me.md".to_string(),
                "file reveal read-me.md".to_string(),
                "file rename read-me.md".to_string(),
                "file trash read-me.md".to_string(),
            ]
        );
    }),
    ("opening a chat leaves the project page", |window| {
        window.show_project_page(&Project::named("Planning"), &[], &[]);
        assert!(window.showing_project());
        window.show_chat();
        assert!(!window.showing_project());
    }),
    (
        "a menu item for a row that has gone does nothing",
        |window| {
            window.set_projects(&projects());
            let sidebar = sidebar_of(window);
            let heard = Rc::new(RefCell::new(0usize));
            window.connect_closure(
                "thread-delete",
                false,
                glib::closure_local!(
                    #[strong]
                    heard,
                    move |_: Window, _slug: String, _id: String| *heard.borrow_mut() += 1
                ),
            );
            // The project was never opened, so the chat is not on screen — and
            // the tree is rebuilt after every turn, so a menu left open across
            // one is exactly this case.
            let _ = sidebar.activate_action(
                "row.delete-chat",
                Some(&chat_row(DEFAULT_PROJECT, "thread-1").key().to_variant()),
            );
            assert_eq!(*heard.borrow(), 0);
        },
    ),
    // -- the window -----------------------------------------------------------
    (
        "the window forwards what the composer was asked",
        |window| {
            let sent = Rc::new(RefCell::new(Vec::<String>::new()));
            window.connect_closure(
                "submit",
                false,
                glib::closure_local!(
                    #[strong]
                    sent,
                    move |_: Window, text: String| sent.borrow_mut().push(text)
                ),
            );

            let composer = window.composer();
            set_text(&composer, "what is due?");
            send_button(&composer).emit_clicked();
            assert_eq!(sent.borrow().as_slice(), ["what is due?"]);
        },
    ),
    (
        "trouble is a banner that stays up until it is fixed",
        |window| {
            let banner = banner(window);
            assert!(!banner.is_revealed());

            window.set_trouble(Some("No llama-server at http://127.0.0.1:8080"));
            assert!(banner.is_revealed());
            assert!(banner.title().contains("8080"), "{}", banner.title());

            window.set_trouble(None);
            assert!(!banner.is_revealed());
        },
    ),
    (
        "the context bar appears only once there is something to measure",
        |window| {
            // get_visible, not is_visible: the latter is ancestor-aware and
            // these windows are never presented, so it is false throughout.
            let bar = level_bar(window);
            assert!(!bar.get_visible());

            window.set_context_usage(Some(0.12));
            assert!(bar.get_visible());
            assert!((bar.value() - 0.12).abs() < f64::EPSILON);

            // A fraction past the end is a full bar, not an overflowing one.
            window.set_context_usage(Some(1.5));
            assert!((bar.value() - 1.0).abs() < f64::EPSILON);

            window.set_context_usage(None);
            assert!(!bar.get_visible());
        },
    ),
    (
        "typing an address and an app password makes a Gmail account",
        |_window| {
            // The mail rows are the one preference with no eval behind them and
            // a real chance of being subtly wrong: five widgets writing into one
            // struct, where a half-filled form has to mean "no account" rather
            // than "an account that cannot connect".
            let (state, changed, group) = mail_rows(None);

            let address = row::<adw::EntryRow>(&group, "Email Address");
            let password = row::<adw::PasswordEntryRow>(&group, "App Password");

            assert!(state.borrow().settings.mail.is_none(), "nothing typed yet");

            address.set_text("me@post.example");
            password.set_text("abcdefghijklmnop");

            let account = state.borrow().settings.mail.clone().expect("an account");
            assert_eq!(account.user, "me@post.example");
            assert_eq!(account.password, "abcdefghijklmnop");
            // Filled in from the preset, so nobody has to know them.
            assert_eq!(account.host, "imap.gmail.com");
            assert_eq!(account.port, 993);
            assert_eq!(account.smtp_host, "smtp.gmail.com");
            assert_eq!(account.smtp_port, 465);
            assert!(account.tls);
            // The address is who the mail comes from unless somebody says
            // otherwise, and an empty `from` would send with no sender.
            assert_eq!(account.from, "me@post.example");
            assert!(*changed.borrow() > 0, "no change was reported");
        },
    ),
    (
        "clearing the address removes the account rather than emptying it",
        |_window| {
            // How somebody takes their mail out of this application. A husk
            // left behind would keep Mail switched on with nothing to reach,
            // and every call would fail at the socket instead of saying there
            // is no account.
            let (state, _changed, group) = mail_rows(Some(("someone@gmail.com", "pw")));
            assert!(state.borrow().settings.mail.is_some());

            row::<adw::EntryRow>(&group, "Email Address").set_text("   ");
            assert!(state.borrow().settings.mail.is_none());
        },
    ),
    (
        "a non-Gmail account can be pointed somewhere else",
        |_window| {
            let (state, _changed, group) = mail_rows(Some(("someone@fastmail.com", "pw")));
            let server = row::<adw::ExpanderRow>(&group, "Server");

            // The rows that only a non-Gmail account needs are folded away, and
            // reachable — somebody on Fastmail has to be able to get at them.
            assert!(!server.is_expanded(), "the server rows start folded away");
            row_under::<adw::EntryRow>(&server, "IMAP Server").set_text("imap.fastmail.com");
            row_under::<adw::EntryRow>(&server, "SMTP Server").set_text("smtp.fastmail.com");

            let account = state.borrow().settings.mail.clone().expect("an account");
            assert_eq!(account.host, "imap.fastmail.com");
            assert_eq!(account.smtp_host, "smtp.fastmail.com");
            // Still Gmail's ports, which are the standard ones anyway.
            assert_eq!(account.port, 993);
        },
    ),
];

// -- helpers -----------------------------------------------------------------

fn summaries() -> Vec<ThreadSummary> {
    ["thread-1", "thread-2"]
        .iter()
        .map(|stem| ThreadSummary {
            id: ThreadId::from_stem(stem).expect("id"),
            title: format!("about {stem}"),
            updated: Utc::now(),
            turns: 1,
        })
        .collect()
}

/// The default project with two chats, and a named one with none.
fn projects() -> Vec<(Project, Vec<ThreadSummary>)> {
    vec![
        (Project::default_project(), summaries()),
        (Project::named("Planning"), Vec::new()),
    ]
}

fn sidebar_of(window: &Window) -> Sidebar {
    find::<Sidebar>(window.clone().upcast()).expect("the sidebar")
}

/// What a click on a row does. The list view's `activate` is what
/// single-click-activate emits, so emitting it drives the same handler a
/// pointer would — which is the point of doing it this way rather than calling
/// into the widget's insides.
fn click(sidebar: &Sidebar, row: &Row) {
    let position = sidebar
        .rows()
        .iter()
        .position(|shown| shown == row)
        .unwrap_or_else(|| panic!("no such row: {row:?}, showing {:?}", sidebar.rows()));
    let list = find::<gtk::ListView>(sidebar.clone().upcast()).expect("the list");
    list.emit_by_name::<()>("activate", &[&(position as u32)]);
}

fn project_row(slug: &str) -> Row {
    Row::Project {
        slug: slug.into(),
        default: slug == DEFAULT_PROJECT,
    }
}

fn chat_row(slug: &str, stem: &str) -> Row {
    Row::Chat {
        slug: slug.into(),
        id: ThreadId::from_stem(stem).expect("id"),
    }
}

fn answered(question: &str, answer: &str) -> StoredTurn {
    StoredTurn {
        user: question.into(),
        answer: answer.into(),
        ..Default::default()
    }
}

/// A chip for a call that was made with these arguments and went that way.
fn chip(name: &str, arguments: &str, state: ToolChip) -> Chip {
    let call = familiar::model::turn::ToolCall {
        id: "1".into(),
        name: name.into(),
        arguments: arguments.into(),
        complete: true,
        outcome: None,
    };
    Chip {
        argument: call.primary_argument(),
        call,
        state,
    }
}

/// Whether the disclosure is drawn at all — `get_visible`, since these windows
/// are never presented.
fn thinking_shown(turn: &familiar::ui::Turn) -> bool {
    find::<gtk::Expander>(turn.clone().upcast())
        .expect("the disclosure")
        .get_visible()
}

fn answer_buffer(turn: &familiar::ui::Turn) -> gtk::TextBuffer {
    find::<gtk::TextView>(turn.clone().upcast())
        .expect("the answer view")
        .buffer()
}

fn buffer_text(buffer: &gtk::TextBuffer) -> String {
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string()
}

/// Whether the named tag covers the character at `offset`. This is the real
/// question — a tag that exists but is applied nowhere styles nothing.
fn tag_at(buffer: &gtk::TextBuffer, name: &str, offset: i32) -> bool {
    let Some(tag) = buffer.tag_table().lookup(name) else {
        panic!("{name} was never installed");
    };
    buffer.iter_at_offset(offset).has_tag(&tag)
}

fn set_text(composer: &Composer, text: &str) {
    let view = find::<gtk::TextView>(composer.clone().upcast()).expect("the entry");
    view.buffer().set_text(text);
}

/// The send/stop button specifically. The composer has several buttons — an
/// attach control, and a remove button on every staged thumbnail — so "the
/// first button" is not it.
fn send_button(composer: &Composer) -> gtk::Button {
    button_with_icon(composer, "document-send-symbolic")
        .or_else(|| button_with_icon(composer, "media-playback-stop-symbolic"))
        .expect("the send button")
}

fn button_with_icon(composer: &Composer, icon: &str) -> Option<gtk::Button> {
    let mut found = None;
    walk(&composer.clone().upcast(), &mut |widget| {
        if found.is_some() {
            return;
        }
        if let Ok(button) = widget.clone().downcast::<gtk::Button>() {
            if button.icon_name().unwrap_or_default() == icon {
                found = Some(button);
            }
        }
    });
    found
}

/// The button whose label reads this, somewhere under `root`.
fn button_labelled(root: &impl IsA<gtk::Widget>, label: &str) -> gtk::Button {
    let mut found = None;
    walk(root.as_ref(), &mut |widget| {
        if found.is_some() {
            return;
        }
        if let Ok(button) = widget.clone().downcast::<gtk::Button>() {
            if button.label().unwrap_or_default() == label {
                found = Some(button);
            }
        }
    });
    found.unwrap_or_else(|| panic!("no {label:?} button"))
}

fn banner(window: &Window) -> adw::Banner {
    find::<adw::Banner>(window.clone().upcast()).expect("the banner")
}

fn level_bar(window: &Window) -> gtk::LevelBar {
    find::<gtk::LevelBar>(window.clone().upcast()).expect("the context bar")
}

/// Which page of the widget's internal stack is showing. The stack is how both
/// the sidebar and the conversation hold their empty state.
fn visible_page(widget: &impl IsA<gtk::Widget>) -> Option<String> {
    find::<gtk::Stack>(widget.clone().upcast())
        .and_then(|stack| stack.visible_child_name())
        .map(|name| name.to_string())
}

/// The first widget of a type anywhere under `root`.
fn find<T: IsA<gtk::Widget>>(root: gtk::Widget) -> Option<T> {
    let mut found = None;
    walk(&root, &mut |widget| {
        if found.is_none() {
            if let Ok(widget) = widget.clone().downcast::<T>() {
                found = Some(widget);
            }
        }
    });
    found
}

fn labels(root: gtk::Widget) -> Vec<String> {
    let mut texts = Vec::new();
    walk(&root, &mut |widget| {
        if let Ok(label) = widget.clone().downcast::<gtk::Label>() {
            texts.push(label.text().to_string());
        }
    });
    texts
}

/// Everything held in a `GtkTextView` under `root`. The detail dialog puts the
/// long values — a script, a traceback, a page of results — in these rather
/// than in labels, so `labels` alone cannot see them.
fn text_views(root: gtk::Widget) -> Vec<String> {
    let mut texts = Vec::new();
    walk(&root, &mut |widget| {
        if let Ok(view) = widget.clone().downcast::<gtk::TextView>() {
            let buffer = view.buffer();
            texts.push(
                buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .to_string(),
            );
        }
    });
    texts
}

/// A one-pixel PNG, so staging has a real image to work with.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

/// How many widgets of a type are under `root`.
fn count<T: IsA<gtk::Widget>>(root: gtk::Widget) -> usize {
    let mut found = 0;
    walk(&root, &mut |widget| {
        if widget.clone().downcast::<T>().is_ok() {
            found += 1;
        }
    });
    found
}

/// Every file under a directory, recursively.
fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walkdir(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("svg") {
            found.push(path);
        }
    }
    found
}

/// Visit every widget under `root`.
fn walk(root: &gtk::Widget, visit: &mut impl FnMut(&gtk::Widget)) {
    visit(root);
    let mut child = root.first_child();
    while let Some(widget) = child {
        walk(&widget, visit);
        child = widget.next_sibling();
    }
}

/// The mail preference rows, plus the state they write into and a count of how
/// many times they reported a change.
fn mail_rows(
    account: Option<(&str, &str)>,
) -> (
    Rc<RefCell<familiar::ui::preferences::Preferences>>,
    Rc<RefCell<usize>>,
    adw::PreferencesGroup,
) {
    use familiar::model::settings::{Config, MailAccount, Settings};
    use familiar::ui::preferences::{mail_group, Preferences};

    let current = Preferences {
        config: Config::default(),
        settings: Settings {
            mail: account.map(|(user, password)| MailAccount {
                host: "imap.gmail.com".into(),
                port: 993,
                user: user.into(),
                password: password.into(),
                tls: true,
                from: user.into(),
                smtp_host: "smtp.gmail.com".into(),
                smtp_port: 465,
            }),
            ..Settings::default()
        },
    };
    let state = Rc::new(RefCell::new(current.clone()));
    let counted = Rc::new(RefCell::new(0usize));
    let changed = Rc::new({
        let counted = counted.clone();
        move || *counted.borrow_mut() += 1
    });
    let group = mail_group(&state, &changed, &current);
    (state, counted, group)
}

/// The row with this title, somewhere under `root`.
fn row<T: IsA<gtk::Widget>>(root: &impl IsA<gtk::Widget>, title: &str) -> T {
    let mut found = None;
    walk(root.as_ref(), &mut |widget| {
        if found.is_some() {
            return;
        }
        if let Some(row) = widget.downcast_ref::<adw::PreferencesRow>() {
            if row.title() == title {
                found = widget.clone().downcast::<T>().ok();
            }
        }
    });
    found.unwrap_or_else(|| panic!("no {title:?} row"))
}

/// The same, for a row inside an expander — whose children are not under it in
/// the widget tree until it has been expanded, so it is expanded first.
fn row_under<T: IsA<gtk::Widget>>(expander: &adw::ExpanderRow, title: &str) -> T {
    let was = expander.is_expanded();
    expander.set_expanded(true);
    let found = row::<T>(expander, title);
    expander.set_expanded(was);
    found
}

/// Collect what the composer asked to be sent.
/// Put a finished turn in the conversation and hand back its widget.
fn shown(window: &Window, question: &str, answer: &str) -> familiar::ui::Turn {
    let view = TurnView::replayed(&answered(question, answer), true);
    window.conversation().append(view.widget());
    view.widget().clone()
}

/// Everything the window asks on the user's behalf. An explained selection is
/// a question like any other, so it arrives as `submit`.
fn record_asks(window: &Window) -> Rc<RefCell<Vec<String>>> {
    let asked = Rc::new(RefCell::new(Vec::new()));
    window.connect_closure(
        "submit",
        false,
        glib::closure_local!(
            #[strong]
            asked,
            move |_: Window, text: String| asked.borrow_mut().push(text)
        ),
    );
    asked
}

fn record_submissions(composer: &Composer) -> Rc<RefCell<Vec<String>>> {
    let sent = Rc::new(RefCell::new(Vec::new()));
    composer.connect_closure(
        "submit",
        false,
        glib::closure_local!(
            #[strong]
            sent,
            move |_: Composer, text: String| sent.borrow_mut().push(text)
        ),
    );
    sent
}
