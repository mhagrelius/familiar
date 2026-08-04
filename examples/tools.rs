//! Drive one turn with tools against the real server, with no window.
//!
//! `ask` proves the transport. This proves the agentic loop: the model is
//! offered the vault tools, its calls are run for real against a vault, the
//! results go back, and it answers. If a turn works here it works in the app,
//! because it is the same fold, the same runner and the same request builder.
//!
//! ```sh
//! cargo run --example tools -- /path/to/vault "what do you know about Familiar?"
//! ```

use std::cell::RefCell;
use std::rc::Rc;

use familiar::model::instructions::{date_line, Prompt, DEFAULT_PERSONA};
use familiar::model::memory::Memory;
use familiar::model::project::ToolSet;
use familiar::model::tools;
use familiar::model::turn::{ToolOutcome, TurnStream};
use familiar::model::wire::{ChatRequest, Content, Message, Role, ToolInvocation};
use familiar::ui::client::Client;
use familiar::ui::runner::Runner;
use gtk::glib;

fn main() {
    let mut arguments = std::env::args().skip(1);
    let vault = arguments.next().expect("a vault path");
    let question = arguments
        .next()
        .unwrap_or_else(|| "What do you know about Familiar? Use your notes.".to_string());

    let memory = Rc::new(RefCell::new(Some(Memory::open(std::path::Path::new(
        &vault,
    )))));
    let runner = Rc::new(Runner::new(
        memory.clone(),
        familiar::ui::runner::exa_key(None),
    ));
    let client = Rc::new(Client::new("http://127.0.0.1:8080"));

    let set = ToolSet {
        memory: true,
        web: true,
        weather: false,
        workspace: false,
        github: false,
        documents: false,
        planner: false,
        magpie: false,
        python: false,
        escalate: false,
        mail: false,
        scheduling: false,
        workflow: false,
    };
    let offered = tools::for_tools(&set, true);
    let capabilities = tools::guidance(&set, true);
    let ambient = memory
        .borrow()
        .as_ref()
        .and_then(|m| m.ambient(chrono::Utc::now()));
    let volatile = date_line(chrono::Local::now());
    let system = Prompt {
        persona: DEFAULT_PERSONA,
        instructions: None,
        capabilities: &capabilities,
        ambient: ambient.as_deref(),
        volatile: &volatile,
    }
    .compose();

    println!("> {question}\n");

    let main_loop = glib::MainLoop::new(None, false);
    let history = Rc::new(RefCell::new(vec![
        Message::system(system),
        Message::user(question),
    ]));
    let round = Rc::new(RefCell::new(0usize));

    ask(&client, &runner, &history, &round, &offered, &main_loop);
    main_loop.run();
}

/// Run each call in turn, then hand them all back.
fn run_tools<F>(
    mut pending: Vec<familiar::model::turn::ToolCall>,
    mut ran: Vec<familiar::model::turn::ToolCall>,
    runner: Rc<Runner>,
    state: familiar::model::turn::TurnState,
    done: F,
) where
    F: FnOnce(Vec<familiar::model::turn::ToolCall>, familiar::model::turn::TurnState) + 'static,
{
    if pending.is_empty() {
        done(ran, state);
        return;
    }
    let mut call = pending.remove(0);
    let runner_again = runner.clone();
    runner.run(&call.clone(), move |outcome| {
        call.outcome = Some(outcome);
        ran.push(call);
        run_tools(pending, ran, runner_again, state, done);
    });
}

fn ask(
    client: &Rc<Client>,
    runner: &Rc<Runner>,
    history: &Rc<RefCell<Vec<Message>>>,
    round: &Rc<RefCell<usize>>,
    offered: &[tools::Tool],
    main_loop: &glib::MainLoop,
) {
    let request = ChatRequest {
        messages: history.borrow().clone(),
        tools: offered.iter().map(|tool| tool.declaration()).collect(),
        ..ChatRequest::new(Vec::new())
    };

    let stream = Rc::new(RefCell::new(TurnStream::new()));
    let _cancellable = client.stream(
        &request,
        {
            let stream = stream.clone();
            move |text: &str| {
                for event in stream.borrow_mut().push(text) {
                    if let familiar::model::turn::Event::Answer(fragment) = event {
                        print!("{fragment}");
                        use std::io::Write;
                        let _ = std::io::stdout().flush();
                    }
                }
            }
        },
        {
            let stream = stream.clone();
            let client = client.clone();
            let runner = runner.clone();
            let history = history.clone();
            let round = round.clone();
            let offered = offered.to_vec();
            let main_loop = main_loop.clone();
            move |outcome| {
                if let Err(error) = &outcome {
                    eprintln!("\ntransport: {error}");
                    main_loop.quit();
                    return;
                }
                let mut borrowed = stream.borrow_mut();
                borrowed.end();
                let state = std::mem::take(&mut *borrowed).finish();
                drop(borrowed);

                if state.tool_calls.is_empty() || *round.borrow() >= 4 {
                    println!("\n\n--- {} ---", state.metrics().one_line());
                    main_loop.quit();
                    return;
                }

                *round.borrow_mut() += 1;
                println!();

                // Sequentially, the way the application does it: each call's
                // answer starts the next, because a web search does not answer
                // before `run` returns.
                run_tools(
                    state.tool_calls.clone(),
                    Vec::new(),
                    runner.clone(),
                    state.clone(),
                    {
                        let client = client.clone();
                        let runner = runner.clone();
                        let history = history.clone();
                        let round = round.clone();
                        let offered = offered.clone();
                        let main_loop = main_loop.clone();
                        move |ran, state| {
                            let mut invocations = Vec::new();
                            let mut results = Vec::new();
                            for call in &ran {
                                let text = match call.outcome.as_ref() {
                                    Some(ToolOutcome::Ok(result)) => result.clone(),
                                    Some(ToolOutcome::Failed(error)) => format!("Error: {error}"),
                                    Some(ToolOutcome::Denied) => "declined".to_string(),
                                    None => String::new(),
                                };
                                println!(
                                    "  [{}({})] → {}",
                                    call.name,
                                    call.primary_argument().unwrap_or_default(),
                                    text.lines().next().unwrap_or("")
                                );
                                invocations.push(ToolInvocation::new(
                                    call.id.clone(),
                                    call.name.clone(),
                                    call.arguments.clone(),
                                ));
                                results.push(Message::tool_result(call.id.clone(), text));
                            }
                            println!();

                            {
                                let mut history = history.borrow_mut();
                                history.push(Message {
                                    role: Role::Assistant,
                                    content: (!state.answer.is_empty())
                                        .then(|| Content::Text(state.answer.clone())),
                                    reasoning_content: (!state.thinking.is_empty())
                                        .then(|| state.thinking.clone()),
                                    tool_calls: invocations,
                                    tool_call_id: None,
                                });
                                history.extend(results);
                            }
                            ask(&client, &runner, &history, &round, &offered, &main_loop);
                        }
                    },
                );
            }
        },
    );

    // The stream outlives this call through the callbacks; the cancellable is
    // only needed to stop it early, which this example never does.
    std::mem::forget(_cancellable);
}
