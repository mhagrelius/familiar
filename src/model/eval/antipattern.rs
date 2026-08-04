//! Mistakes worth counting whatever the scenario was.
//!
//! A scenario's checks say what *this* question should have looked like.
//! These say what no question should ever look like: asking twice for the same
//! thing, sending arguments that are not JSON, calling a tool that was not
//! offered, arguing with a decline, running six tools and then going quiet.
//!
//! They run over every trace in the suite, so the report has a second axis that
//! does not depend on anyone having predicted the failure. That is the axis
//! that catches a prompt change making the model worse in a way the suite was
//! not written to look for.

use std::collections::BTreeMap;

use super::trace::{Reaction, Step, Trace};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Antipattern {
    /// The same tool with the same arguments, twice. A tool that failed tells
    /// you why; repeating it unchanged is not fixing the call.
    RepeatedCall,
    /// Arguments that are not a JSON object. The call could not have run.
    MalformedArguments,
    /// A tool name that was never offered.
    UndeclaredTool,
    /// The user declined, and the model went back to the same tool.
    ArguedWithDecline,
    /// A required-looking string argument was empty.
    EmptyArgument,
    /// Four or more calls to one tool in a single step.
    Thrashed,
    /// Ran the harness out of rounds.
    RanOutOfRounds,
    /// Called tools and then produced no answer at all.
    WentQuiet,
    /// Wrote a tool call into its prose instead of calling one.
    LeakedIntoProse,
    /// The server left a well-formed call in `reasoning_content` and the fold
    /// had to rescue it. Not the model's fault, and worth seeing anyway.
    LeftInThinking,
    /// A whole step that produced nothing — no answer, no call.
    ProducedNothing,
}

impl Antipattern {
    pub const ALL: [Self; 11] = [
        Self::RepeatedCall,
        Self::MalformedArguments,
        Self::UndeclaredTool,
        Self::ArguedWithDecline,
        Self::EmptyArgument,
        Self::Thrashed,
        Self::RanOutOfRounds,
        Self::WentQuiet,
        Self::LeakedIntoProse,
        Self::LeftInThinking,
        Self::ProducedNothing,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::RepeatedCall => "repeated an identical call",
            Self::MalformedArguments => "sent malformed arguments",
            Self::UndeclaredTool => "called a tool it was not offered",
            Self::ArguedWithDecline => "retried after a decline",
            Self::EmptyArgument => "sent an empty argument",
            Self::Thrashed => "thrashed on one tool",
            Self::RanOutOfRounds => "ran out of rounds",
            Self::WentQuiet => "used tools and never answered",
            Self::LeakedIntoProse => "wrote a tool call as prose",
            Self::LeftInThinking => "call rescued from the thinking (llama.cpp#22684)",
            Self::ProducedNothing => "produced nothing",
        }
    }
}

impl std::fmt::Display for Antipattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// One sighting, with the call or step that caused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sighting {
    pub kind: Antipattern,
    pub step: usize,
    pub detail: String,
}

/// Every antipattern in a trace, given what the scenario actually offered.
pub fn detect(trace: &Trace, offered: &[String]) -> Vec<Sighting> {
    let mut sightings = Vec::new();
    for (index, step) in trace.steps.iter().enumerate() {
        in_step(step, index, offered, &mut sightings);
    }
    sightings
}

fn in_step(step: &Step, index: usize, offered: &[String], sightings: &mut Vec<Sighting>) {
    let mut see = |kind, detail: String| {
        sightings.push(Sighting {
            kind,
            step: index,
            detail,
        })
    };

    let mut seen: Vec<String> = Vec::new();
    let mut per_tool: BTreeMap<&str, usize> = BTreeMap::new();
    let mut declined: Vec<&str> = Vec::new();

    for call in &step.calls {
        *per_tool.entry(call.name.as_str()).or_default() += 1;

        let signature = call.signature();
        if seen.contains(&signature) {
            see(Antipattern::RepeatedCall, signature.clone());
        }
        seen.push(signature);

        if call.malformed() {
            see(
                Antipattern::MalformedArguments,
                format!("{}({})", call.name, elide(&call.arguments)),
            );
        }

        if !offered.contains(&call.name) {
            see(Antipattern::UndeclaredTool, call.name.clone());
        }

        if declined.contains(&call.name.as_str()) {
            see(Antipattern::ArguedWithDecline, call.name.clone());
        }
        if call.reaction == Reaction::Denied {
            declined.push(call.name.as_str());
        }

        if let Some(empty) = empty_argument(call) {
            see(Antipattern::EmptyArgument, format!("{}.{empty}", call.name));
        }
    }

    for (tool, count) in per_tool {
        if count >= 4 {
            see(Antipattern::Thrashed, format!("{tool} {count}×"));
        }
    }

    if step.hit_round_cap {
        see(
            Antipattern::RanOutOfRounds,
            format!("{} rounds", step.rounds),
        );
    }
    if !step.calls.is_empty() && step.answer.trim().is_empty() {
        see(
            Antipattern::WentQuiet,
            format!("{} calls, no answer", step.calls.len()),
        );
    }
    if step.leaked {
        see(Antipattern::LeakedIntoProse, String::new());
    }
    if step.recovered > 0 {
        see(
            Antipattern::LeftInThinking,
            format!("{} call(s) the server did not parse", step.recovered),
        );
    }
    if step.empty {
        see(Antipattern::ProducedNothing, String::new());
    }
}

/// The name of an argument whose value is an empty or whitespace string.
///
/// Only strings: an empty `sheets` array is a different mistake and a `0` is a
/// number, not an omission.
fn empty_argument(call: &super::trace::Call) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
    let fields = parsed.as_object()?;
    fields.iter().find_map(|(key, value)| {
        matches!(value, serde_json::Value::String(text) if text.trim().is_empty())
            .then(|| key.clone())
    })
}

fn elide(text: &str) -> String {
    if text.chars().count() <= 60 {
        return text.to_string();
    }
    format!("{}…", text.chars().take(57).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::super::trace::{Call, Reaction, Step, Trace};
    use super::*;

    fn offered() -> Vec<String> {
        ["recall", "remember", "web_search", "write_file"]
            .iter()
            .map(|name| name.to_string())
            .collect()
    }

    fn call(name: &str, arguments: &str, reaction: Reaction) -> Call {
        Call {
            name: name.into(),
            arguments: arguments.into(),
            reaction,
            ..Call::default()
        }
    }

    fn step(calls: Vec<Call>, answer: &str) -> Trace {
        Trace {
            steps: vec![Step {
                calls,
                answer: answer.into(),
                rounds: 1,
                ..Step::default()
            }],
            broken: None,
        }
    }

    fn kinds(trace: &Trace) -> Vec<Antipattern> {
        detect(trace, &offered())
            .into_iter()
            .map(|sighting| sighting.kind)
            .collect()
    }

    #[test]
    fn a_clean_trace_has_nothing_to_report() {
        let trace = step(
            vec![call("recall", r#"{"query":"roof"}"#, Reaction::Ok)],
            "Nothing in your notes about the roof.",
        );
        assert!(detect(&trace, &offered()).is_empty());
    }

    #[test]
    fn the_same_call_twice_is_caught_and_the_sighting_names_it() {
        let trace = step(
            vec![
                call("recall", r#"{"query":"roof"}"#, Reaction::Ok),
                call("recall", r#"{"query":"roof"}"#, Reaction::Ok),
            ],
            "Nothing there.",
        );
        let sightings = detect(&trace, &offered());
        assert_eq!(sightings.len(), 1);
        assert_eq!(sightings[0].kind, Antipattern::RepeatedCall);
        assert!(sightings[0].detail.contains("roof"));
    }

    #[test]
    fn going_back_to_a_tool_the_user_declined_is_caught() {
        // The guidance is explicit: a decline is their answer, not an obstacle.
        let trace = step(
            vec![
                call(
                    "write_file",
                    r#"{"path":"a.md","contents":"x"}"#,
                    Reaction::Denied,
                ),
                call(
                    "write_file",
                    r#"{"path":"b.md","contents":"x"}"#,
                    Reaction::Ok,
                ),
            ],
            "Saved it under a different name.",
        );
        assert_eq!(kinds(&trace), [Antipattern::ArguedWithDecline]);
    }

    #[test]
    fn a_decline_the_model_accepts_is_not_an_antipattern() {
        let trace = step(
            vec![call(
                "write_file",
                r#"{"path":"a.md","contents":"x"}"#,
                Reaction::Denied,
            )],
            "I did not write the file.",
        );
        assert!(detect(&trace, &offered()).is_empty());
    }

    #[test]
    fn a_tool_that_was_never_offered_is_caught() {
        let trace = step(
            vec![call("run_command", r#"{"command":"ls"}"#, Reaction::Failed)],
            "…",
        );
        assert!(kinds(&trace).contains(&Antipattern::UndeclaredTool));
    }

    #[test]
    fn running_tools_and_then_saying_nothing_is_caught() {
        let trace = step(vec![call("recall", r#"{"query":"x"}"#, Reaction::Ok)], "  ");
        assert_eq!(kinds(&trace), [Antipattern::WentQuiet]);
    }

    #[test]
    fn four_calls_to_one_tool_in_a_step_is_thrashing() {
        let queries = ["a", "b", "c", "d"];
        let trace = step(
            queries
                .iter()
                .map(|q| call("recall", &format!(r#"{{"query":"{q}"}}"#), Reaction::Ok))
                .collect(),
            "…",
        );
        assert_eq!(kinds(&trace), [Antipattern::Thrashed]);
    }

    #[test]
    fn an_empty_string_argument_is_caught() {
        let trace = step(
            vec![call("web_search", r#"{"query":"   "}"#, Reaction::Ok)],
            "…",
        );
        assert_eq!(kinds(&trace), [Antipattern::EmptyArgument]);
    }

    #[test]
    fn arguments_that_are_not_an_object_are_caught() {
        let trace = step(vec![call("recall", "roof", Reaction::Failed)], "…");
        assert!(kinds(&trace).contains(&Antipattern::MalformedArguments));
    }

    #[test]
    fn a_sighting_knows_which_step_it_was_in() {
        let trace = Trace {
            steps: vec![
                Step {
                    answer: "fine".into(),
                    ..Step::default()
                },
                Step {
                    calls: vec![call("recall", r#"{"query":""}"#, Reaction::Ok)],
                    answer: "also fine".into(),
                    ..Step::default()
                },
            ],
            broken: None,
        };
        let sightings = detect(&trace, &offered());
        assert_eq!(sightings[0].step, 1);
    }
}
