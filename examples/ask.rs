//! Ask the real server one question, through the real client, with no window.
//!
//! The test suite drives `TurnStream` with recorded frames, which proves the
//! fold and proves nothing about the wire. This runs the actual path — libsoup,
//! the SSE decoder, the UTF-8 tail, the fold — against a live `llama-server`
//! and prints what came back, so a transport problem can be told apart from a
//! UI problem without opening the app.
//!
//! ```sh
//! cargo run --example ask -- "why is the sky blue?"
//! cargo run --example ask -- "hello" http://127.0.0.1:8080
//! ```

use std::cell::RefCell;
use std::rc::Rc;

use familiar::model::instructions::{date_line, Prompt, DEFAULT_PERSONA};
use familiar::model::turn::{Event, TurnStream};
use familiar::model::wire::{ChatRequest, Message};
use familiar::ui::client::Client;
use gtk::glib;

fn main() {
    let mut arguments = std::env::args().skip(1);
    let question = arguments
        .next()
        .unwrap_or_else(|| "In one sentence: what is a familiar?".to_string());
    let url = arguments
        .next()
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());

    let main_loop = glib::MainLoop::new(None, false);
    let client = Client::new(&url);

    client.probe({
        let url = url.clone();
        move |result| match result {
            Ok(info) => println!(
                "server: {} · context {}\n",
                info.model.as_deref().unwrap_or("unnamed"),
                info.context_window
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "unknown".into())
            ),
            Err(error) => println!("server at {url} did not answer /props: {error}\n"),
        }
    });

    let volatile = date_line(chrono::Local::now());
    let request = ChatRequest {
        messages: vec![
            Message::system(
                Prompt {
                    persona: DEFAULT_PERSONA,
                    instructions: None,
                    capabilities: &[],
                    ambient: None,
                    volatile: &volatile,
                }
                .compose(),
            ),
            Message::user(question.clone()),
        ],
        ..ChatRequest::new(Vec::new())
    };

    println!("> {question}\n");

    let stream = Rc::new(RefCell::new(TurnStream::new()));
    let thinking_shown = Rc::new(RefCell::new(false));

    let _cancellable = client.stream(
        &request,
        {
            let stream = stream.clone();
            let thinking_shown = thinking_shown.clone();
            move |text: &str| {
                let events = stream.borrow_mut().push(text);
                for event in events {
                    match event {
                        // Thinking is printed once, as a marker: this example
                        // exists to prove the two streams arrive separately.
                        Event::Thinking(_) => {
                            if !*thinking_shown.borrow() {
                                *thinking_shown.borrow_mut() = true;
                                print!("[thinking…] ");
                                flush();
                            }
                        }
                        Event::Answer(fragment) => {
                            print!("{fragment}");
                            flush();
                        }
                        Event::ToolCall(index) => print!("[tool {index}]"),
                        Event::Failed(error) => eprintln!("\n  frame failed: {error}"),
                        Event::Measured | Event::Finished(_) => {}
                    }
                }
            }
        },
        {
            let stream = stream.clone();
            let main_loop = main_loop.clone();
            move |outcome| {
                if let Err(error) = &outcome {
                    eprintln!("\n\ntransport: {error}");
                }
                let mut borrowed = stream.borrow_mut();
                borrowed.end();
                let state = std::mem::take(&mut *borrowed).finish();

                println!("\n\n--- {} ---", state.metrics().one_line());
                println!(
                    "thinking {} chars, answer {} chars, finish {:?}",
                    state.thinking.chars().count(),
                    state.answer.chars().count(),
                    state.finish
                );
                main_loop.quit();
            }
        },
    );

    main_loop.run();
}

fn flush() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}
