//! An eval harness for the system prompt.
//!
//! The question this answers is not "was the answer right" — it is "did the
//! model work the way the prompt told it to". So no tool is ever run: the
//! harness invents every result ([`stub`]), records what the model reached for
//! and in what order ([`trace`]), and judges the shape of that work against
//! assertions written beside the sentence of guidance they hold to account
//! ([`check`], [`suite`]). Antipatterns nobody predicted are counted separately
//! ([`antipattern`]), and two runs are compared scenario by scenario
//! ([`report`]) so a change to a paragraph of the prompt has a number attached
//! to it.
//!
//! Everything here is pure and display-free, so it runs in the same half of the
//! suite as the rest of `model`. The part that needs a server is
//! `examples/eval.rs`, which is the thing you run:
//!
//! ```sh
//! cargo run --release --example eval -- --repeats 3 --out baseline.json
//! cargo run --release --example eval -- --persona variant.txt --baseline baseline.json
//! ```
//!
//! It lives in the library rather than beside the driver because it is worth
//! testing, and `examples/` is not where tested code goes in this crate.

pub mod antipattern;
pub mod check;
pub mod lookout;
pub mod memory;
pub mod recall;
pub mod report;
pub mod scenario;
pub mod stub;
pub mod suite;
pub mod trace;
pub mod world;

use crate::model::capability;
use crate::model::instructions::Prompt;
use crate::model::project::ToolSet;
use crate::model::tools;
use check::Check;
use report::{Failure, Flaw, Run};
use scenario::Scenario;
use trace::Trace;

/// A case the single-call driver can run: one request, one reply, judged.
///
/// Two suites have this shape now — what the memory pass decides, and what the
/// proactive check decides — and neither is a *turn*: no tools, no agentic
/// loop, nothing to trace. Rather than a second driver, both answer these five
/// questions and `examples/eval.rs` runs whichever it was given.
pub trait Graded {
    fn name(&self) -> &'static str;
    fn about(&self) -> &'static str;
    fn weight(&self) -> usize;
    /// `None` when the application would not have sent anything at all, which
    /// is a result and not a skip.
    fn request(&self) -> Option<crate::model::wire::ChatRequest>;
    /// The reply, put through exactly the parsing and vetting that ships.
    fn verdicts(&self, reply: Option<&str>) -> Vec<check::Verdict>;

    /// What the model was shown, for a reviewer reading the artifact.
    ///
    /// There is no user message in a single-call suite — what stands in its
    /// place is the block of signals or the exchange being judged, which is the
    /// thing somebody needs in front of them to argue about the expectation.
    fn asked(&self) -> String {
        self.request()
            .and_then(|request| {
                request
                    .messages
                    .last()
                    .map(|message| message.text_of().to_string())
            })
            .unwrap_or_default()
    }

    /// The expectations, in the words the artifact prints.
    fn expectations(&self) -> Vec<String> {
        self.verdicts(None)
            .into_iter()
            .map(|verdict| verdict.check)
            .collect()
    }
}

impl Graded for memory::Case {
    fn name(&self) -> &'static str {
        self.name
    }
    fn about(&self) -> &'static str {
        self.about
    }
    fn weight(&self) -> usize {
        self.weight()
    }
    fn request(&self) -> Option<crate::model::wire::ChatRequest> {
        memory::Case::request(self, memory::today())
    }
    fn verdicts(&self, reply: Option<&str>) -> Vec<check::Verdict> {
        let outcome = match reply {
            Some(reply) => memory::Case::read(self, reply, memory::today()),
            None => memory::Outcome::default(),
        };
        memory::Case::judge(self, &outcome)
    }
}

impl Graded for lookout::Case {
    fn name(&self) -> &'static str {
        self.name
    }
    fn about(&self) -> &'static str {
        self.about
    }
    fn weight(&self) -> usize {
        self.weight()
    }
    fn request(&self) -> Option<crate::model::wire::ChatRequest> {
        lookout::Case::request(self)
    }
    fn verdicts(&self, reply: Option<&str>) -> Vec<check::Verdict> {
        let outcome = lookout::Case::read(self, reply);
        lookout::Case::judge(self, &outcome)
    }
}

/// The system prompt a scenario is asked under, composed exactly the way the
/// application composes it — same order, same capability notes, same framing of
/// the memory block. The only differences are that the date is fixed and the
/// persona can be swapped, which is the axis the harness exists to measure.
pub fn prompt(scenario: &Scenario, persona: &str) -> String {
    prompt_for(&scenario.tools, persona, true)
}

/// The prompt for a tool set that may no longer be the scenario's own.
///
/// `use_tools` changes what a conversation has partway through, and the
/// application rebuilds the whole system message on the next round because it
/// rebuilds it on *every* round. A harness that composed the prompt once at the
/// start would be measuring a model that had been handed tools nothing told it
/// about.
/// `catalogue` is what `--no-catalogue` turns off, so the cost of carrying the
/// menu in every prompt can be measured rather than argued about. It is the one
/// thing in this harness that is a *choice* rather than a copy of what the
/// application does, and it exists because that choice needed a number.
pub fn prompt_for(tools: &ToolSet, persona: &str, catalogue: bool) -> String {
    let mut capabilities = tools::guidance(tools, true);
    // Everything is assumed installed here. The suite's question is what the
    // model does with a capability it could reach, and whether this machine has
    // podman on it is not a property of the prompt.
    if catalogue {
        if let Some(note) = capability::catalogue(&capability::offerable(tools, |_| true)) {
            capabilities.push(note);
        }
    }
    Prompt {
        persona,
        // The suite measures the prompt this application ships, and it ships
        // with none: a project's instructions are the user's words, not the
        // application's, and scoring them would be scoring the fixture.
        instructions: None,
        capabilities: &capabilities,
        ambient: Some(scenario::AMBIENT),
        volatile: scenario::TODAY,
    }
    .compose()
}

/// The declarations a tool set offers, `use_tools` included unless it is being
/// measured by its absence.
pub fn declarations_for(
    tools: &ToolSet,
    catalogue: bool,
) -> Vec<crate::model::wire::ToolDeclaration> {
    let mut offered = tools::for_tools(tools, true);
    if catalogue {
        if let Some(tool) = tools::discovery_tool(&capability::offerable(tools, |_| true)) {
            offered.push(tool);
        }
    }
    offered.iter().map(tools::Tool::declaration).collect()
}

/// Every distinct piece of prompt text the suite put in front of the model:
/// the persona, then each capability note exactly once.
///
/// Recorded in the report because the persona is not the only thing worth
/// iterating on — most of what governs tool use is the guidance that lives
/// beside each tool, in `tools::guidance` and the modules it draws from. Those
/// are edited in source and compiled in, so without this a report from six
/// weeks ago is a number with nothing attached to it.
pub fn prompt_surface<'a>(
    scenarios: impl IntoIterator<Item = &'a Scenario>,
    persona: &str,
) -> String {
    let mut sections = vec![persona.trim().to_string()];
    for scenario in scenarios {
        for note in tools::guidance(&scenario.tools, true) {
            if !sections.contains(&note) {
                sections.push(note);
            }
        }
    }
    sections.join("\n\n---\n\n")
}

/// The tools a scenario offers, by name.
///
/// `use_tools` is in here when there is anything left to switch on, because
/// this is what the antipattern detector checks a call against — and a model
/// scored for calling a tool it *was* offered would be an antipattern report
/// nobody could trust.
pub fn offered(scenario: &Scenario) -> Vec<String> {
    names_for(&scenario.tools)
}

fn names_for(tools: &ToolSet) -> Vec<String> {
    let mut names: Vec<String> = tools::for_tools(tools, true)
        .iter()
        .map(|tool| tool.name.to_string())
        .collect();
    if !capability::offerable(tools, |_| true).is_empty() {
        names.push("use_tools".to_string());
    }
    names
}

/// Everything the model was offered by the *end* of a run, `use_tools` included.
///
/// A scenario's tool set is where a conversation starts and no longer where it
/// finishes. Scoring against the opening set reported twenty-seven sightings of
/// "called a tool it was not offered" for calls to a tool the model had just
/// been handed, correctly, one round earlier.
pub fn offered_by_the_end(scenario: &Scenario, trace: &Trace) -> Vec<String> {
    let mut tools = scenario.tools;
    for call in trace.steps.iter().flat_map(|step| step.calls.iter()) {
        if call.name != "use_tools" {
            continue;
        }
        // Whatever the arguments named, however they were shaped. This is
        // being generous on purpose: the question here is only whether a later
        // call had been legitimately unlocked.
        for wanted in capability::ALL {
            if call.arguments.contains(wanted.name) {
                capability::switch_on(&mut tools, wanted.name);
            }
        }
    }
    names_for(&tools)
}

/// One trace, judged: the scenario's own checks plus the antipatterns that
/// apply to every trace.
pub fn score(scenario: &Scenario, trace: &Trace) -> Run {
    if let Some(broken) = &trace.broken {
        return Run {
            total: scenario.weight(),
            broken: Some(broken.clone()),
            ..Run::default()
        };
    }

    let verdicts = scenario.judge(trace);
    let failures: Vec<Failure> = verdicts
        .iter()
        .filter(|verdict| !verdict.passed)
        .map(|verdict| Failure {
            check: verdict.check.clone(),
            detail: verdict.detail.clone(),
        })
        .collect();

    let flaws = antipattern::detect(trace, &offered_by_the_end(scenario, trace))
        .into_iter()
        .map(|sighting| Flaw {
            kind: sighting.kind.label().to_string(),
            detail: sighting.detail,
        })
        .collect();

    Run {
        passed: verdicts.iter().filter(|verdict| verdict.passed).count(),
        total: verdicts.len(),
        failures,
        flaws,
        calls: trace.calls().map(|call| call.name.clone()).collect(),
        verdicts: verdicts
            .iter()
            .map(|verdict| report::VerdictReport {
                check: verdict.check.clone(),
                passed: verdict.passed,
                detail: verdict.detail.clone(),
            })
            .collect(),
        steps: trace
            .steps
            .iter()
            .map(|step| report::StepReport {
                user: step.user.clone(),
                calls: step
                    .calls
                    .iter()
                    .map(|call| report::CallReport {
                        name: call.name.clone(),
                        arguments: report::capped(&call.arguments, report::ARGUMENT_CAP),
                        reaction: format!("{:?}", call.reaction).to_lowercase(),
                        gated: call.gated,
                        result: report::capped(&call.result, report::RESULT_CAP),
                    })
                    .collect(),
                preamble: report::capped(&step.preamble(), report::ANSWER_CAP),
                answer: report::capped(&step.answer, report::ANSWER_CAP),
            })
            .collect(),
        rounds: trace.steps.iter().map(|step| step.rounds).sum(),
        elapsed_ms: 0,
        generated_tokens: 0,
        broken: None,
    }
}

/// What a scenario asks and asserts, for the report.
pub fn asked(scenario: &Scenario) -> Vec<report::AskReport> {
    let mut asks: Vec<report::AskReport> = scenario
        .asks
        .iter()
        .map(|ask| report::AskReport {
            user: ask.user.to_string(),
            checks: ask.checks.iter().map(Check::to_string).collect(),
        })
        .collect();
    // Checks over the whole conversation belong to no single ask, and dropping
    // them would hide exactly the assertions a reviewer most wants to argue
    // with — `NeverSays`, `AtMostCalls`, and everything else that is about the
    // shape of the turn rather than one moment in it.
    let overall: Vec<String> = scenario.checks.iter().map(Check::to_string).collect();
    if !overall.is_empty() {
        asks.push(report::AskReport {
            user: String::new(),
            checks: overall,
        });
    }
    asks
}

/// Which capabilities a scenario switched on, by name.
pub fn switched_on(tools: &ToolSet) -> Vec<String> {
    let mut on = Vec::new();
    for (name, is_on) in [
        ("memory", tools.memory),
        ("web", tools.web),
        ("weather", tools.weather),
        ("workspace", tools.workspace),
        ("documents", tools.documents),
        ("python", tools.python),
        ("github", tools.github),
        ("planner", tools.planner),
        ("magpie", tools.magpie),
        ("mail", tools.mail),
        ("scheduling", tools.scheduling),
        ("escalate", tools.escalate),
    ] {
        if is_on {
            on.push(name.to_string());
        }
    }
    on
}

#[cfg(test)]
mod tests {
    use super::trace::{Call, Step};
    use super::*;
    use crate::model::instructions::DEFAULT_PERSONA;

    fn call(name: &str, arguments: &str) -> Call {
        Call {
            name: name.into(),
            arguments: arguments.into(),
            ..Call::default()
        }
    }

    #[test]
    fn the_prompt_is_the_applications_own_and_ends_with_the_fixed_date() {
        let scenario = &suite::all()[0];
        let composed = prompt(scenario, DEFAULT_PERSONA);
        assert!(composed.starts_with("You are Familiar"), "{composed}");
        assert!(composed.contains("## What you can do"), "{composed}");
        assert!(composed.contains("<saved_memory>"), "{composed}");
        assert!(composed.ends_with(scenario::TODAY), "{composed}");
    }

    #[test]
    fn a_scenario_only_carries_guidance_for_what_it_switched_on() {
        // Otherwise every scenario would be measuring the same prompt, and the
        // suite could not tell a documents regression from a web one.
        let offline = Scenario::new("x/y", "…", scenario::offline_workspace());
        let composed = prompt(&offline, DEFAULT_PERSONA);
        assert!(composed.contains("read_skill"), "{composed}");
        assert!(!composed.contains("web_search"), "{composed}");
    }

    #[test]
    fn swapping_the_persona_swaps_only_the_persona() {
        let scenario = &suite::all()[0];
        let stock = prompt(scenario, DEFAULT_PERSONA);
        let variant = prompt(scenario, "You are a terse assistant.");
        assert!(variant.starts_with("You are a terse assistant."));
        assert_eq!(
            stock.split_once("## What you can do").map(|(_, rest)| rest),
            variant
                .split_once("## What you can do")
                .map(|(_, rest)| rest)
        );
    }

    #[test]
    fn the_recorded_surface_holds_every_note_once_and_the_persona_first() {
        // A guidance edit in `tools.rs` is the main thing this harness exists
        // to measure, so a report has to say what that text said at the time.
        let scenarios = suite::all();
        let surface = prompt_surface(&scenarios, "You are a terse assistant.");
        assert!(surface.starts_with("You are a terse assistant."));
        for needle in ["`recall`", "`news`", "read_skill", "GitHub CLI", "weather"] {
            assert!(surface.contains(needle), "{needle} is not in the surface");
        }
        // Every scenario carries the declined-call note; it appears once.
        assert_eq!(surface.matches("that is their answer").count(), 1);
    }

    #[test]
    fn a_broken_run_keeps_the_scenarios_weight_and_scores_nothing() {
        let scenario = Scenario::new("x/y", "…", scenario::out_of_the_box())
            .ask("hello", [check::Check::NoTools, check::Check::Answers]);
        let run = score(
            &scenario,
            &Trace {
                steps: Vec::new(),
                broken: Some("connection refused".into()),
            },
        );
        assert_eq!(run.total, 2);
        assert_eq!(run.passed, 0);
        assert_eq!(run.broken.as_deref(), Some("connection refused"));
    }

    #[test]
    fn scoring_reports_both_the_failed_checks_and_the_antipatterns() {
        // The two axes, on one trace: the scenario expected `news` and got a
        // repeated `web_search`.
        let scenario = Scenario::new("web/x", "…", scenario::out_of_the_box()).ask(
            "what's new with Zig?",
            [check::Check::Calls("news"), check::Check::Answers],
        );
        let trace = Trace {
            steps: vec![Step {
                user: "what's new with Zig?".into(),
                calls: vec![
                    call("web_search", r#"{"query":"Zig"}"#),
                    call("web_search", r#"{"query":"Zig"}"#),
                ],
                answer: "Here is what I found.".into(),
                rounds: 2,
                ..Step::default()
            }],
            broken: None,
        };

        let run = score(&scenario, &trace);
        assert_eq!((run.passed, run.total), (1, 2));
        assert_eq!(run.failures.len(), 1);
        assert_eq!(run.failures[0].check, "calls news");
        assert_eq!(run.failures[0].detail, "called web_search → web_search");
        assert_eq!(run.flaws.len(), 1);
        assert_eq!(run.flaws[0].kind, "repeated an identical call");
        assert_eq!(run.calls, ["web_search", "web_search"]);
    }

    #[test]
    fn every_scenario_in_the_suite_composes_a_prompt_and_offers_its_tools() {
        for scenario in suite::all() {
            let composed = prompt(&scenario, DEFAULT_PERSONA);
            assert!(!composed.is_empty(), "{}", scenario.name);
            assert!(
                !offered(&scenario).is_empty(),
                "{} offers no tools at all",
                scenario.name
            );
        }
    }
}
