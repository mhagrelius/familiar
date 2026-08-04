//! A realistic multi-turn conversation, run twice: with prior reasoning sent
//! back, and without.
//!
//! The question this answers is whether dropping the model's own thinking from
//! history costs anything on a long, reasoning-heavy thread — and what carrying
//! it costs in context. Both runs use the same script, the same settings and a
//! cold prompt cache, so the only variable is `carry_reasoning`.
//!
//! ```sh
//! cargo run --example conversation          # carrying reasoning
//! cargo run --example conversation -- off   # not carrying it
//! ```

use std::cell::RefCell;
use std::rc::Rc;

use familiar::model::instructions::{date_line, Prompt, DEFAULT_PERSONA};
use familiar::model::thread::{StoredTurn, Thread};
use familiar::model::turn::TurnStream;
use familiar::model::wire::{ChatRequest, Message};
use familiar::ui::client::Client;
use gtk::glib;

/// A thread that builds on itself: each turn depends on a decision made in an
/// earlier one, and the last two ask the model to recall *why* — which is the
/// part that lives in its reasoning rather than its answers.
const SCRIPT: &[&str] = &[
    "I'm designing the storage layer for a GNOME notes app in Rust. Notes are Markdown \
     files in a folder the user picks. Should I add a SQLite index for search, or scan \
     the folder at startup? Think it through.",
    "Say the vault has 5,000 notes averaging 4 KB. Does your answer change?",
    "What breaks first as that grows — the scan, the search, or the memory?",
    "Now add a second app writing to the same folder. What has to change?",
    "Which of the trade-offs you weighed earlier turned out to matter most here?",
    "Summarise the decision and the single strongest argument against it.",
];

fn main() {
    // `on` or `off`: carrying reasoning is all or nothing, because a window
    // rewrites the middle of the prompt and throws the cached prefix away.
    let carry = std::env::args().nth(1).map_or(true, |a| a != "off");

    let client = Rc::new(Client::new("http://127.0.0.1:8080"));
    let thread = Rc::new(RefCell::new(Thread::new()));
    let main_loop = glib::MainLoop::new(None, false);

    println!("carry_reasoning = {carry}\n");
    turn(0, carry, client, thread, main_loop.clone());
    main_loop.run();
}

fn turn(
    at: usize,
    carry: bool,
    client: Rc<Client>,
    thread: Rc<RefCell<Thread>>,
    main_loop: glib::MainLoop,
) {
    let Some(question) = SCRIPT.get(at) else {
        let thread = thread.borrow();
        let turns: Vec<&StoredTurn> = thread.turns().collect();
        let thinking: usize = turns.iter().map(|t| t.thinking.chars().count()).sum();
        let answers: usize = turns.iter().map(|t| t.answer.chars().count()).sum();
        println!("\n=== reasoning carried: {carry} ===");
        println!("  thinking written : {thinking} chars");
        println!("  answers written  : {answers} chars");
        if let Some(last) = turns.last().and_then(|t| t.metrics) {
            println!("  final prompt     : {} tokens", last.prompt_tokens);
        }
        println!(
            "\n--- last answer ---\n{}",
            turns.last().map(|t| t.answer.as_str()).unwrap_or("")
        );
        main_loop.quit();
        return;
    };

    let volatile = date_line(chrono::Local::now());
    let system = Prompt {
        persona: DEFAULT_PERSONA,
        instructions: None,
        capabilities: &[],
        ambient: None,
        volatile: &volatile,
    }
    .compose();

    let mut messages = vec![Message::system(system)];
    messages.extend(thread.borrow().messages_with_reasoning(carry));
    messages.push(Message::user(*question));

    let request = ChatRequest {
        messages,
        temperature: Some(0.6),
        top_p: Some(0.95),
        ..ChatRequest::new(Vec::new())
    };

    let stream = Rc::new(RefCell::new(TurnStream::new()));
    let next = client.clone();
    let _keep = client.stream(
        &request,
        {
            let stream = stream.clone();
            move |text: &str| {
                stream.borrow_mut().push(text);
            }
        },
        {
            let stream = stream.clone();
            move |outcome| {
                if let Err(error) = &outcome {
                    eprintln!("transport: {error}");
                    main_loop.quit();
                    return;
                }
                let mut borrowed = stream.borrow_mut();
                borrowed.end();
                let state = std::mem::take(&mut *borrowed).finish();
                drop(borrowed);

                let metrics = state.metrics();
                println!(
                    "turn {}: prompt {:>6} · thinking {:>5} chars · answer {:>5} chars · \
                     {:>5.0} t/s prefill · {:>5.1} t/s gen",
                    at + 1,
                    metrics.prompt_tokens,
                    state.thinking.chars().count(),
                    state.answer.chars().count(),
                    metrics.prompt_per_second.unwrap_or(0.0),
                    metrics.generation_per_second.unwrap_or(0.0),
                );

                thread
                    .borrow_mut()
                    .push_turn(StoredTurn::new(SCRIPT[at], &state));
                turn(at + 1, carry, next, thread, main_loop);
            }
        },
    );
    std::mem::forget(_keep);
}
