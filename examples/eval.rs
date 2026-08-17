//! Run the prompt eval suite against a real llama-server.
//!
//! The suite, the stubs and the scoring live in `familiar::model::eval`, which
//! is pure and tested. This is the part that needs a server: it composes the
//! prompt the application would compose, drives the same agentic loop the
//! application drives, and hands every tool call an invented answer rather than
//! running anything. Nothing here reaches a vault, a workspace, Exa, GitHub or
//! the weather service — a whole pass is one process talking to one local
//! model.
//!
//! ```sh
//! # a baseline, three samples per scenario
//! cargo run --release --example eval -- --repeats 3 --out baseline.json
//!
//! # change a paragraph of the prompt, then see what it bought
//! cargo run --release --example eval -- --persona variant.txt \
//!     --repeats 3 --baseline baseline.json --out variant.json
//!
//! # while iterating: one family, one sample, every call printed
//! cargo run --release --example eval -- --filter documents/ --repeats 1 --verbose
//! ```

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Instant;

use familiar::model::compaction::{self, Headings};
use familiar::model::eval::report::{Report, ScenarioResult};
use familiar::model::eval::scenario::Scenario;
use familiar::model::eval::stub::Reply;
use familiar::model::eval::trace::{Call, Trace};
use familiar::model::eval::{self, suite};
use familiar::model::instructions::DEFAULT_PERSONA;
use familiar::model::project::ToolSet;
use familiar::model::settings::Settings;
use familiar::model::turn::{Finish, TurnStream, LAST_ROUND, WRAP_UP, WRAP_UP_AFTER};
use familiar::model::web::Budget;
use familiar::model::wire::{ChatRequest, Content, Message, Role, ToolDeclaration, ToolInvocation};
use familiar::model::workflow::{Overlap, Workflow};
use familiar::ui::client::Client;
use gtk::glib;

/// How many rounds of tools one step may take before the harness stops it.
///
/// Below the application's 16. A scenario here is one question with invented
/// answers, and anything that needs nine rounds of them is thrashing — which is
/// what the harness wants to record, not wait out.
const DEFAULT_ROUNDS: usize = 8;

/// The most tokens one response may generate.
///
/// The application sets no ceiling, and for a person watching an answer appear
/// that is right. Here it is not: an unbounded generation that falls into a
/// repetition loop keeps going until it has filled the whole 175k context, which
/// costs twenty minutes of wall clock and blocks every scenario behind it. One
/// stall of exactly that shape ate a measurement pass. Hitting it is meant to be
/// the finding itself, and the trace records the truncation as a step that
/// produced nothing rather than as a result.
///
/// **It was 4096, and that was a measurement of Qwen3.6's appetite rather than a
/// ceiling.** 3.6 never generated more than 3229 tokens in any run of the whole
/// suite; Qwen3.8 at `medium` reasoning effort crossed 4096 fifteen times in the
/// same 408 runs and peaked at 8426. Every one of those truncations scored as an
/// empty answer, which cost the `escalate` family nine of its thirty-five runs
/// and read as a model regression until the token counts were looked at. A cap
/// tuned to one model's thinking budget silently grades the next one down.
/// Raising it cannot move a 3.6 number, because 3.6 never reached the old value.
const MAX_GENERATED: u32 = 16384;

/// How many times a run the server never completed is tried again.
const DEFAULT_RETRIES: usize = 2;

/// The tools that *spend* the budget, matching `ui::application`.
fn is_a_search(tool: &str) -> bool {
    matches!(tool, "web_search" | "news")
}

/// The tools the budget *refuses* once it is spent, matching `ui::application`.
/// Wider than the above: a `fetch_url` after the searches are gone is the same
/// hunt by another route, and letting it through is how a turn reaches eight
/// lookups — unless the user named the site, which the application exempts and
/// so must this, or the harness scores a rule the product does not have.
fn is_a_lookup(call: &familiar::model::turn::ToolCall, said: &[String]) -> bool {
    if is_a_search(&call.name) {
        return true;
    }
    call.name == "fetch_url" && !familiar::model::web::fetches_a_named_page(&call.arguments, said)
}

/// How long to wait before retrying, in seconds. Long enough for systemd to
/// restart `llama-server` and for it to reload 27B of weights.
const RESTART_GRACE: u32 = 45;

fn main() {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(complaint) => {
            eprintln!("{complaint}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    // Before anything composes a prompt: the arm decides what the `gh`
    // declaration and the capability catalogue say, and both are built per
    // scenario from here on.
    options.overlap.install();

    if options.suite == "memory" || options.suite == "lookout" {
        let cases: Vec<Box<dyn eval::Graded>> = if options.suite == "lookout" {
            eval::lookout::all()
                .into_iter()
                .map(|case| Box::new(case) as Box<dyn eval::Graded>)
                .collect()
        } else {
            eval::memory::all()
                .into_iter()
                .map(|case| Box::new(case) as Box<dyn eval::Graded>)
                .collect()
        };
        run_single(&options, cases, baseline_of(&options));
        return;
    }

    let chosen = match options.suite.as_str() {
        "recall" => eval::recall::all(),
        _ => suite::all(),
    };
    let scenarios: Vec<Scenario> = chosen
        .into_iter()
        .filter(|scenario| match &options.filter {
            Some(needle) => scenario.name.contains(needle.as_str()),
            None => true,
        })
        .collect();

    if options.list {
        for scenario in &scenarios {
            println!("{:<44} {}", scenario.name, scenario.about);
        }
        return;
    }
    if scenarios.is_empty() {
        eprintln!("no scenario matches that filter");
        std::process::exit(2);
    }

    let persona = match &options.persona {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => text.trim().to_string(),
            Err(error) => {
                eprintln!("could not read {path}: {error}");
                std::process::exit(2);
            }
        },
        None => DEFAULT_PERSONA.to_string(),
    };

    let baseline = baseline_of(&options);

    let main_loop = glib::MainLoop::new(None, false);
    let harness = Rc::new(Harness::new(
        &options,
        scenarios,
        persona,
        main_loop.clone(),
    ));

    eprintln!(
        "{} scenario(s) × {} run(s) = {} against {}",
        harness.scenarios.len(),
        options.repeats,
        harness.total,
        options.server
    );

    // The model's name is worth having in the report, and asking for it also
    // fails fast on a server that is not there.
    let started = harness.clone();
    let concurrency = options.jobs;
    harness.client.probe(move |info| {
        match info {
            Ok(info) => *started.model.borrow_mut() = info.model,
            Err(error) => {
                eprintln!("the server did not answer /props: {error}");
                started.main_loop.quit();
                return;
            }
        }
        for _ in 0..concurrency {
            start_next(&started);
        }
    });

    main_loop.run();

    let report = harness.report(&options);
    print!("{}", eval::report::render(&report));

    if let Some(path) = &options.out {
        match serde_json::to_string_pretty(&report) {
            Ok(json) => match std::fs::write(path, json) {
                Ok(()) => eprintln!("\nwrote {path}"),
                Err(error) => eprintln!("\ncould not write {path}: {error}"),
            },
            Err(error) => eprintln!("\ncould not serialise the report: {error}"),
        }
    }

    if let Some(path) = &options.html {
        match std::fs::write(path, eval::report::render_html(&report)) {
            Ok(()) => eprintln!("wrote {path} — open it in a browser"),
            Err(error) => eprintln!("could not write {path}: {error}"),
        }
    }

    if let Some(baseline) = &baseline {
        println!();
        print!(
            "{}",
            eval::report::render_comparison(&report.against(baseline))
        );
    }
}

// -- options -----------------------------------------------------------------

const USAGE: &str = "\
usage: cargo run --release --example eval -- [options]

  --server URL       llama-server (default http://127.0.0.1:8080)
  --suite NAME       prompt (default), recall, memory or lookout
  --compaction MODE  off (default) or headings — fold between turns
  --keep-recent N    turns compaction leaves alone (default 6, as the app)
  --repeats N        samples per scenario (default 3)
  --jobs N           requests in flight at once (default 1)
  --rounds N         tool rounds allowed per step (default 8)
  --retries N        re-run a run the server never completed (default 2)
  --filter TEXT      only scenarios whose name contains TEXT
  --persona FILE     replace the persona, to A/B the prompt
  --label TEXT       what to call this run in the report
  --out FILE         write the report as JSON
  --baseline FILE    compare against an earlier report
  --no-catalogue     leave out the capability menu and `use_tools`, to measure
                     what carrying them in every prompt costs
  --overlap ARM      current (default), reword or disambiguate — how `workflow`
                     and `gh workflow` are told apart. Run the `overlap` family
                     in each arm against a --baseline of the same suite; adopt
                     one only if it gains more there than it costs in `github`
  --html FILE        write a browsable report: every scenario, what it expects,
                     and what the model did, with a box to disagree in
  --list             print the scenarios and stop
  --verbose          print every tool call as it happens";

/// Which summarizer the driver folds with between turns, or none.
///
/// `off` is the ceiling every other arm is measured against — the model reading
/// the whole thread. `model` is what the application does: a low-temperature
/// call to the same server, through `compaction::summary_request`. `headings`
/// is the offline fallback the application drops to when that call fails, and
/// the floor worth knowing because it is what shipped before.
///
/// **The arms fold whenever there is anything to fold**, ignoring
/// `compaction::should_fold`. That is deliberate: the gate decides how *often* a
/// real thread folds, and these scenarios exist to measure what one fold
/// *costs*. Ten short turns would never cross the gate on a 175k window, and a
/// suite that therefore never folded would report a difference of zero and mean
/// nothing by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compaction {
    Off,
    Headings,
    Model,
}

impl Compaction {
    fn parse(text: &str) -> Result<Self, String> {
        match text {
            "off" | "none" => Ok(Self::Off),
            "headings" => Ok(Self::Headings),
            "model" => Ok(Self::Model),
            other => Err(format!("unknown compaction mode {other}")),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Headings => "headings",
            Self::Model => "model",
        }
    }
}

struct Options {
    server: String,
    suite: String,
    compaction: Compaction,
    keep_recent: usize,
    repeats: usize,
    jobs: usize,
    rounds: usize,
    retries: usize,
    filter: Option<String>,
    persona: Option<String>,
    label: Option<String>,
    out: Option<String>,
    baseline: Option<String>,
    list: bool,
    verbose: bool,
    catalogue: bool,
    /// Which arm of the `gh workflow` overlap experiment to run under.
    overlap: Overlap,
    /// Where to write the browsable report, if anywhere.
    html: Option<String>,
}

impl Options {
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut options = Self {
            server: "http://127.0.0.1:8080".into(),
            suite: "prompt".into(),
            compaction: Compaction::Off,
            keep_recent: Settings::default().keep_recent_turns,
            repeats: 3,
            jobs: 1,
            rounds: DEFAULT_ROUNDS,
            retries: DEFAULT_RETRIES,
            filter: None,
            persona: None,
            label: None,
            out: None,
            baseline: None,
            list: false,
            verbose: false,
            catalogue: true,
            overlap: Overlap::default(),
            html: None,
        };
        let mut arguments = arguments.peekable();
        while let Some(argument) = arguments.next() {
            let mut value = || {
                arguments
                    .next()
                    .ok_or_else(|| format!("{argument} needs a value"))
            };
            match argument.as_str() {
                "--server" => options.server = value()?,
                "--suite" => options.suite = value()?,
                "--compaction" => options.compaction = Compaction::parse(&value()?)?,
                "--keep-recent" => options.keep_recent = number(&value()?)?.max(1),
                "--repeats" => options.repeats = number(&value()?)?,
                "--jobs" => options.jobs = number(&value()?)?.max(1),
                "--rounds" => options.rounds = number(&value()?)?.max(1),
                "--retries" => options.retries = number(&value()?)?,
                "--filter" => options.filter = Some(value()?),
                "--persona" => options.persona = Some(value()?),
                "--label" => options.label = Some(value()?),
                "--out" => options.out = Some(value()?),
                "--baseline" => options.baseline = Some(value()?),
                "--no-catalogue" => options.catalogue = false,
                "--overlap" => {
                    let arm = value()?;
                    options.overlap = Overlap::parse(&arm).ok_or_else(|| {
                        format!("--overlap is current, reword or disambiguate, not {arm:?}")
                    })?;
                }
                "--html" => options.html = Some(value()?),
                "--list" => options.list = true,
                "--verbose" | "-v" => options.verbose = true,
                "--help" | "-h" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                other => return Err(format!("unknown option {other}")),
            }
        }
        if options.repeats == 0 {
            return Err("--repeats must be at least 1".into());
        }
        if !matches!(
            options.suite.as_str(),
            "prompt" | "recall" | "memory" | "lookout"
        ) {
            return Err(format!("unknown suite {}", options.suite));
        }
        Ok(options)
    }
}

fn number(text: &str) -> Result<usize, String> {
    text.parse().map_err(|_| format!("{text} is not a number"))
}

// -- the harness -------------------------------------------------------------

struct Harness {
    client: Rc<Client>,
    scenarios: Vec<Scenario>,
    persona: String,
    rounds: usize,
    /// How the driver folds between turns, mirroring `ui::application`.
    compaction: Compaction,
    keep_recent: usize,
    /// Whether the capability catalogue and `use_tools` are in the prompt.
    ///
    /// On, because that is what ships. `--no-catalogue` turns it off, which is
    /// the only way to find out what carrying the menu in every prompt actually
    /// costs — the argument for the menu is that it is cheaper than switching
    /// every capability on, and that is a claim with a number behind it or it is
    /// nothing.
    catalogue: bool,
    verbose: bool,
    /// Every (scenario, repeat, attempt) still to run.
    queue: RefCell<VecDeque<(usize, usize, usize)>>,
    /// How many times a run that never completed is put back on the queue.
    retries: usize,
    /// Results by scenario, in the order they finish.
    results: RefCell<Vec<Vec<eval::report::Run>>>,
    running: Cell<usize>,
    finished: Cell<usize>,
    total: usize,
    model: RefCell<Option<String>>,
    main_loop: glib::MainLoop,
}

impl Harness {
    fn new(
        options: &Options,
        scenarios: Vec<Scenario>,
        persona: String,
        main_loop: glib::MainLoop,
    ) -> Self {
        let mut queue = VecDeque::new();
        for repeat in 0..options.repeats {
            for scenario in 0..scenarios.len() {
                queue.push_back((scenario, repeat, 0));
            }
        }
        let total = queue.len();
        let results = vec![Vec::new(); scenarios.len()];
        Self {
            client: Rc::new(Client::new(&options.server)),
            scenarios,
            persona,
            rounds: options.rounds,
            compaction: options.compaction,
            keep_recent: options.keep_recent,
            catalogue: options.catalogue,
            retries: options.retries,
            verbose: options.verbose,
            queue: RefCell::new(queue),
            results: RefCell::new(results),
            running: Cell::new(0),
            finished: Cell::new(0),
            total,
            model: RefCell::new(None),
            main_loop,
        }
    }

    fn report(&self, options: &Options) -> Report {
        let results = std::mem::take(&mut *self.results.borrow_mut());
        let scenarios = self
            .scenarios
            .iter()
            .zip(results)
            .map(|(scenario, runs)| ScenarioResult {
                name: scenario.name.to_string(),
                about: scenario.about.to_string(),
                tools: eval::switched_on(&scenario.tools),
                asks: eval::asked(scenario),
                runs,
            })
            .collect();
        Report {
            label: options.label.clone().unwrap_or_else(|| {
                options
                    .persona
                    .clone()
                    .unwrap_or_else(|| "stock persona".into())
            }),
            model: self.model.borrow().clone(),
            compaction: Some(options.compaction.label().to_string()),
            repeats: options.repeats,
            prompt_digest: eval::prompt_surface(&self.scenarios, &self.persona),
            scenarios,
        }
    }
}

/// One scenario, being run once.
struct Job {
    scenario: usize,
    repeat: usize,
    attempt: usize,
    /// The whole thread, system prompt first. Never shortened — what the model
    /// is sent is [`compaction::view`] of it, so a fold changes the request and
    /// leaves the record of what was actually asked intact.
    history: Vec<Message>,
    /// The rolling summary, as the application carries it on the thread.
    fold: Option<compaction::Fold>,
    declarations: Vec<ToolDeclaration>,
    /// What this conversation has switched on, which starts as the scenario's
    /// tool set and is not fixed: `use_tools` changes it mid-run, exactly as it
    /// does in the application, and the declarations and the system prompt are
    /// rebuilt from it when it does.
    tools: ToolSet,
    step: usize,
    round: usize,
    /// How many times each tool has been called this conversation, so a
    /// scenario's nth-call stub knows which call it is looking at.
    seen: HashMap<String, usize>,
    /// Searches this step has actually run, for [`Budget`]. Reset per ask, since
    /// the budget is per turn and each ask is a turn.
    searches: usize,
    /// The workflow this conversation has planned, carried across its asks
    /// exactly as the application carries it on the thread.
    ///
    /// The second thing the harness runs for real, after `use_tools`, and for
    /// the same reason: a `workflow` reply *is* the checklist the model reasons
    /// against next round, so a stub returning "ok" would leave it nothing to
    /// advance and the score would be measuring the fixture. Not reset between
    /// asks — a workflow spanning turns is the whole point of it.
    workflow: Option<Workflow>,
    /// The round cap has been reached and `LAST_ROUND` has been sent, so the
    /// next response is the turn's final word whatever it contains.
    last_chance: bool,
    trace: Trace,
    current: eval::trace::Step,
    started: Instant,
    generated: u32,
}

fn start_next(harness: &Rc<Harness>) {
    let Some((index, repeat, attempt)) = harness.queue.borrow_mut().pop_front() else {
        if harness.running.get() == 0 {
            harness.main_loop.quit();
        }
        return;
    };
    harness.running.set(harness.running.get() + 1);

    let scenario = &harness.scenarios[index];
    let job = Box::new(Job {
        scenario: index,
        repeat,
        attempt,
        history: vec![Message::system(eval::prompt_for(
            &scenario.tools,
            &harness.persona,
            harness.catalogue,
        ))],
        declarations: eval::declarations_for(&scenario.tools, harness.catalogue),
        tools: scenario.tools,
        fold: None,
        step: 0,
        round: 0,
        seen: HashMap::new(),
        searches: 0,
        workflow: None,
        last_chance: false,
        trace: Trace::default(),
        current: eval::trace::Step::default(),
        started: Instant::now(),
        generated: 0,
    });
    start_step(harness, job);
}

fn start_step(harness: &Rc<Harness>, mut job: Box<Job>) {
    let scenario = &harness.scenarios[job.scenario];
    let Some(ask) = scenario.asks.get(job.step) else {
        finish_job(harness, job);
        return;
    };

    if harness.verbose {
        eprintln!("\n  [{}·{}] > {}", scenario.name, job.repeat, ask.user);
    }
    job.history.push(Message::user(ask.user));
    job.round = 0;
    // Each ask is a turn, and the budget is per turn.
    job.searches = 0;
    job.last_chance = false;
    job.current = eval::trace::Step {
        user: ask.user.to_string(),
        ..eval::trace::Step::default()
    };
    fold_then_send(harness, job);
}

/// The thread below the system prompt, which is what compaction reasons about.
fn below_prompt(job: &Job) -> &[Message] {
    match job.history.split_first() {
        Some((first, rest)) if first.role == Role::System => rest,
        _ => &job.history,
    }
}

/// Bring the fold up to date, then send.
///
/// At a turn boundary only, for the reason the application has: folding mid-turn
/// changes the prompt under a turn already running and throws away the KV prefix
/// the server cached. Recursive because catching up can take more than one pass,
/// and because the `model` arm has to wait for a server round trip between them.
fn fold_then_send(harness: &Rc<Harness>, mut job: Box<Job>) {
    if harness.compaction == Compaction::Off {
        send(harness, job);
        return;
    }
    let Some((chunk, more)) =
        compaction::to_summarize(below_prompt(&job), job.fold.as_ref(), harness.keep_recent)
    else {
        send(harness, job);
        return;
    };

    if harness.compaction == Compaction::Headings {
        job.fold = Some(compaction::extend(
            job.fold.as_ref(),
            &chunk,
            more,
            &Headings,
        ));
        announce_fold(harness, &job, more);
        fold_then_send(harness, job);
        return;
    }

    let request =
        compaction::summary_request(job.fold.as_ref().map(|fold| fold.summary.as_str()), &chunk);
    let covers = job.fold.as_ref().map_or(0, |fold| fold.covers) + more;
    let stream = Rc::new(RefCell::new(TurnStream::new()));
    let harness = harness.clone();

    harness.clone().client.stream(
        &request,
        {
            let stream = stream.clone();
            move |text: &str| {
                stream.borrow_mut().push(text);
            }
        },
        move |outcome| {
            let summary = match outcome {
                Ok(()) => {
                    let state = std::mem::take(&mut *stream.borrow_mut()).finish();
                    let answer = state.answer.trim().to_string();
                    (!answer.is_empty()).then_some(answer)
                }
                Err(error) => {
                    // Falling back to `Headings` here would quietly turn this
                    // run into a different arm and average the two. A run whose
                    // summarizer never answered is not a result, and the harness
                    // already knows how to retry one.
                    job.trace.broken = Some(format!("the summarizer failed: {error}"));
                    finish_job(&harness, job);
                    return;
                }
            };
            let Some(summary) = summary else {
                job.trace.broken = Some("the summarizer returned nothing".into());
                finish_job(&harness, job);
                return;
            };
            job.fold = Some(compaction::Fold { summary, covers });
            announce_fold(&harness, &job, more);
            fold_then_send(&harness, job);
        },
    );
}

fn announce_fold(harness: &Rc<Harness>, job: &Job, more: usize) {
    if !harness.verbose {
        return;
    }
    let covers = job.fold.as_ref().map_or(0, |fold| fold.covers);
    eprintln!("      · folded {more} more turn(s); the summary now covers {covers}");
}

fn send(harness: &Rc<Harness>, job: Box<Job>) {
    // The fold is applied here rather than stored, exactly as `build_request`
    // does it: `job.history` stays the whole thread and only the request is
    // shortened.
    let mut messages = Vec::with_capacity(job.history.len());
    match job.history.split_first() {
        Some((prompt, rest)) if prompt.role == Role::System => {
            messages.push(prompt.clone());
            messages.extend(compaction::view(rest, job.fold.as_ref()));
        }
        _ => messages.extend(compaction::view(&job.history, job.fold.as_ref())),
    }

    let request = ChatRequest {
        messages,
        tools: job.declarations.clone(),
        max_tokens: Some(MAX_GENERATED),
        ..ChatRequest::new(Vec::new())
    };

    let stream = Rc::new(RefCell::new(TurnStream::new()));
    let cancellable = harness.client.stream(
        &request,
        {
            let stream = stream.clone();
            move |text: &str| {
                stream.borrow_mut().push(text);
            }
        },
        {
            let harness = harness.clone();
            let stream = stream.clone();
            move |outcome| {
                if let Err(error) = outcome {
                    let mut job = job;
                    job.trace.broken = Some(error.to_string());
                    finish_job(&harness, job);
                    return;
                }

                let mut borrowed = stream.borrow_mut();
                borrowed.end();
                // Captured before `finish`, which is where the app strips a
                // tool call the model wrote into its prose. The difference is
                // the leak.
                let raw = borrowed.state().answer.clone();
                let state = std::mem::take(&mut *borrowed).finish();
                drop(borrowed);

                let mut job = job;
                job.generated += state
                    .usage
                    .map(|usage| usage.completion_tokens)
                    .unwrap_or(0);
                job.current.thinking_chars += state.thinking.len();
                // Trimmed on both sides, because `strip_tool_noise` trims and
                // almost every answer ends in a newline. Comparing untrimmed
                // counted a trailing `\n` as a leaked tool call, which made the
                // commonest antipattern in the report one that had never
                // happened — 27 sightings in a family of nine scenarios.
                if raw.trim() != state.answer {
                    job.current.leaked = true;
                }
                job.current.recovered += state.recovered_calls;
                // A round that produced nothing is the hardest failure to read
                // from a trace, because there is by definition nothing in it.
                // The thinking is the only evidence of what the model meant.
                if harness.verbose && state.tool_calls.is_empty() && state.answer.trim().is_empty()
                {
                    let tail: String = state
                        .thinking
                        .chars()
                        .rev()
                        .take(400)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    eprintln!("      ! silent round; thinking tail: {tail:?}");
                }
                // What the *user* would see as nothing: no answer and no
                // call. Thinking does not count — it is behind a disclosure and
                // it is not a reply. This used to require the thinking to be
                // empty too, which meant the commonest silent round in this
                // suite was never counted as one.
                if state.answer.trim().is_empty() && state.tool_calls.is_empty() {
                    job.current.empty = true;
                }

                if state.tool_calls.is_empty() {
                    job.current.answer = state.answer;
                    job.history
                        .push(Message::assistant(job.current.answer.clone()));
                    finish_step(&harness, job);
                    return;
                }

                // Anything said in a round that also called tools is what the
                // model told the user it was about to do.
                if !state.answer.trim().is_empty() {
                    job.current.asides.push(eval::trace::Aside {
                        round: job.round,
                        text: state.answer.trim().to_string(),
                    });
                }

                // It asked for tools again after being told the last round was
                // over. The application stops here too, with "Stopped after too
                // many tool calls in one turn."
                if job.last_chance {
                    job.current.hit_round_cap = true;
                    job.current.answer = state.answer.clone();
                    record_calls(&harness, &mut job, &state.tool_calls);
                    finish_step(&harness, job);
                    return;
                }

                let results = record_calls(&harness, &mut job, &state.tool_calls);

                // The ceiling, handled the way `ui::application` handles it: the
                // results of the last round still go back, with a note saying it
                // was the last, and the model gets one more turn to write
                // something. Cutting the conversation off here instead — which
                // is what this did — scored a turn the application would have
                // ended with "I could not confirm that" as a turn that said
                // nothing at all.
                if job.round + 1 >= harness.rounds {
                    job.current.hit_round_cap = true;
                    job.history.push(assistant_turn(&state));
                    job.history.extend(results);
                    job.history.push(Message::user(LAST_ROUND));
                    job.last_chance = true;
                    job.round += 1;
                    job.current.rounds = job.round;
                    send(&harness, job);
                    return;
                }
                job.history.push(assistant_turn(&state));
                job.history.extend(results);
                // The same nudge the application sends, at the same point: once
                // a turn has made this many calls and still written nothing.
                if job.current.calls.len() >= WRAP_UP_AFTER
                    && job.current.preamble().trim().is_empty()
                {
                    job.history.push(Message::user(WRAP_UP));
                }

                job.round += 1;
                job.current.rounds = job.round;
                let _ = state.finish.unwrap_or(Finish::ToolCalls);
                send(&harness, job);
            }
        },
    );
    // The stream owns itself through the callbacks; nothing here cancels early.
    std::mem::forget(cancellable);
}

/// The assistant's side of a round that called tools, as it goes back on the
/// wire: whatever it said, and the calls it made.
fn assistant_turn(state: &familiar::model::turn::TurnState) -> Message {
    Message {
        role: Role::Assistant,
        content: (!state.answer.is_empty()).then(|| Content::Text(state.answer.clone())),
        reasoning_content: None,
        tool_calls: state
            .tool_calls
            .iter()
            .map(|call| {
                ToolInvocation::new(call.id.clone(), call.name.clone(), call.arguments.clone())
            })
            .collect(),
        tool_call_id: None,
    }
}

/// Record what the model asked for and invent the answers. No tool runs.
fn record_calls(
    harness: &Rc<Harness>,
    job: &mut Job,
    calls: &[familiar::model::turn::ToolCall],
) -> Vec<Message> {
    let scenario = &harness.scenarios[job.scenario];
    let stubs = &scenario.stubs;
    // What the user has typed by this point in the conversation, which is what
    // exempts a `fetch_url` from the budget. Taken once, before the loop, so it
    // is not held across the borrow of `job.seen` below.
    let said: Vec<String> = scenario.asks[..=job.step]
        .iter()
        .map(|ask| ask.user.to_string())
        .collect();
    let mut results = Vec::new();
    for call in calls {
        let nth = job.seen.entry(call.name.clone()).or_insert(0);

        // The application refuses a search once the turn has spent its budget,
        // and so does this — a rule the harness does not enforce is a rule the
        // score cannot see. The count is of searches that actually went out, so
        // a refusal does not push the number up on its own.
        let reply = if call.name == "workflow" {
            // Carried out for real against the job's own state, so `advance`
            // acts on the plan the model actually made rather than on a fixture
            // that agrees with anything. It goes through the same
            // `workflow::apply` the application calls.
            *nth += 1;
            eval::stub::workflow_reply(&mut job.workflow, &call.arguments)
        } else if call.name == "use_tools" {
            // The one call the harness carries out for real. Everything else
            // here is invented, but this one changes what the model may do
            // next, and a stub that said "switched on" while the tool list
            // stayed the same would measure a model reaching for tools it had
            // been told it had and did not.
            switch_on(job, &call.arguments, &harness.persona)
        } else if is_a_lookup(call, &said) && !Budget::allows(job.searches) {
            Reply::ok(Budget::refuse(job.searches))
        } else if is_a_search(&call.name) {
            job.searches += 1;
            *nth += 1;
            // Past the soft line the application appends the count and the
            // condition to the result, so the harness does too: a scenario
            // graded without it is graded on a shorter leash than the product
            // gives, and the whole question the budget answers is what the
            // model does when it is told what it has left.
            match (
                stubs.reply(&call.name, &call.arguments, *nth - 1),
                Budget::pressure(job.searches),
            ) {
                (Reply::Ok(text), Some(note)) => Reply::ok(text + &note),
                (reply, _) => reply,
            }
        } else {
            *nth += 1;
            stubs.reply(&call.name, &call.arguments, *nth - 1)
        };

        if harness.verbose {
            let marker = match reply {
                Reply::Ok(_) => "→",
                Reply::Failed(_) => "✗",
                Reply::Denied => "⊘",
            };
            eprintln!("      {marker} {}({})", call.name, elide(&call.arguments));
        }

        // The text the model reads next, kept on the trace so the report can
        // show it. Without this a reviewer sees "web_search → ok" and has no
        // way to tell whether the model ignored a good answer or reasonably
        // gave up on a bad one.
        let said_back = reply.for_model();
        job.current.calls.push(Call {
            round: job.round,
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            reaction: reply.reaction(),
            // The same verdict `ui::application` reaches before it puts a dialog
            // on screen. The harness approves it and carries on; recording it is
            // what stops a trace of `mail send …` reading as an email nobody
            // saw. Read from the arguments, so `gh pr list` and `gh pr merge`
            // are told apart the way they really are.
            gated: matches!(
                familiar::model::tools::gate_for(&call.name, &call.arguments),
                familiar::model::tools::Gate::Always
            ),
            result: said_back.clone(),
        });
        results.push(Message::tool_result(call.id.clone(), said_back));
    }
    results
}

/// Carry out a `use_tools` call: switch the capabilities on, then rebuild the
/// declarations and the system prompt from what the conversation now has.
///
/// Rebuilding the system message is the half that is easy to leave out. The
/// application composes it per round, so a capability switched on brings its
/// guidance with it; a harness that only added declarations would be scoring
/// the model on tools it had been given no instructions for.
fn switch_on(job: &mut Job, arguments: &str, persona: &str) -> Reply {
    use familiar::model::capability;

    let asked: Vec<String> = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| value.get("names").cloned())
        .map(|names| match names {
            serde_json::Value::Array(values) => values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect(),
            serde_json::Value::String(one) => vec![one],
            _ => Vec::new(),
        })
        .unwrap_or_default();
    if asked.is_empty() {
        return Reply::failed("`use_tools` needs the name of at least one capability.");
    }

    let mut turned_on = Vec::new();
    let mut already = Vec::new();
    let mut unknown = Vec::new();
    for name in &asked {
        let name = name.trim().to_lowercase();
        if capability::named(&name).is_none() {
            unknown.push(name);
        } else if capability::switch_on(&mut job.tools, &name) {
            turned_on.push(name);
        } else {
            already.push(name);
        }
    }
    if turned_on.is_empty() && already.is_empty() {
        return Reply::failed(format!(
            "there is no capability called {}",
            unknown.join(", ")
        ));
    }

    if !turned_on.is_empty() {
        job.declarations = eval::declarations_for(&job.tools, true);
        job.history[0] = Message::system(eval::prompt_for(&job.tools, persona, true));
    }
    Reply::ok(capability::switched(&turned_on, &already))
}

fn finish_step(harness: &Rc<Harness>, mut job: Box<Job>) {
    job.current.rounds = job.round.max(1);
    let step = std::mem::take(&mut job.current);
    job.trace.steps.push(step);
    job.step += 1;
    start_step(harness, job);
}

fn finish_job(harness: &Rc<Harness>, job: Box<Job>) {
    // A run the server never completed goes back on the queue rather than into
    // the report. This machine's GPU watchdog kills llama-server under a long
    // unbroken load — `CUDA error: the launch timed out` — and systemd brings it
    // straight back, which without this wrote off every scenario after the
    // crash as unmeasured.
    if job.trace.broken.is_some() && job.attempt < harness.retries {
        harness.running.set(harness.running.get() - 1);
        harness
            .queue
            .borrow_mut()
            .push_back((job.scenario, job.repeat, job.attempt + 1));
        eprint!("r");
        let harness = harness.clone();
        glib::timeout_add_seconds_local_once(RESTART_GRACE, move || start_next(&harness));
        return;
    }

    let scenario = &harness.scenarios[job.scenario];
    let mut run = eval::score(scenario, &job.trace);
    run.elapsed_ms = job.started.elapsed().as_millis() as u64;
    run.generated_tokens = job.generated;

    harness.results.borrow_mut()[job.scenario].push(run.clone());
    harness.running.set(harness.running.get() - 1);
    harness.finished.set(harness.finished.get() + 1);

    let mark = match (&run.broken, run.clean()) {
        (Some(_), _) => '!',
        (None, true) => '·',
        (None, false) => '✗',
    };
    eprint!("{mark}");
    if harness.finished.get() % 40 == 0 || harness.finished.get() == harness.total {
        eprintln!(" {}/{}", harness.finished.get(), harness.total);
    }
    if harness.verbose && !run.clean() {
        for failure in &run.failures {
            eprintln!("      ✗ {} — {}", failure.check, failure.detail);
        }
        for flaw in &run.flaws {
            eprintln!("      ! {} {}", flaw.kind, flaw.detail);
        }
    }

    start_next(harness);
}

fn elide(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 90 {
        return flat;
    }
    format!("{}…", flat.chars().take(87).collect::<String>())
}

// -- the memory suite ---------------------------------------------------------

/// The third suite runs a different shape of thing, so it gets a driver of its
/// own rather than an enum threaded through the one above.
///
/// Neither call it grades is a *turn*: there is no tool loop, no agentic
/// iteration and no conversation. One request, one reply, parsed and judged by
/// exactly the code the application uses before anything is written to a vault.
/// That last part is the point — a suite that scored the raw JSON would be
/// grading the model rather than what ships, and most of the safety in this
/// subsystem is in the vetting.
fn run_single(options: &Options, cases: Vec<Box<dyn eval::Graded>>, baseline: Option<Report>) {
    let cases: Vec<Box<dyn eval::Graded>> = cases
        .into_iter()
        .filter(|case| match &options.filter {
            Some(needle) => case.name().contains(needle.as_str()),
            None => true,
        })
        .collect();

    if options.list {
        for case in &cases {
            println!("{:<48} {}", case.name(), case.about());
        }
        return;
    }
    if cases.is_empty() {
        eprintln!("no case matches that filter");
        std::process::exit(2);
    }

    let main_loop = glib::MainLoop::new(None, false);
    let harness = Rc::new(MemoryHarness::new(options, cases, main_loop.clone()));

    eprintln!(
        "{} case(s) × {} run(s) = {} against {}",
        harness.cases.len(),
        options.repeats,
        harness.total,
        options.server
    );

    let started = harness.clone();
    let concurrency = options.jobs;
    harness.client.probe(move |info| {
        match info {
            Ok(info) => *started.model.borrow_mut() = info.model,
            Err(error) => {
                eprintln!("the server did not answer /props: {error}");
                started.main_loop.quit();
                return;
            }
        }
        for _ in 0..concurrency {
            next_case(&started);
        }
    });

    main_loop.run();

    let report = harness.report(options);
    print!("{}", eval::report::render(&report));

    if let Some(path) = &options.out {
        match serde_json::to_string_pretty(&report) {
            Ok(json) => match std::fs::write(path, json) {
                Ok(()) => eprintln!("\nwrote {path}"),
                Err(error) => eprintln!("\ncould not write {path}: {error}"),
            },
            Err(error) => eprintln!("\ncould not serialise the report: {error}"),
        }
    }

    if let Some(path) = &options.html {
        match std::fs::write(path, eval::report::render_html(&report)) {
            Ok(()) => eprintln!("wrote {path} — open it in a browser"),
            Err(error) => eprintln!("could not write {path}: {error}"),
        }
    }

    if let Some(baseline) = &baseline {
        println!();
        print!(
            "{}",
            eval::report::render_comparison(&report.against(baseline))
        );
    }
}

struct MemoryHarness {
    client: Rc<Client>,
    cases: Vec<Box<dyn eval::Graded>>,
    verbose: bool,
    queue: RefCell<VecDeque<(usize, usize, usize)>>,
    retries: usize,
    results: RefCell<Vec<Vec<eval::report::Run>>>,
    running: Cell<usize>,
    finished: Cell<usize>,
    total: usize,
    model: RefCell<Option<String>>,
    main_loop: glib::MainLoop,
}

impl MemoryHarness {
    fn new(
        options: &Options,
        cases: Vec<Box<dyn eval::Graded>>,
        main_loop: glib::MainLoop,
    ) -> Self {
        let mut queue = VecDeque::new();
        for repeat in 0..options.repeats {
            for case in 0..cases.len() {
                queue.push_back((case, repeat, 0));
            }
        }
        let total = queue.len();
        let results = vec![Vec::new(); cases.len()];
        Self {
            client: Rc::new(Client::new(&options.server)),
            cases,
            verbose: options.verbose,
            queue: RefCell::new(queue),
            retries: options.retries,
            results: RefCell::new(results),
            running: Cell::new(0),
            finished: Cell::new(0),
            total,
            model: RefCell::new(None),
            main_loop,
        }
    }

    fn report(&self, options: &Options) -> Report {
        let results = std::mem::take(&mut *self.results.borrow_mut());
        let scenarios = self
            .cases
            .iter()
            .zip(results)
            .map(|(case, runs)| ScenarioResult {
                name: case.name().to_string(),
                about: case.about().to_string(),
                tools: Vec::new(),
                // A single-call case has no user message, so the ask is the
                // signals it was shown; the expectations are the same shape as
                // everywhere else.
                asks: vec![eval::report::AskReport {
                    user: case.asked(),
                    checks: case.expectations(),
                }],
                runs,
            })
            .collect();
        Report {
            label: options
                .label
                .clone()
                .unwrap_or_else(|| options.suite.clone()),
            model: self.model.borrow().clone(),
            compaction: None,
            repeats: options.repeats,
            // What governs these two calls is not the persona — it is the
            // instructions compiled in beside them. A report that did not say
            // what they said would be a number with nothing attached to it.
            prompt_digest: match options.suite.as_str() {
                "lookout" => familiar::model::lookout::INSTRUCTIONS.to_string(),
                _ => format!(
                    "{}\n\n---\n\n{}",
                    familiar::model::memory::harvest::INSTRUCTIONS,
                    familiar::model::memory::dream::INSTRUCTIONS
                ),
            },
            scenarios,
        }
    }
}

fn next_case(harness: &Rc<MemoryHarness>) {
    let Some((index, repeat, attempt)) = harness.queue.borrow_mut().pop_front() else {
        if harness.running.get() == 0 {
            harness.main_loop.quit();
        }
        return;
    };
    harness.running.set(harness.running.get() + 1);

    let case = &harness.cases[index];
    if harness.verbose {
        eprintln!("\n  [{}·{repeat}]", case.name());
    }

    // A case the gate turns away is a result and not a skip: nothing was sent,
    // which for a turn with nothing in it is the right answer.
    let Some(request) = case.request() else {
        if harness.verbose {
            eprintln!("      · not worth reading; nothing was sent");
        }
        settle_case(harness, index, repeat, attempt, None, Instant::now(), 0);
        return;
    };

    let started = Instant::now();
    let stream = Rc::new(RefCell::new(TurnStream::new()));
    let cancellable = harness.client.stream(
        &request,
        {
            let stream = stream.clone();
            move |text: &str| {
                stream.borrow_mut().push(text);
            }
        },
        {
            let harness = harness.clone();
            let stream = stream.clone();
            move |outcome| {
                if let Err(error) = outcome {
                    settle_broken(&harness, index, repeat, attempt, error.to_string());
                    return;
                }
                let mut borrowed = stream.borrow_mut();
                borrowed.end();
                let state = std::mem::take(&mut *borrowed).finish();
                drop(borrowed);

                let generated = state
                    .usage
                    .map(|usage| usage.completion_tokens)
                    .unwrap_or(0);
                let reply = state.answer.trim().to_string();
                if reply.is_empty() {
                    settle_broken(
                        &harness,
                        index,
                        repeat,
                        attempt,
                        "the model answered with nothing at all".into(),
                    );
                    return;
                }
                if harness.verbose {
                    eprintln!("      → {}", elide(&reply));
                }
                settle_case(
                    &harness,
                    index,
                    repeat,
                    attempt,
                    Some(reply),
                    started,
                    generated,
                );
            }
        },
    );
    std::mem::forget(cancellable);
}

/// A run the server never completed goes back on the queue rather than into the
/// report, for the same reason the prompt harness does it: this machine's GPU
/// watchdog kills llama-server under a long unbroken load and systemd brings it
/// straight back.
fn settle_broken(
    harness: &Rc<MemoryHarness>,
    index: usize,
    repeat: usize,
    attempt: usize,
    why: String,
) {
    if attempt < harness.retries {
        harness.running.set(harness.running.get() - 1);
        harness
            .queue
            .borrow_mut()
            .push_back((index, repeat, attempt + 1));
        eprint!("r");
        let harness = harness.clone();
        glib::timeout_add_seconds_local_once(RESTART_GRACE, move || next_case(&harness));
        return;
    }
    let run = eval::report::Run {
        total: harness.cases[index].weight(),
        broken: Some(why),
        ..eval::report::Run::default()
    };
    record_case(harness, index, run);
}

fn settle_case(
    harness: &Rc<MemoryHarness>,
    index: usize,
    repeat: usize,
    attempt: usize,
    reply: Option<String>,
    started: Instant,
    generated: u32,
) {
    let _ = (repeat, attempt);
    let case = &harness.cases[index];
    let verdicts = case.verdicts(reply.as_deref());

    let run = eval::report::Run {
        passed: verdicts.iter().filter(|verdict| verdict.passed).count(),
        total: verdicts.len(),
        failures: verdicts
            .iter()
            .filter(|verdict| !verdict.passed)
            .map(|verdict| eval::report::Failure {
                check: verdict.check.clone(),
                detail: verdict.detail.clone(),
            })
            .collect(),
        verdicts: verdicts
            .iter()
            .map(|verdict| eval::report::VerdictReport {
                check: verdict.check.clone(),
                passed: verdict.passed,
                detail: verdict.detail.clone(),
            })
            .collect(),
        // One call, not a turn — so the whole "trace" is the reply, and the
        // artifact shows it in the same place it shows an answer.
        steps: vec![eval::report::StepReport {
            user: case.about().to_string(),
            calls: Vec::new(),
            preamble: String::new(),
            answer: eval::report::capped(
                reply.as_deref().unwrap_or_default(),
                eval::report::ANSWER_CAP,
            ),
        }],
        flaws: Vec::new(),
        calls: Vec::new(),
        rounds: usize::from(reply.is_some()),
        elapsed_ms: started.elapsed().as_millis() as u64,
        generated_tokens: generated,
        broken: None,
    };
    record_case(harness, index, run);
}

fn record_case(harness: &Rc<MemoryHarness>, index: usize, run: eval::report::Run) {
    harness.results.borrow_mut()[index].push(run.clone());
    harness.running.set(harness.running.get() - 1);
    harness.finished.set(harness.finished.get() + 1);

    let mark = match (&run.broken, run.clean()) {
        (Some(_), _) => '!',
        (None, true) => '·',
        (None, false) => '✗',
    };
    eprint!("{mark}");
    if harness.finished.get() % 40 == 0 || harness.finished.get() == harness.total {
        eprintln!(" {}/{}", harness.finished.get(), harness.total);
    }
    if harness.verbose && !run.clean() {
        for failure in &run.failures {
            eprintln!("      ✗ {} — {}", failure.check, failure.detail);
        }
    }

    next_case(harness);
}

fn baseline_of(options: &Options) -> Option<Report> {
    options.baseline.as_ref().map(|path| {
        let text = std::fs::read_to_string(path).unwrap_or_else(|error| {
            eprintln!("could not read {path}: {error}");
            std::process::exit(2);
        });
        serde_json::from_str::<Report>(&text).unwrap_or_else(|error| {
            eprintln!("{path} is not an eval report: {error}");
            std::process::exit(2);
        })
    })
}
