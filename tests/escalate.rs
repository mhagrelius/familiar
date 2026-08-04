//! Asking a stronger model, against the real CLI.
//!
//! **Opt-in, and it has to be.** Every run of this spends the user's Claude or
//! Codex subscription and sends text to a company's servers, which is not
//! something `./test.sh` should do behind anyone's back. It runs only with
//! `FAMILIAR_ESCALATE=1` in the environment.
//!
//! Which leaves the unit tests asserting an argv nothing ever executed, and
//! that gap is where the actual bug was: both CLIs take *variadic* options, so
//! the question passed as a trailing argument was swallowed as another tool
//! name and the call died with a message about stdin. Every unit test passed.
//! This is the one that catches it.
//!
//! ```sh
//! FAMILIAR_ESCALATE=1 cargo test --test escalate -- --nocapture
//! ```

use std::cell::RefCell;
use std::rc::Rc;

use familiar::model::escalate::{self, Backend};
use familiar::model::turn::{ToolCall, ToolOutcome};
use familiar::ui::runner::{Escalation, Runner};

/// Whether to spend somebody's subscription on a test.
fn allowed() -> bool {
    if std::env::var("FAMILIAR_ESCALATE").as_deref() != Ok("1") {
        eprintln!("skipping: set FAMILIAR_ESCALATE=1 to run a real consultation");
        return false;
    }
    let found = std::process::Command::new("sh")
        .args(["-c", "command -v claude"])
        .status();
    match found {
        Ok(status) if status.success() => true,
        _ => {
            eprintln!("skipping: `claude` is not installed");
            false
        }
    }
}

/// Ask, through the same `Runner` the application uses, and wait.
fn ask(question: &str) -> ToolOutcome {
    let root = tempfile::tempdir().expect("temp dir");
    let context = gtk::glib::MainContext::new();
    context
        .with_thread_default(|| {
            let main_loop = gtk::glib::MainLoop::new(Some(&context), false);
            let outcome = Rc::new(RefCell::new(None));

            let runner =
                Runner::new(Rc::new(RefCell::new(None)), None).with_escalation(Some(Escalation {
                    backend: Backend::Claude,
                    model: None,
                    root: root.path().to_path_buf(),
                }));
            runner.run(
                &ToolCall {
                    id: "1".into(),
                    name: "escalate".into(),
                    arguments: serde_json::json!({ "question": question }).to_string(),
                    complete: true,
                    outcome: None,
                },
                {
                    let outcome = outcome.clone();
                    let main_loop = main_loop.clone();
                    move |result| {
                        outcome.replace(Some(result));
                        main_loop.quit();
                    }
                },
            );
            if outcome.borrow().is_none() {
                main_loop.run();
            }
            let answer = outcome.borrow_mut().take();
            answer.expect("the runner answered")
        })
        .expect("a thread-default main context")
}

#[test]
fn a_real_consultation_comes_back_with_an_answer() {
    if !allowed() {
        return;
    }
    // Deliberately a question with one short, checkable answer, so the test
    // costs as little of somebody's subscription as a test can.
    let outcome = ask(
        "In one short sentence: what is the smallest positive integer expressible as the sum \
         of two positive cubes in two different ways?",
    );
    let ToolOutcome::Ok(said) = &outcome else {
        panic!("the consultation failed: {outcome:?}");
    };
    eprintln!("--- answer ---\n{said}\n---");
    assert!(said.contains("1729"), "{said}");
    // And it arrives with the rule that it is somebody else's answer.
    assert!(said.contains("say plainly that you asked it"), "{said}");
}

#[test]
fn an_empty_question_is_refused_without_spending_anything() {
    // No `allowed()` guard: this one never reaches the network, which is the
    // whole point of checking it.
    let outcome = ask("   ");
    let ToolOutcome::Failed(why) = &outcome else {
        panic!("an empty question should be refused: {outcome:?}");
    };
    assert!(why.contains("no question to ask"), "{why}");
}

#[test]
fn the_question_is_not_visible_on_the_command_line() {
    // A question the user approved in confidence must not be readable by
    // anyone who can run `ps`. It travels on standard input for that reason,
    // and because the CLI's variadic options would otherwise eat it.
    let argv = escalate::command(Backend::Claude, None);
    let secret = "the user's private question";
    assert!(!argv.iter().any(|word| word.contains(secret)));
    assert!(escalate::prompt(secret).contains(secret));
}
