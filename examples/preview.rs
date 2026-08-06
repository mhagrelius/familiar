//! Render the real widget tree to a PNG.
//!
//! Screenshotting a live GNOME Wayland session needs interactive consent, which
//! makes "does this look right?" hard to answer while iterating. This builds the
//! actual widgets against a seeded thread and paints them offscreen instead, so
//! a design change can be looked at in one command.
//!
//! ```sh
//! cargo run --example preview -- /tmp/preview
//! cargo run --example preview -- /tmp/preview dark
//! ```
//!
//! **The window preview shows layout, not answer text.** A `GtkTextView` deep
//! inside the window's toolbar/split-view/overlay chain paints nothing when
//! nothing has ever mapped the window for real — question cards, the thinking
//! disclosure and the metrics line all draw, and the answers come out blank.
//! It is not a bug in the app: `Turn` measures its true height (see
//! `tests/widgets.rs`) and the conversation preview below renders the same
//! answers correctly. Judge long-form styling there, and proportions here.

use std::fs;

use adw::prelude::*;
use chrono::{Duration, Utc};
use gtk::gio;
use gtk::glib;

use familiar::model::project::{Project, ThreadSummary, DEFAULT_PROJECT};
use familiar::model::thread::{StoredToolCall, StoredTurn, Thread, ThreadId};
use familiar::model::turn::{Finish, ToolOutcome, TurnMetrics};
use familiar::ui::{Composer, Conversation, Sidebar, TurnView, Window};

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "/tmp/preview".to_string());
    let dark = args.next().is_some_and(|scheme| scheme == "dark");

    gtk::init().expect("a display — run under xvfb-run if there is none");
    adw::init().expect("libadwaita");

    // An animating widget is a widget that is not finished being laid out.
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_enable_animations(false);
    }

    adw::StyleManager::default().set_color_scheme(if dark {
        adw::ColorScheme::ForceDark
    } else {
        adw::ColorScheme::ForceLight
    });
    if let Some(display) = gtk::gdk::Display::default() {
        familiar::ui::load_stylesheet(&display);
    }

    fs::create_dir_all(&out).expect("output directory");
    let scheme = if dark { "dark" } else { "light" };

    let sidebar = Sidebar::new();
    sidebar.set_projects(&listed());
    sidebar.select(
        DEFAULT_PROJECT,
        Some(&ThreadId::from_stem("thread-1").expect("id")),
    );
    // Open the other project's files too, so the preview shows the tree at its
    // full depth rather than two closed rows.
    sidebar.open_project("planning");
    render(&sidebar, 300, 460, &format!("{out}/sidebar-{scheme}.png"));

    let conversation = Conversation::new();
    for turn in seeded().turns() {
        // The chips come from the stored turn now, drawn by the same `replayed`
        // fold the application uses when a thread is reopened. The preview used
        // to build them itself, which meant it was the only thing on this
        // machine that had ever drawn a replayed chip — and the app drew none.
        conversation.append(TurnView::replayed(turn, true).widget());
    }
    render(
        &conversation,
        760,
        1500,
        &format!("{out}/conversation-{scheme}.png"),
    );

    let composer = Composer::new();
    render(&composer, 760, 90, &format!("{out}/composer-{scheme}.png"));

    // What a chip opens. Rendered from the dialog's own content rather than by
    // presenting it, which needs a compositor — the arrangement is the thing
    // being judged, and it is the same widget tree either way.
    for (name, call) in detailed() {
        let dialog = familiar::ui::tool_detail::dialog_for(&call);
        let content = dialog.child().expect("the dialog's content");
        // Taken off the dialog *before* rendering: `render` puts the widget in
        // a window of its own, and a widget that still has a parent is refused
        // with a `Gtk-CRITICAL` and a blank PNG.
        dialog.set_child(gtk::Widget::NONE);
        render(
            &content,
            560,
            620,
            &format!("{out}/detail-{name}-{scheme}.png"),
        );
    }

    // The preference rows that have no eval behind them. `tests/widgets.rs`
    // drives them and proves they write the right account; this is the half a
    // test cannot answer, which is whether the group reads as one thing or as
    // five fields somebody has to work out the order of.
    {
        use familiar::model::settings::{Config, Settings};
        use familiar::ui::preferences::{mail_group, Preferences};

        let current = Preferences {
            config: Config::default(),
            settings: Settings::default(),
        };
        let state = std::rc::Rc::new(std::cell::RefCell::new(current.clone()));
        let changed = std::rc::Rc::new(|| {});
        let page = adw::PreferencesPage::new();
        page.add(&mail_group(&state, &changed, &current));
        render(&page, 560, 420, &format!("{out}/mail-{scheme}.png"));
    }

    // The voice window, in the two states worth looking at: hearing somebody,
    // and having answered them. It is its own window rather than a widget, so
    // it is presented and snapshotted like the main one.
    {
        use familiar::ui::voice::{State, VoiceWindow};
        for (name, seed) in [("idle", 2), ("listening", 0), ("answered", 1)] {
            let voice = VoiceWindow::new();
            voice.set_default_size(460, 300);
            if seed == 2 {
                voice.set_chat(None);
                voice.set_state(State::Idle);
            } else if seed == 0 {
                voice.set_chat(None);
                voice.set_state(State::Listening);
                voice.set_heard("what did I say about the deploy", false);
                for level in [0.1, 0.4, 0.7, 0.5, 0.3, 0.6, 0.8, 0.4, 0.2, 0.5, 0.9, 0.3] {
                    voice.hear(level);
                }
            } else {
                voice.set_chat(Some("The deploy"));
                voice.set_state(State::Speaking);
                voice.set_heard("what did I say about the deploy", true);
                voice.set_answer(
                    "It went out at four this afternoon and nothing failed. You said you \
                     wanted the migration checked before Friday, which has not happened yet.",
                );
            }
            voice.present();
            settle();
            snapshot(
                &voice,
                460,
                300,
                &format!("{out}/voice-{name}-{scheme}.png"),
            );
            voice.destroy();
        }
    }

    // The whole window, which is the only preview that shows the proportions
    // between the three.
    //
    // A fresh window per attempt: `set_default_size` only takes effect before a
    // window is presented, so growing one that is already on screen silently
    // does nothing — which is exactly how the ladder came to re-snapshot the
    // same 720px window four times and report failure.
    let application = adw::Application::builder()
        .application_id("us.hagreli.Familiar.Preview")
        .build();
    // Registering first: a window added before GApplication::startup has been
    // emitted is a critical, and this example never calls run().
    let _ = application.register(gio::Cancellable::NONE);

    let path = format!("{out}/window-{scheme}.png");
    let mut drawn = false;
    for height in [720, 900, 1200, 1600] {
        let window = Window::new(&application);
        window.set_default_size(1000, height);
        window.set_projects(&listed());
        window.select_thread(
            DEFAULT_PROJECT,
            Some(&ThreadId::from_stem("thread-1").expect("id")),
        );
        window.set_thread_title("How does the scanner work?");
        window.set_project("Chats", true);
        window.set_status("qwen3.6-27b · 1% of context");
        window.set_context_usage(Some(0.012));

        // A short thread on purpose. A WidgetPaintable declines to draw a
        // scroller whose content overflows it, and inside the window the ladder
        // cannot outrun a long answer — the conversation preview above is where
        // long-form styling is judged. This one is about proportions: sidebar
        // against conversation against composer.
        let conversation = window.conversation();
        for turn in seeded_short().turns() {
            let view = TurnView::replayed(turn, true);
            conversation.append(view.widget());
            // Measure it against the width it will get. A GtkTextView reports
            // zero height until something asks it for a height at a known
            // width, and offscreen nothing ever does — so the answers come out
            // missing while every label around them draws correctly.
            view.widget().measure(gtk::Orientation::Vertical, 700);
        }

        window.present();
        settle();
        drawn = snapshot(&window, 1000, height, &path);
        window.destroy();
        if drawn {
            break;
        }
    }
    if !drawn {
        eprintln!("{path}: nothing was drawn, even with room to spare");
    }

    println!("wrote {out}/*-{scheme}.png");
}

/// Two projects, so a plain chat and a project with a folder are both visible.
fn listed() -> Vec<(Project, Vec<ThreadSummary>)> {
    let mut planning = Project::named("Planning");
    planning.instructions = Some("You help plan the week.".into());
    // A real folder, because the Files row is only drawn for one that exists —
    // and the repository this is being run from is as real as it gets.
    planning.workspace = std::env::current_dir().ok();
    vec![
        (Project::default_project(), summaries()),
        (
            planning,
            vec![ThreadSummary {
                id: ThreadId::from_stem("thread-4").expect("id"),
                title: "Q3 roadmap".into(),
                updated: Utc::now() - Duration::days(2),
                turns: 12,
            }],
        ),
    ]
}

fn summaries() -> Vec<ThreadSummary> {
    let now = Utc::now();
    vec![
        ThreadSummary {
            id: ThreadId::from_stem("thread-1").expect("id"),
            title: "How does the scanner work?".into(),
            updated: now,
            turns: 2,
        },
        ThreadSummary {
            id: ThreadId::from_stem("thread-2").expect("id"),
            title: "Ideas for the vault layout".into(),
            updated: now - Duration::days(1),
            turns: 5,
        },
        ThreadSummary {
            id: ThreadId::from_stem("thread-3").expect("id"),
            title: "What did I decide about compaction".into(),
            updated: now - Duration::days(3),
            turns: 9,
        },
    ]
}

/// The calls worth looking at the detail dialog for: the three shapes it has
/// to lay out, which are a handful of short arguments, a script that needs a
/// block of its own, and a failure whose error is the whole point.
fn detailed() -> Vec<(&'static str, familiar::model::turn::ToolCall)> {
    let call = |name: &str, arguments: String, outcome| familiar::model::turn::ToolCall {
        id: "1".into(),
        name: name.into(),
        arguments,
        complete: true,
        outcome: Some(outcome),
    };
    vec![
        (
            "fields",
            call(
                "remember",
                serde_json::json!({
                    "subject": "Roof",
                    "observation": "The north slope was replaced in April 2026 by Vandenberg.",
                    "kind": "fact",
                    "related_to": "Contractors"
                })
                .to_string(),
                ToolOutcome::Ok("Saved to Familiar/Roof.md as a fact.".into()),
            ),
        ),
        (
            "script",
            call(
                "run_python",
                serde_json::json!({
                    "code": "principal = 250_000\nrate = 0.0637 / 12\nmonths = 30 * 12\n\n\
                             payment = principal * (rate * (1 + rate) ** months) / \\\n    \
                             ((1 + rate) ** months - 1)\nprint(f\"monthly payment: {payment:.2f}\")"
                })
                .to_string(),
                ToolOutcome::Ok(
                    "Output:\n```\nmonthly payment: 1558.86\n```\n\nThat output is the answer. \
                     Give it to the user now, in a sentence, with the figure in it."
                        .into(),
                ),
            ),
        ),
        (
            "failed",
            call(
                "read_file",
                serde_json::json!({ "path": "budget.md" }).to_string(),
                ToolOutcome::Failed("budget.md is not there".into()),
            ),
        ),
    ]
}

/// A thread with enough in it to show a turn at both lengths.
fn seeded() -> Thread {
    let mut thread = Thread::new();
    thread.push_turn(StoredTurn {
        at: Some(Utc::now()),
        user: "How does the markdown scanner decide what is syntax?".into(),
        images: Vec::new(),
        thinking: "They want the span model explained. Worth being concrete about the \
                   offsets, since that is the part people get wrong."
            .into(),
        answer: "It reports spans in **character** offsets rather than byte offsets, because \
                 a `GtkTextBuffer` addresses text in characters.\n\n\
                 ## What it costs\n\n\
                 - Each span carries *what it is*\n\
                 - …and which characters are the markers\n\n\
                 That is why a general CommonMark library was not used: it tells you what is \
                 styled, not which characters are `syntax`."
            .into(),
        tool_calls: vec![
            StoredToolCall {
                id: "call_1".into(),
                name: "recall".into(),
                arguments: "markdown scanner".into(),
                outcome: Some(ToolOutcome::Ok("3 notes".into())),
            },
            StoredToolCall {
                id: "call_2".into(),
                name: "web_search".into(),
                arguments: "CommonMark spans".into(),
                outcome: Some(ToolOutcome::Failed("not connected".into())),
            },
        ],
        finish: Some(Finish::Stop),
        metrics: Some(TurnMetrics {
            prompt_tokens: 812,
            generated_tokens: 140,
            generation_per_second: Some(84.0),
            time_to_first_token_ms: Some(320),
            thinking_ms: Some(4_300),
            draft_acceptance: Some(0.86),
            ..Default::default()
        }),
    });
    thread.push_turn(StoredTurn {
        at: Some(Utc::now()),
        user: "Is that why lists nest by indent width?".into(),
        answer: "Yes. The depth comes from the indent widths a note actually uses, so a \
                 two-space file and a four-space file both nest one step at a time."
            .into(),
        finish: Some(Finish::Stop),
        ..Default::default()
    });
    thread.push_turn(long_form());
    thread
}

/// The shape an answer to a research question actually takes: a run of chips,
/// headings, prose, a table and a code block. Density is what this one is for —
/// every other fixture is short enough that the spacing between blocks never
/// adds up to anything, which is how a conversation that scrolls for a page and
/// a half was signed off from previews that fit on one screen.
fn long_form() -> StoredTurn {
    let searched = |query: &str| StoredToolCall {
        id: format!("call_{query}"),
        name: "web_search".into(),
        arguments: query.into(),
        outcome: Some(ToolOutcome::Ok("6 results".into())),
    };
    StoredTurn {
        at: Some(Utc::now()),
        user: "I have two laptops with 128 GB of RAM each. Can I serve a 4-bit quantisation \
               across both of them, and what do people doing this settle on?"
            .into(),
        images: Vec::new(),
        thinking: "Two questions: whether it fits at all, and what the stack looks like. The \
                   memory arithmetic decides the first one and the answer is no."
            .into(),
        answer: "This is a timely question — the ecosystem converged fast over the last few \
                 months. Here is what the consensus looks like.\n\n\
                 ## The hard constraint is memory\n\n\
                 The 4-bit weights are **378 GB**. Two machines at 128 GB gives you 256 GB, so \
                 the model does not fit: you need *at least three*, and four with headroom for \
                 the KV cache and the OS.\n\n\
                 ## The stack that has emerged\n\n\
                 | Layer | Tool | Role |\n\
                 | --- | --- | --- |\n\
                 | Orchestration | Exo | Discovers nodes, shards the model, speaks the \
                 OpenAI API |\n\
                 | Interconnect | RDMA or ring over TCP | Tensor parallelism against pipeline \
                 parallelism |\n\
                 | Inference | MLX | Per-node engine, ahead on long context |\n\n\
                 Both vendors document this natively now.\n\n\
                 ## Setting it up\n\n\
                 ```sh\n\
                 rdma_ctl enable\n\
                 exo --discovery manual --node-id one\n\
                 ```\n\n\
                 Then the parts worth knowing before you spend a weekend on it:\n\n\
                 - Thunderbolt gives you the bandwidth but not the latency\n\
                 - A ring is easier to bring up than a mesh and slower at every token\n\
                 - Quantisation below 4-bit costs more than the machine you save\n\n\
                 If the third machine is not an option, the honest answer is a smaller model."
            .into(),
        tool_calls: vec![
            searched("RDMA over Thunderbolt for model serving"),
            searched("serving large models across two laptops"),
            searched("4-bit quantisation memory footprint"),
            searched("cluster interconnect best practices"),
            searched("distributed inference tokens per second"),
        ],
        finish: Some(Finish::Stop),
        metrics: Some(TurnMetrics {
            prompt_tokens: 4_180,
            generated_tokens: 610,
            generation_per_second: Some(71.0),
            time_to_first_token_ms: Some(410),
            thinking_ms: Some(5_100),
            draft_acceptance: Some(0.79),
            ..Default::default()
        }),
    }
}

/// Two short turns: enough to show a question, an answer and the numbers, and
/// short enough that the window's scroller does not overflow.
fn seeded_short() -> Thread {
    let mut thread = Thread::new();
    thread.push_turn(StoredTurn {
        at: Some(Utc::now()),
        user: "Which offsets does the scanner report?".into(),
        thinking: "They want the short version.".into(),
        answer: "**Character** offsets, because that is what `GtkTextBuffer` takes.".into(),
        finish: Some(Finish::Stop),
        metrics: Some(TurnMetrics {
            prompt_tokens: 812,
            generated_tokens: 140,
            generation_per_second: Some(84.0),
            time_to_first_token_ms: Some(320),
            thinking_ms: Some(4_300),
            draft_acceptance: Some(0.86),
            ..Default::default()
        }),
        ..Default::default()
    });
    thread.push_turn(StoredTurn {
        at: Some(Utc::now()),
        user: "Is that why lists nest by indent width?".into(),
        answer: "Yes — depth comes from the widths a note actually uses.".into(),
        finish: Some(Finish::Stop),
        ..Default::default()
    });
    thread
}

fn render(widget: &impl IsA<gtk::Widget>, width: i32, height: i32, path: &str) {
    for factor in [1, 2, 3, 4] {
        if try_render(widget, width, height * factor, path) {
            return;
        }
    }
    eprintln!("{path}: nothing was drawn, even with room to spare");
}

fn try_render(widget: &impl IsA<gtk::Widget>, width: i32, height: i32, path: &str) -> bool {
    let window = gtk::Window::builder()
        .default_width(width)
        .default_height(height)
        .child(widget)
        .build();
    // No titlebar: these are pictures of a widget, and a window decoration
    // around one reads as a mistake.
    window.set_titlebar(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));
    window.present();

    settle();
    let drawn = snapshot(
        &window,
        window.width().max(width),
        window.height().max(height),
        path,
    );

    // Take the widget back before the window goes, so a caller can render the
    // same one twice.
    window.set_child(gtk::Widget::NONE);
    window.destroy();
    drawn
}

/// Run the main loop until there is nothing left to lay out.
fn settle() {
    let context = glib::MainContext::default();
    for _ in 0..100 {
        let mut worked = false;
        while context.iteration(false) {
            worked = true;
        }
        if !worked {
            break;
        }
    }
}

/// Paint a realised window into a PNG. Reports whether anything was drawn.
fn snapshot(window: &impl IsA<gtk::Widget>, width: i32, height: i32, path: &str) -> bool {
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, width as f64, height as f64);

    let Some(node) = snapshot.to_node() else {
        return false;
    };
    let renderer = gtk::gsk::CairoRenderer::new();
    renderer
        .realize(gtk::gdk::Surface::NONE)
        .expect("a renderer");
    let texture = renderer.render_texture(&node, None);
    texture.save_to_png(path).expect("write the png");
    renderer.unrealize();
    true
}
