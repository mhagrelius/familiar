//! Drive the two memory calls that never happen in a turn.
//!
//! `--suite memory` grades them against fixed transcripts and invented corpora,
//! which is what makes it a regression test. This is the other half, and the
//! same seam `examples/tools.rs` is for the agentic loop: the real server, a
//! real vault, and your own words. A prompt problem and a vault problem are
//! otherwise indistinguishable.
//!
//! ```sh
//! # what the reader would take out of one exchange
//! cargo run --release --example memory -- read ~/Notes \
//!     "From now on put my files under work/" "Understood."
//!
//! # what tonight would do, without doing it
//! cargo run --release --example memory -- dream ~/Notes
//! cargo run --release --example memory -- dream ~/Notes --apply
//! ```
//!
//! **`dream` is read-only unless you pass `--apply`.** It removes lines from
//! notes, and a flag is cheap insurance against finding out what it does by
//! having it done.

use std::cell::RefCell;
use std::rc::Rc;

use familiar::model::memory::dream::{self, Policy};
use familiar::model::memory::{harvest, Memory};
use familiar::model::turn::TurnStream;
use familiar::ui::client::Client;
use familiar::ui::embedder::Embeddings;
use gtk::glib;

const USAGE: &str = "\
usage: cargo run --release --example memory -- <command> <vault> [options]

  read <vault> \"<what they said>\" [\"<what you answered>\"]
        Run the passive reader over one exchange and print what it would save.

  dream <vault> [--apply]
        Run a night's consolidation. Prints what it would do; --apply does it.

  index <vault>
        Bring the vault's vectors level with it, through the embedding server.

  recall <vault> \"<what to look for>\"
        Search the vault the way the tool does, semantically where it can, and
        say which half of the search found each hit.

  --server URL   llama-server (default http://127.0.0.1:8080)
  --lexical      skip the embedding server, to see what recall does without it";

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let server = value(&arguments, "--server").unwrap_or_else(|| "http://127.0.0.1:8080".into());
    let apply = arguments.iter().any(|argument| argument == "--apply");
    let positional: Vec<&String> = arguments
        .iter()
        .filter(|argument| !argument.starts_with("--"))
        .collect();

    let (Some(command), Some(vault)) = (positional.first(), positional.get(1)) else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let root = std::path::PathBuf::from(vault);
    if !root.is_dir() {
        eprintln!("{} is not a directory", root.display());
        std::process::exit(2);
    }

    let main_loop = glib::MainLoop::new(None, false);
    let client = Rc::new(Client::new(&server));
    // The ledger stays where the application keeps it: this is a real vault and
    // a lookup here is a real lookup.
    let memory = Rc::new(RefCell::new(Memory::open(&root)));
    eprintln!(
        "{} note(s), {} observation(s), vectors {}",
        memory.borrow().index().len(),
        memory.borrow().observations().len(),
        if memory.borrow().semantic().is_some() {
            "loaded"
        } else {
            "absent — recall will match words only"
        }
    );

    match command.as_str() {
        "read" => {
            let said = positional.get(2).map(|text| text.as_str()).unwrap_or("");
            let answered = positional.get(3).map(|text| text.as_str()).unwrap_or("");
            read(&client, &memory, said, answered, main_loop.clone());
        }
        "dream" => night(&client, &memory, apply, main_loop.clone()),
        "index" => index(&memory, main_loop.clone()),
        "recall" => {
            let query = positional.get(2).map(|text| text.as_str()).unwrap_or("");
            let lexical = arguments.iter().any(|argument| argument == "--lexical");
            look(&memory, query, lexical, main_loop.clone());
        }
        other => {
            eprintln!("no command called {other}\n\n{USAGE}");
            std::process::exit(2);
        }
    }
    main_loop.run();
}

/// Embed whatever the vault has that the store does not.
///
/// The application does this at launch and after every change to a note, in the
/// background, where a failure is silent by design. Here it is the whole job and
/// says what happened — which is the difference between "semantic search is not
/// working" and "the server is on a different host".
fn index(memory: &Rc<RefCell<Memory>>, main_loop: glib::MainLoop) {
    let url = familiar::model::memory::brain_embedding_url();
    let (passages, store_path) = {
        let borrowed = memory.borrow();
        (
            borrowed.passages().to_vec(),
            borrowed.store_path().to_path_buf(),
        )
    };
    println!("embedding {} note(s) through {url}", passages.len());
    println!("into {}", store_path.display());

    let embeddings = Embeddings::start(&url);
    embeddings.catch_up(passages, store_path, {
        let memory = memory.clone();
        move |store| {
            match store {
                Some(store) => {
                    println!("the store now holds {} note(s)", store.len());
                    memory.borrow_mut().set_semantic(store);
                }
                // The one failure worth spelling out: everything else here
                // degrades to lexical search silently and on purpose.
                None => println!(
                    "nothing came back — the server is unreachable, or it was started \
                     without --embeddings"
                ),
            }
            main_loop.quit();
        }
    });
    std::mem::forget(embeddings);
}

/// Search the vault, through the same two halves the tool goes through.
///
/// The one path nothing else here covers. The unit tests give `recall` no
/// vector, the eval suite stubs it out entirely, and the application only ever
/// reaches it through a tool call — so whether the embedding server is
/// reachable, whether its model matches the cache, and whether a query with none
/// of a note's words finds it are all questions only this answers.
fn look(memory: &Rc<RefCell<Memory>>, query: &str, lexical: bool, main_loop: glib::MainLoop) {
    if query.trim().is_empty() {
        eprintln!("nothing to look for\n\n{USAGE}");
        std::process::exit(2);
    }
    let show = {
        let memory = memory.clone();
        let query = query.to_string();
        move |vector: Option<Vec<f32>>| {
            match (&vector, memory.borrow().semantic().is_some()) {
                (Some(v), true) => println!("embedded the query as {} dimensions", v.len()),
                (Some(_), false) => println!("embedded the query, but the vault has no vectors"),
                (None, _) => println!("no vector — searching words only"),
            }
            let found =
                memory
                    .borrow_mut()
                    .recall(&query, 5, vector.as_deref(), chrono::Utc::now());
            println!();
            if found.is_empty() {
                println!("nothing");
            }
            for hit in found {
                let how = match (hit.lexical, hit.semantic) {
                    (true, true) => "words and meaning",
                    (true, false) => "words",
                    (false, true) => "meaning alone — related, not an exact match",
                    (false, false) => "?",
                };
                println!("{} ({how})\n    {}", hit.title, hit.excerpt);
                for observation in &hit.observations {
                    println!("    · {observation}");
                }
            }
            main_loop.quit();
        }
    };

    if lexical {
        // Deferred rather than called: `show` ends by quitting the main loop,
        // and `main` has not started it yet. Quitting one that has never run
        // does nothing, and `run()` then blocks for ever.
        glib::idle_add_local_once(move || show(None));
        return;
    }
    let url = familiar::model::memory::brain_embedding_url();
    println!("embedding through {url}");
    let embeddings = Embeddings::start(&url);
    embeddings.query(query, show);
    // The handle has to outlive the call. Dropping it closes the channel the
    // worker is waiting on, the thread ends, and the answer never arrives.
    std::mem::forget(embeddings);
}

fn value(arguments: &[String], name: &str) -> Option<String> {
    let at = arguments.iter().position(|argument| argument == name)?;
    arguments.get(at + 1).cloned()
}

fn read(
    client: &Rc<Client>,
    memory: &Rc<RefCell<Memory>>,
    said: &str,
    answered: &str,
    main_loop: glib::MainLoop,
) {
    if !harvest::worth_reading(said) {
        println!("The gate turned this away — nothing in it looks like a durable fact.");
        println!("Nothing was sent, which is what happens after most turns.");
        main_loop.quit();
        return;
    }

    let known: Vec<String> = memory
        .borrow()
        .ranked(chrono::Utc::now())
        .0
        .iter()
        .map(|ranked| ranked.observation.line())
        .collect();
    let request = harvest::request(said, answered, &known);

    let stream = Rc::new(RefCell::new(TurnStream::new()));
    let cancellable = client.stream(
        &request,
        {
            let stream = stream.clone();
            move |text: &str| {
                stream.borrow_mut().push(text);
            }
        },
        {
            let known = known.clone();
            move |outcome| {
                if let Err(error) = outcome {
                    eprintln!("the reader failed: {error}");
                    main_loop.quit();
                    return;
                }
                let mut borrowed = stream.borrow_mut();
                borrowed.end();
                let state = std::mem::take(&mut *borrowed).finish();
                drop(borrowed);

                println!("\n--- what came back ---\n{}", state.answer.trim());
                let kept = harvest::vet(harvest::parse(&state.answer), &known);
                println!("\n--- what would be saved ---");
                if kept.is_empty() {
                    println!("nothing");
                }
                for candidate in kept {
                    println!(
                        "[{}] {}: {}",
                        candidate.kind.label(),
                        candidate.subject,
                        candidate.observation
                    );
                }
                main_loop.quit();
            }
        },
    );
    std::mem::forget(cancellable);
}

fn night(
    client: &Rc<Client>,
    memory: &Rc<RefCell<Memory>>,
    apply: bool,
    main_loop: glib::MainLoop,
) {
    let now = chrono::Utc::now();
    let held = memory.borrow().held(&Default::default());
    if held.is_empty() {
        println!("Nothing has been saved into this vault yet.");
        main_loop.quit();
        return;
    }

    let settled = dream::arithmetic(&held, now, &Policy::default());
    println!("--- what needs no model ---");
    describe(&settled);
    if apply {
        let done = memory.borrow_mut().dream(&settled, now);
        println!("applied: {}", done.describe().unwrap_or("nothing".into()));
        journal(done, now);
    }

    // Whatever is left, one batch at a time, exactly as the application does it.
    let held = memory.borrow().held(&Default::default());
    let batches: Vec<Vec<dream::Held>> = held
        .chunks(dream::BATCH)
        .map(<[dream::Held]>::to_vec)
        .collect();
    batch(client, memory, batches, 0, apply, now, main_loop);
}

fn batch(
    client: &Rc<Client>,
    memory: &Rc<RefCell<Memory>>,
    batches: Vec<Vec<dream::Held>>,
    at: usize,
    apply: bool,
    now: chrono::DateTime<chrono::Utc>,
    main_loop: glib::MainLoop,
) {
    let Some(this) = batches.get(at).cloned() else {
        if !apply {
            println!("\nNothing was written. Pass --apply to carry it out.");
        }
        main_loop.quit();
        return;
    };
    println!("\n--- batch {} of {} ---", at + 1, batches.len());

    let stream = Rc::new(RefCell::new(TurnStream::new()));
    let client = client.clone();
    let memory = memory.clone();
    let cancellable = client.clone().stream(
        &dream::request(&this, now),
        {
            let stream = stream.clone();
            move |text: &str| {
                stream.borrow_mut().push(text);
            }
        },
        move |outcome| {
            if let Err(error) = outcome {
                eprintln!("the batch failed: {error}");
                main_loop.quit();
                return;
            }
            let mut borrowed = stream.borrow_mut();
            borrowed.end();
            let state = std::mem::take(&mut *borrowed).finish();
            drop(borrowed);

            let plan = dream::parse(&state.answer, &this).bounded(&this, &Policy::default(), now);
            describe(&plan);
            if apply {
                let done = memory.borrow_mut().dream(&plan, now);
                println!("applied: {}", done.describe().unwrap_or("nothing".into()));
                journal(done, now);
            }
            batch(&client, &memory, batches, at + 1, apply, now, main_loop);
        },
    );
    std::mem::forget(cancellable);
}

/// Write what went where the application writes it.
///
/// Not decoration: "it forgot something I wanted" has to have an answer that is
/// not "it is gone", and that has to be true of a night run by hand as much as
/// one run at three in the morning.
fn journal(applied: dream::Applied, now: chrono::DateTime<chrono::Utc>) {
    if applied.is_quiet() {
        return;
    }
    let path = dream::Journal::default_path();
    let mut journal = dream::Journal::load(&path);
    journal.record(applied, now);
    match journal.save(&path) {
        Ok(()) => println!("         (recorded in {})", path.display()),
        Err(error) => eprintln!("could not write {}: {error}", path.display()),
    }
}

fn describe(plan: &dream::Plan) {
    if plan.is_empty() {
        println!("nothing");
        return;
    }
    for operation in &plan.operations {
        match operation {
            dream::Operation::Drop {
                subject, text, why, ..
            } => println!("drop     [{}] {subject}: {text}", why.label()),
            dream::Operation::Merge {
                subject,
                texts,
                into,
                ..
            } => {
                println!("merge    {subject}: {} lines → {into}", texts.len());
                for text in texts {
                    println!("           was: {text}");
                }
            }
            dream::Operation::Reclassify {
                subject,
                text,
                from,
                to,
                ..
            } => println!(
                "refile   {subject}: {} → {} — {text}",
                from.label(),
                to.label()
            ),
        }
    }
}
