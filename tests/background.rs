//! A turn that completes with nothing on screen, against the real server.
//!
//! **This is the half the rest of the suite cannot reach.** `session.rs` feeds
//! recorded frames straight into a [`TurnStream`], below the transport, and the
//! unit tests pin the arithmetic. Neither touches the path a scheduled run
//! actually takes: a request over the wire, streamed back, folded into a turn,
//! with no window and no widget anywhere in it.
//!
//! That gap is not theoretical. Two defects lived in exactly it — `save_thread`
//! returned early without a window, so a windowless turn saved nothing and
//! reported no error, and `on_text` returned before pushing to the stream, so a
//! windowless turn stalled mid-answer instead of running. Both were found by
//! reading rather than by testing, which is the argument for this file.
//!
//! **Opt-in, because it needs llama-server.** `./test.sh` must pass on a
//! machine with no GPU and no model, so this runs only when pointed at one:
//!
//! ```sh
//! FAMILIAR_SERVER=http://127.0.0.1:8080 cargo test --test background -- --nocapture
//! ```
//!
//! No display is needed — a `glib::MainLoop` is not a GTK one — so this belongs
//! in the display-free half of the suite.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use familiar::model::instructions::THINK_OFF;
use familiar::model::project::{Store, DEFAULT_PROJECT};
use familiar::model::thread::{StoredTurn, Thread};
use familiar::model::turn::TurnStream;
use familiar::model::wire::{ChatRequest, Message, StreamOptions};
use familiar::ui::client::Client;
use gtk::gio::prelude::CancellableExt;
use gtk::glib;

/// Where the server is, if the runner pointed us at one.
fn server() -> Option<String> {
    match std::env::var("FAMILIAR_SERVER") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            eprintln!(
                "skipping: set FAMILIAR_SERVER=http://127.0.0.1:8080 to run against a real server"
            );
            None
        }
    }
}

/// A short, cheap ask. The content does not matter — that a turn *completes*
/// with nothing to draw on is the whole assertion.
fn one_line(question: &str) -> ChatRequest {
    ChatRequest {
        model: None,
        // `THINK_OFF` and a small `max_tokens` are one decision, not two: a
        // thinking model given sixty-four tokens spends every one of them
        // thinking and answers nothing, which looks exactly like a dead
        // server. Every other one-shot call in the app pairs them the same way.
        messages: vec![Message::system(THINK_OFF), Message::user(question)],
        stream: true,
        stream_options: StreamOptions::default(),
        temperature: None,
        top_p: None,
        // Small on purpose: this is a transport test, not a generation one, and
        // a long answer is a long test.
        max_tokens: Some(64),
        // Honoured only when llama-server was started without
        // `--reasoning-budget`, which is why `THINK_OFF` above is the part that
        // actually does the work. Kept because it costs nothing and helps on a
        // server launched the other way.
        reasoning_budget: Some(0),
        tools: Vec::new(),
    }
}

/// Drive a real request to completion on a main loop, with a wall-clock bound.
///
/// Bounded because the local server is known to die mid-run under sustained
/// load; a test that hangs forever is worse than one that fails.
fn run_turn(client: &Client, request: &ChatRequest) -> (TurnStream, Result<(), String>) {
    let loop_ = glib::MainLoop::new(None, false);
    let stream = Rc::new(RefCell::new(TurnStream::new()));
    let outcome = Rc::new(RefCell::new(None));

    let cancellable = client.stream(
        request,
        {
            let stream = stream.clone();
            move |text: &str| {
                // The push is the point: this is what `on_text` used to skip
                // when there was no window to draw on.
                stream.borrow_mut().push(text);
            }
        },
        {
            let loop_ = loop_.clone();
            let outcome = outcome.clone();
            move |result| {
                *outcome.borrow_mut() = Some(result.map_err(|error| error.to_string()));
                loop_.quit();
            }
        },
    );

    // Ninety seconds is generous for sixty-four tokens and short enough that a
    // wedged server fails the run rather than holding it open.
    let timed_out = Rc::new(std::cell::Cell::new(false));
    glib::timeout_add_local_once(Duration::from_secs(90), {
        let loop_ = loop_.clone();
        let timed_out = timed_out.clone();
        move || {
            timed_out.set(true);
            cancellable.cancel();
            loop_.quit();
        }
    });
    loop_.run();

    assert!(!timed_out.get(), "the server did not answer within 90s");
    let settled = std::mem::take(&mut *stream.borrow_mut());
    let outcome = outcome.borrow_mut().take().expect("an outcome");
    (settled, outcome)
}

#[test]
fn a_turn_completes_with_no_window_and_no_widget() {
    let Some(url) = server() else { return };
    let client = Client::new(&url);
    let (stream, outcome) = run_turn(&client, &one_line("Reply with the single word: ready."));
    outcome.expect("the turn should finish cleanly");

    let state = stream.finish();
    assert!(
        !state.answer.trim().is_empty(),
        "a windowless turn produced no answer at all: {state:?}"
    );
}

#[test]
fn a_due_job_runs_against_its_own_chat_and_leaves_the_open_one_alone() {
    // The whole feature, end to end, against a real server: a job that is due,
    // a chat that is not on screen, and a second chat standing in for whatever
    // the user happens to be looking at. The turn is driven through the same
    // pieces the application uses — the job list decides what is owed, the
    // client streams it, the stream folds into a turn, the store keeps it.
    //
    // What this catches is the class of defect the whole change was about:
    // the answer landing in the wrong conversation.
    use familiar::model::heartbeat::{Recovery, Schedule};
    use familiar::model::jobs::{Destination, Job, Jobs};

    let Some(url) = server() else { return };
    let directory = tempfile::tempdir().expect("a directory");
    let store = Store::new(directory.path());

    // The chat the job writes into, and the one that must be left alone.
    let mut briefing = store.new_thread(DEFAULT_PROJECT).expect("a thread");
    briefing.push_turn(StoredTurn {
        user: "set up a briefing".into(),
        answer: "Done.".into(),
        ..Default::default()
    });
    store.save_thread(DEFAULT_PROJECT, &briefing).expect("save");
    let mut elsewhere = store.new_thread(DEFAULT_PROJECT).expect("a thread");
    elsewhere.push_turn(StoredTurn {
        user: "what the user is actually reading".into(),
        answer: "Unrelated.".into(),
        ..Default::default()
    });
    store
        .save_thread(DEFAULT_PROJECT, &elsewhere)
        .expect("save");

    let mut jobs = Jobs::default();
    let mut job = Job::new(
        "morning",
        Schedule::Daily {
            at: chrono::Local::now().time(),
        },
        "Reply with the single word: briefed.",
        Destination::Chat {
            slug: DEFAULT_PROJECT.into(),
            thread: briefing.id.to_string(),
        },
    );
    job.recovery = Recovery::Whenever;
    // Yesterday, so this occurrence is owed.
    job.last_run = Some(chrono::Utc::now() - chrono::Duration::days(1));
    jobs.add(job, chrono::Utc::now());
    store.save_jobs(&jobs).expect("save the jobs file");

    // What `tick` asks.
    let reloaded = store.load_jobs();
    let (owed, _, _) = reloaded
        .next_due(chrono::Local::now())
        .expect("the job should be owed");
    assert_eq!(
        owed.destination.thread(),
        Some(briefing.id.to_string().as_str())
    );

    // What `run_in_background` then does: a turn against that chat, with no
    // window and no widget.
    let client = Client::new(&url);
    let (stream, outcome) = run_turn(&client, &one_line(&owed.prompt));
    outcome.expect("the scheduled turn should finish cleanly");
    let state = stream.finish();
    assert!(!state.answer.trim().is_empty(), "{state:?}");

    let mut target = store
        .load_thread(DEFAULT_PROJECT, &briefing.id)
        .expect("its chat is still there");
    let before = target.turns().count();
    target.push_turn(StoredTurn::new(&owed.prompt, &state));
    store.save_thread(DEFAULT_PROJECT, &target).expect("save");

    // The job's chat gained the run.
    let after = store
        .load_thread(DEFAULT_PROJECT, &briefing.id)
        .expect("read it back");
    assert_eq!(after.turns().count(), before + 1);
    assert!(!after
        .turns()
        .last()
        .expect("the run")
        .answer
        .trim()
        .is_empty());

    // And the chat that was open gained nothing at all.
    let untouched = store
        .load_thread(DEFAULT_PROJECT, &elsewhere.id)
        .expect("read it back");
    assert_eq!(untouched.turns().count(), 1);
    assert_eq!(
        untouched.turns().last().expect("its one turn").user,
        "what the user is actually reading",
        "a scheduled run must not write into whatever is on screen"
    );
}

#[test]
fn what_a_windowless_turn_produced_reaches_the_disk() {
    // The other half of the pair, and the one `save_thread` used to fail: a
    // turn that runs with nothing on screen still has to be written, because
    // for a scheduled run the file *is* the delivery.
    let Some(url) = server() else { return };
    let client = Client::new(&url);
    let question = "Reply with the single word: stored.";
    let (stream, outcome) = run_turn(&client, &one_line(question));
    outcome.expect("the turn should finish cleanly");
    let state = stream.finish();

    let directory = tempfile::tempdir().expect("a directory");
    let store = Store::new(directory.path());
    let mut thread = store.new_thread(DEFAULT_PROJECT).expect("a thread");
    thread.push_turn(StoredTurn::new(question, &state));
    store
        .save_thread(DEFAULT_PROJECT, &thread)
        .expect("a windowless turn must still be saved");

    let read: Thread = store
        .load_thread(DEFAULT_PROJECT, &thread.id)
        .expect("read it back");
    let turn = read
        .turns()
        .last()
        .expect("the turn survived the round trip");
    assert_eq!(turn.user, question);
    assert!(
        !turn.answer.trim().is_empty(),
        "the answer did not reach the disk"
    );
}
