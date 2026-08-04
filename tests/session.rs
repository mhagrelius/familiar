//! A whole thread, end to end, with no server and no display.
//!
//! The unit tests pin each fold on its own. This one drives the path the app
//! actually takes — recorded frames off the wire, folded into a turn, stored in
//! a thread, written to a project, reopened from disk, and handed back to the
//! model — because the interesting bugs live in the joins between those, not
//! inside any one of them.
//!
//! The transport is not here yet. What stands in for it is exactly what it will
//! deliver: the bytes llama-server sends, split at the awkward places.

use std::time::Duration;

use familiar::model::project::{Store, DEFAULT_PROJECT};
use familiar::model::thread::{StoredToolCall, StoredTurn};
use familiar::model::turn::{Finish, ManualClock, ToolOutcome, TurnStream};
use familiar::model::wire::Role;

/// One SSE event, as it arrives.
fn frame(json: &str) -> String {
    format!("data: {json}\n\n")
}

/// A stream that thinks, answers, and reports its numbers — the ordinary case.
fn recorded_answer() -> Vec<String> {
    vec![
        frame(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#),
        frame(r#"{"choices":[{"delta":{"reasoning_content":"They want the "}}]}"#),
        frame(r#"{"choices":[{"delta":{"reasoning_content":"scanner explained."}}]}"#),
        frame(r#"{"choices":[{"delta":{"content":"The scanner reports "}}]}"#),
        frame(r#"{"choices":[{"delta":{"content":"syntax spans."}}]}"#),
        frame(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
        frame(
            r#"{"choices":[],"usage":{"prompt_tokens":812,"completion_tokens":140,"total_tokens":952},"timings":{"prompt_n":812,"prompt_ms":406.0,"predicted_n":140,"predicted_ms":1750.0}}"#,
        ),
        "data: [DONE]\n\n".to_string(),
    ]
}

fn stream_with_clock() -> (TurnStream, std::rc::Rc<ManualClock>) {
    struct Shared(std::rc::Rc<ManualClock>);
    impl familiar::model::turn::Clock for Shared {
        fn now(&self) -> Duration {
            self.0.now()
        }
    }
    let clock = std::rc::Rc::new(ManualClock::default());
    (
        TurnStream::with_clock(Box::new(Shared(clock.clone()))),
        clock,
    )
}

#[test]
fn a_turn_survives_the_wire_the_disk_and_being_reopened() {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = Store::new(directory.path());

    let (mut stream, clock) = stream_with_clock();
    // 400 ms of network and prefill, then a frame every 100 ms. The first of
    // those announces the role and carries no text.
    clock.advance(Duration::from_millis(400));
    for chunk in recorded_answer() {
        stream.push(&chunk);
        clock.advance(Duration::from_millis(100));
    }
    let state = stream.finish();

    let mut thread = store.new_thread(DEFAULT_PROJECT).expect("new thread");
    thread.push_turn(StoredTurn::new("How does the scanner work?", &state));
    store.save_thread(DEFAULT_PROJECT, &thread).expect("save");

    let reopened = store
        .load_thread(DEFAULT_PROJECT, &thread.id)
        .expect("load");
    let turn = reopened.turns().next().expect("a turn");

    assert_eq!(turn.user, "How does the scanner work?");
    assert_eq!(turn.answer, "The scanner reports syntax spans.");
    assert_eq!(turn.thinking, "They want the scanner explained.");
    assert_eq!(turn.finish, Some(Finish::Stop));

    // The numbers are llama.cpp's, not the wall clock's.
    let metrics = turn.metrics.expect("metrics");
    assert_eq!(metrics.prompt_tokens, 812);
    assert_eq!(metrics.generated_tokens, 140);
    assert_eq!(metrics.generation_per_second, Some(80.0));
    // An empty role delta is not a first token. The thinking that arrived
    // 100 ms after it is.
    assert_eq!(metrics.time_to_first_token_ms, Some(500));
    assert!(!metrics.one_line().is_empty());

    // And the thinking, which is on disk and on screen, is not in what goes
    // back to the model.
    let messages = reopened.messages_for_model();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(messages[1].text_of(), "The scanner reports syntax spans.");
    let sent = serde_json::to_string(&messages).expect("serialize");
    assert!(!sent.contains("They want"), "{sent}");
}

#[test]
fn a_reply_that_arrives_in_arbitrary_pieces_folds_the_same_way() {
    // A socket read lands wherever it lands. Reassembled a byte at a time, the
    // turn has to come out identical to one read whole.
    let whole: String = recorded_answer().concat();

    let (mut byte_at_a_time, _clock) = stream_with_clock();
    let mut buffer = [0u8; 4];
    for character in whole.chars() {
        byte_at_a_time.push(character.encode_utf8(&mut buffer));
    }

    let (mut all_at_once, _clock) = stream_with_clock();
    all_at_once.push(&whole);

    let split = byte_at_a_time.finish();
    let single = all_at_once.finish();
    assert_eq!(split.answer, single.answer);
    assert_eq!(split.thinking, single.thinking);
    assert_eq!(split.finish, single.finish);
    assert_eq!(split.usage, single.usage);
}

#[test]
fn a_tool_call_and_its_result_come_back_as_a_request_and_an_answer() {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = Store::new(directory.path());

    let (mut stream, _clock) = stream_with_clock();
    stream.push(&frame(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"recall","arguments":"{\"query\":\"scan"}}]}}]}"#,
    ));
    stream.push(&frame(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ner\"}"}}]}}]}"#,
    ));
    stream.push(&frame(
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    ));
    let state = stream.finish();

    assert_eq!(state.finish, Some(Finish::ToolCalls));
    assert_eq!(
        state.tool_calls[0].primary_argument().as_deref(),
        Some("scanner")
    );

    // The application runs the tool and records what it returned.
    let mut turn = StoredTurn::new("what do I know about scanners?", &state);
    turn.tool_calls[0].outcome = Some(ToolOutcome::Ok("3 notes mention it".into()));
    turn.answer = "Three notes mention it.".into();

    let mut thread = store.new_thread(DEFAULT_PROJECT).expect("new thread");
    thread.push_turn(turn);
    store.save_thread(DEFAULT_PROJECT, &thread).expect("save");

    let messages = store
        .load_thread(DEFAULT_PROJECT, &thread.id)
        .expect("load")
        .messages_for_model();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1].tool_calls[0].function.name, "recall");
    assert_eq!(messages[2].role, Role::Tool);
    assert_eq!(messages[2].text_of(), "3 notes mention it");
}

#[test]
fn cancelling_halfway_keeps_what_was_said_and_marks_it_cancelled() {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = Store::new(directory.path());

    let (mut stream, _clock) = stream_with_clock();
    stream.push(&frame(
        r#"{"choices":[{"delta":{"content":"The scanner rep"}}]}"#,
    ));
    stream.cancel();
    let state = stream.finish();

    let mut thread = store.new_thread(DEFAULT_PROJECT).expect("new thread");
    thread.push_turn(StoredTurn::new("How does the scanner work?", &state));
    store.save_thread(DEFAULT_PROJECT, &thread).expect("save");

    let reopened = store
        .load_thread(DEFAULT_PROJECT, &thread.id)
        .expect("load");
    let turn = reopened.turns().next().expect("a turn");
    assert_eq!(turn.answer, "The scanner rep");
    assert_eq!(turn.finish, Some(Finish::Cancelled));
    // A half-answer is still history: the next turn sees what was said.
    assert_eq!(reopened.messages_for_model().len(), 2);
}

#[test]
fn a_server_that_says_no_leaves_the_thread_untouched() {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = Store::new(directory.path());

    let (mut stream, _clock) = stream_with_clock();
    let events = stream.push(&frame(
        r#"{"error":{"message":"the request exceeds the available context size","type":"server_error"}}"#,
    ));
    assert!(matches!(
        events.as_slice(),
        [familiar::model::turn::Event::Failed(_)]
    ));

    let state = stream.finish();
    assert!(state.is_empty());

    // Nothing was said, so nothing is written: an empty thread never reaches
    // the disk.
    let mut thread = store.new_thread(DEFAULT_PROJECT).expect("new thread");
    if !state.is_empty() {
        thread.push_turn(StoredTurn::new("hello", &state));
    }
    store.save_thread(DEFAULT_PROJECT, &thread).expect("save");
    assert!(store.threads(DEFAULT_PROJECT).expect("threads").is_empty());
}

#[test]
fn a_project_keeps_its_own_chats() {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = Store::new(directory.path());
    let planning = store.create_project("Planning").expect("create");

    for (slug, question) in [
        (DEFAULT_PROJECT, "what is the weather"),
        (&planning.slug, "what is due"),
    ] {
        let (mut stream, _clock) = stream_with_clock();
        stream.push(&frame(
            r#"{"choices":[{"delta":{"content":"…"},"finish_reason":"stop"}]}"#,
        ));
        let mut thread = store.new_thread(slug).expect("new thread");
        thread.push_turn(StoredTurn::new(question, &stream.finish()));
        store.save_thread(slug, &thread).expect("save");
    }

    let lobby = store.threads(DEFAULT_PROJECT).expect("threads");
    let focused = store.threads(&planning.slug).expect("threads");
    assert_eq!(lobby.len(), 1);
    assert_eq!(focused.len(), 1);
    assert_eq!(lobby[0].title, "what is the weather");
    assert_eq!(focused[0].title, "what is due");

    // Deleting the project takes its chats and leaves the plain ones alone.
    store.delete_project(&planning.slug).expect("delete");
    assert_eq!(store.threads(DEFAULT_PROJECT).expect("threads").len(), 1);
    assert_eq!(store.projects().expect("projects").len(), 1);
}

#[test]
fn a_stored_tool_call_that_was_never_run_is_not_replayed() {
    let mut thread = familiar::model::thread::Thread::new();
    thread.push_turn(StoredTurn {
        user: "tidy the folder".into(),
        tool_calls: vec![StoredToolCall {
            id: "call_1".into(),
            name: "delete".into(),
            arguments: "{}".into(),
            outcome: None,
        }],
        ..Default::default()
    });
    // The turn was interrupted at the approval dialog. Sending the request with
    // no result would leave the model waiting on an answer that never comes.
    assert_eq!(thread.messages_for_model().len(), 1);
}

#[test]
fn a_turn_that_calls_a_tool_is_still_one_turn_on_disk() {
    // The shape of the agentic loop: the model asks for a tool, the result goes
    // back, it answers, and what lands in the thread is a single turn carrying
    // both the call and the answer.
    let directory = tempfile::tempdir().expect("temp dir");
    let store = Store::new(directory.path());

    // Round one: the model asks for a tool and says nothing else.
    let (mut first, _clock) = stream_with_clock();
    first.push(&frame(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"recall","arguments":"{\"query\":\"scanner\"}"}}]}}]}"#,
    ));
    first.push(&frame(
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    ));
    let asked = first.finish();
    assert_eq!(asked.finish, Some(Finish::ToolCalls));

    // Round two: given the result, it answers.
    let (mut second, _clock) = stream_with_clock();
    second.push(&frame(
        r#"{"choices":[{"delta":{"content":"Three notes mention it."},"finish_reason":"stop"}]}"#,
    ));
    let answered = second.finish();

    // The application folds the rounds together into one turn.
    let mut calls = asked.tool_calls.clone();
    calls[0].outcome = Some(ToolOutcome::Ok("3 notes".into()));
    let settled = familiar::model::turn::TurnState {
        answer: answered.answer.clone(),
        tool_calls: calls,
        ..asked
    };

    let mut thread = store.new_thread(DEFAULT_PROJECT).expect("new thread");
    thread.push_turn(StoredTurn::new("what do I know?", &settled));
    store.save_thread(DEFAULT_PROJECT, &thread).expect("save");

    let reopened = store
        .load_thread(DEFAULT_PROJECT, &thread.id)
        .expect("load");
    assert_eq!(reopened.turns().count(), 1);

    // And the model's view of it has the request and the result in the right
    // order, with the answer after them.
    let messages = reopened.messages_for_model();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1].tool_calls[0].function.name, "recall");
    assert_eq!(messages[2].role, Role::Tool);
    assert_eq!(messages[3].text_of(), "Three notes mention it.");
}

#[test]
fn a_long_thread_is_compacted_without_losing_the_transcript() {
    use familiar::model::compaction::{self, Fold, Headings};

    let directory = tempfile::tempdir().expect("temp dir");
    let store = Store::new(directory.path());
    let mut thread = store.new_thread(DEFAULT_PROJECT).expect("new thread");

    for turn in 1..=10 {
        let (mut stream, _clock) = stream_with_clock();
        stream.push(&frame(&format!(
            r#"{{"choices":[{{"delta":{{"content":"a{turn}"}},"finish_reason":"stop"}}]}}"#
        )));
        thread.push_turn(StoredTurn::new(format!("q{turn}"), &stream.finish()));
    }
    store.save_thread(DEFAULT_PROJECT, &thread).expect("save");

    let history = thread.messages_for_model();
    let before = history.len();

    let mut fold: Option<Fold> = None;
    while let Some((chunk, more)) = compaction::to_summarize(&history, fold.as_ref(), 3) {
        fold = Some(compaction::extend(fold.as_ref(), &chunk, more, &Headings));
    }
    thread.fold = fold;
    store
        .save_thread(DEFAULT_PROJECT, &thread)
        .expect("save folded");

    let view = compaction::view(&history, thread.fold.as_ref());
    assert!(thread.fold.is_some(), "nothing was folded");
    assert!(view.len() < before, "the model's view did not narrow");

    // The transcript on disk is untouched: only the model's view narrowed. The
    // fold rides with the thread, so reopening it does not have to summarise
    // the conversation again to be able to answer in it.
    let reopened = store
        .load_thread(DEFAULT_PROJECT, &thread.id)
        .expect("load");
    assert_eq!(reopened.turns().count(), 10);
    assert_eq!(reopened.messages_for_model().len(), before);
    assert_eq!(reopened.fold, thread.fold);
    assert_eq!(
        compaction::view(&reopened.messages_for_model(), reopened.fold.as_ref()).len(),
        view.len()
    );
}
