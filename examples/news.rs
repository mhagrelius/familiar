//! Research a subject for real, and print the brief the model would read.
//!
//! The unit tests are folds over recorded bodies, which proves the merging and
//! the ranking but not that four sources still answer the way they did. This is
//! that other half, and it is worth more here than for a single-endpoint tool:
//! the whole point of `news` is that several lanes come back at different
//! speeds and get assembled once, which no unit test observes.
//!
//! Hacker News needs no key, so a run with no Exa key configured still exercises
//! the engagement lane and shows the brief admitting what it could not reach.
//!
//! ```sh
//! cargo run --example news                       # what is drawing attention
//! cargo run --example news -- "Gemma 4"
//! cargo run --example news -- "Gemma 4" 7
//! ```

use familiar::model::settings::Config;
use familiar::model::turn::{ToolCall, ToolOutcome};
use familiar::ui::runner::{exa_key, Runner};
use gtk::glib;

fn main() {
    let mut arguments = std::env::args().skip(1);
    let topic = arguments.next();
    let days: i64 = arguments
        .next()
        .and_then(|days| days.parse().ok())
        .unwrap_or(30);

    let (config, _) = Config::load(&Config::default_path());
    let key = exa_key(config.exa_api_key.as_deref());
    match &topic {
        Some(topic) => println!("Researching {topic:?} over {days} days…\n"),
        None => println!("Sweeping what is drawing attention…\n"),
    }
    if key.is_none() {
        println!("No Exa key configured — expect the tool to say so.\n");
    }

    let runner = Runner::new(std::rc::Rc::new(std::cell::RefCell::new(None)), key);

    let call = ToolCall {
        id: "call_1".into(),
        name: "news".into(),
        arguments: match &topic {
            Some(topic) => serde_json::json!({ "topic": topic, "days": days }).to_string(),
            None => serde_json::json!({ "days": days }).to_string(),
        },
        complete: true,
        outcome: None,
    };

    let main_loop = glib::MainLoop::new(None, false);
    runner.run(&call, {
        let main_loop = main_loop.clone();
        move |outcome| {
            match outcome {
                ToolOutcome::Ok(said) => println!("{said}"),
                other => println!("{other:?}"),
            }
            main_loop.quit();
        }
    });
    main_loop.run();
}
