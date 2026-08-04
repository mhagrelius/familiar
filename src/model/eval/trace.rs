//! What the model actually did, recorded so it can be judged.
//!
//! A trace is deliberately *not* a transcript. It keeps the shape of the work —
//! which tool, with what arguments, in which round, and what the harness handed
//! back — and the final answer, because that is what the checks are about. The
//! thinking is kept only as a length: how much a model deliberated is worth
//! trending, and quoting it into every report is not.

use serde::{Deserialize, Serialize};

/// What the harness told the model a call returned. The harness never runs a
/// tool, so this is always something it decided rather than something that
/// happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reaction {
    Ok,
    Failed,
    /// The user said no at the approval dialog.
    Denied,
}

/// One tool call, as it was asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Call {
    /// Which round of the step it arrived in, from zero.
    pub round: usize,
    pub name: String,
    /// The arguments as they came off the wire: a JSON object, as a string.
    pub arguments: String,
    pub reaction: Reaction,
    /// Whether the application would have stopped and asked the user before
    /// running this.
    ///
    /// The harness approves everything, because a suite that stopped for input
    /// would not run. That is fine for scoring — the gates are unit-tested where
    /// they live — and it is misleading in a trace, where `mail send …` followed
    /// by "Sent." reads as an assistant firing off somebody's email unasked. It
    /// is not: `send` is `Gate::Always` and the user sees the exact argv. This
    /// records which calls those were so the report can say so.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gated: bool,
    /// What the harness handed back, verbatim — the text that went into the
    /// tool message the model read next.
    ///
    /// Kept because a reviewer cannot tell a prompt failure from a fixture
    /// failure without it. Every recurring complaint about this suite has been
    /// some version of "the model was given a bad result and did the sensible
    /// thing with it", and the report used to show the call and the reaction
    /// and nothing else — so the one artifact that could settle the question
    /// was the one thing missing from it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub result: String,
}

impl Default for Call {
    fn default() -> Self {
        Self {
            round: 0,
            name: String::new(),
            arguments: String::new(),
            reaction: Reaction::Ok,
            gated: false,
            result: String::new(),
        }
    }
}

impl Call {
    /// One argument, rendered as the text a check compares against.
    ///
    /// A string is itself; a list of strings is joined with spaces, which is
    /// what makes `gh`'s argv readable as `"pr list --state open"`; anything
    /// else is its compact JSON. `None` means the key is absent — or the whole
    /// object failed to parse, which [`Call::malformed`] reports separately.
    pub fn argument(&self, key: &str) -> Option<String> {
        let parsed: serde_json::Value = serde_json::from_str(&self.arguments).ok()?;
        let value = parsed.get(key)?;
        Some(render(value))
    }

    /// The model sent something that is not a JSON object.
    pub fn malformed(&self) -> bool {
        !matches!(
            serde_json::from_str::<serde_json::Value>(&self.arguments),
            Ok(serde_json::Value::Object(_))
        )
    }

    /// Name plus normalised arguments: two calls with the same signature are
    /// the same call, whatever whitespace or key order the model used.
    pub fn signature(&self) -> String {
        let normalised = serde_json::from_str::<serde_json::Value>(&self.arguments)
            .map(|value| render(&value))
            .unwrap_or_else(|_| self.arguments.trim().to_string());
        format!("{}({normalised})", self.name)
    }

    /// Every argument value the call carries, for checks that do not care which
    /// key something was under.
    pub fn all_arguments(&self) -> String {
        match serde_json::from_str::<serde_json::Value>(&self.arguments) {
            Ok(value) => render(&value),
            Err(_) => self.arguments.clone(),
        }
    }
}

/// A JSON value as the text a person would compare against.
fn render(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(render)
            .collect::<Vec<_>>()
            .join(" ")
            .to_string(),
        serde_json::Value::Object(fields) => {
            // Sorted, because serde_json preserves insertion order and two
            // models writing the same object in a different order are not two
            // different calls. `BTreeMap` is what the `preserve_order` feature
            // would give; sorting here does not depend on that feature.
            let mut pairs: Vec<String> = fields
                .iter()
                .map(|(key, value)| format!("{key}={}", render(value)))
                .collect();
            pairs.sort();
            pairs.join(" ")
        }
        other => other.to_string(),
    }
}

/// Everything that happened between one user message and the model's answer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub user: String,
    pub calls: Vec<Call>,
    /// The text the model finished with. Prose it wrote *alongside* a tool call
    /// is in [`Step::preamble`], not here.
    pub answer: String,
    /// Anything the model said before or between tool calls, kept round by
    /// round. The guidance asks it to say what it is about to do, and this is
    /// where that lands.
    ///
    /// Round by round rather than as one string because "said it *before*" is a
    /// real distinction and a flattened preamble cannot draw it: a model that
    /// sends the mail and then, in a later round, says what it sent, reads
    /// identically to one that said what it was about to send. See
    /// [`View::said_before`].
    pub asides: Vec<Aside>,
    pub rounds: usize,
    pub thinking_chars: usize,
    /// The harness stopped the step at its round ceiling.
    pub hit_round_cap: bool,
    /// The turn ended with no answer and no tool call at all.
    pub empty: bool,
    /// The model wrote a tool call into its prose instead of calling one. The
    /// app strips these, so this is the count of what it had to strip.
    pub leaked: bool,
    /// Tool calls the server left in `reasoning_content` and the fold rescued.
    ///
    /// Counted apart from `leaked` because it is a different fault with a
    /// different owner: the model wrote a correct call and llama.cpp failed to
    /// parse it (ggml-org/llama.cpp#22684). A turn that only worked because of
    /// the rescue is worth seeing even though it scored.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub recovered: usize,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// Something the model said in a round that also called tools.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aside {
    pub round: usize,
    pub text: String,
}

impl Step {
    /// Everything the model said alongside its calls, as one block. What the
    /// report prints and what most checks read.
    pub fn preamble(&self) -> String {
        self.asides
            .iter()
            .map(|aside| aside.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// What it said up to and including the given round.
    pub fn preamble_through(&self, round: usize) -> String {
        self.asides
            .iter()
            .filter(|aside| aside.round <= round)
            .map(|aside| aside.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Everything the model said this step, whether before a call or after the
    /// last one.
    pub fn said(&self) -> String {
        let mut said = self.preamble();
        if !said.is_empty() && !self.answer.is_empty() {
            said.push('\n');
        }
        said.push_str(&self.answer);
        said
    }
}

/// One whole conversation, run once.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Trace {
    pub steps: Vec<Step>,
    /// Set when the run never completed — the server refused, or the connection
    /// died. A failed run scores nothing rather than scoring zero, because a
    /// server that is down is not a prompt regression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broken: Option<String>,
}

impl Trace {
    pub fn calls(&self) -> impl Iterator<Item = &Call> {
        self.steps.iter().flat_map(|step| step.calls.iter())
    }
}

/// The slice of a trace a set of checks is judged against: one step, or all of
/// them.
#[derive(Debug, Clone, Copy)]
pub struct View<'a> {
    steps: &'a [Step],
}

impl<'a> View<'a> {
    pub fn whole(trace: &'a Trace) -> Self {
        Self {
            steps: &trace.steps,
        }
    }

    pub fn step(trace: &'a Trace, index: usize) -> Self {
        Self {
            steps: &trace.steps[index..=index],
        }
    }

    pub fn calls(&self) -> impl Iterator<Item = &Call> {
        self.steps.iter().flat_map(|step| step.calls.iter())
    }

    pub fn names(&self) -> Vec<&str> {
        self.calls().map(|call| call.name.as_str()).collect()
    }

    pub fn calls_to(&self, tool: &str) -> impl Iterator<Item = &Call> {
        let tool = tool.to_string();
        self.calls().filter(move |call| call.name == tool)
    }

    pub fn rounds(&self) -> usize {
        self.steps.iter().map(|step| step.rounds).sum()
    }

    pub fn hit_round_cap(&self) -> bool {
        self.steps.iter().any(|step| step.hit_round_cap)
    }

    /// Everything said across the view.
    pub fn said(&self) -> String {
        self.steps
            .iter()
            .map(Step::said)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// What the model said before it first reached for this tool, or `None` if
    /// it never did.
    ///
    /// Everything from earlier rounds, plus the prose written in the same round
    /// as the call — which is where "I'll email Ada and ask when the scaffolding
    /// comes down" lands, because the model writes it alongside the call rather
    /// than in a round of its own. Anything from a later round is a report of
    /// what it did, and this check is about the warning.
    pub fn said_before(&self, tool: &str) -> Option<String> {
        self.steps.iter().find_map(|step| {
            let at = step.calls.iter().find(|call| call.name == tool)?;
            Some(step.preamble_through(at.round))
        })
    }

    /// The last answer, which is the one the user would read.
    pub fn answer(&self) -> &str {
        self.steps
            .last()
            .map(|step| step.answer.as_str())
            .unwrap_or("")
    }

    pub fn steps(&self) -> &[Step] {
        self.steps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: &str) -> Call {
        Call {
            name: name.into(),
            arguments: arguments.into(),
            ..Call::default()
        }
    }

    #[test]
    fn an_argv_reads_as_the_command_line_it_is() {
        // The whole reason `gh` checks can say "did it pass --json" without
        // every one of them parsing JSON itself.
        let gh = call(
            "gh",
            r#"{"args":["pr","list","--state","open","--json","number"]}"#,
        );
        assert_eq!(
            gh.argument("args").as_deref(),
            Some("pr list --state open --json number")
        );
    }

    #[test]
    fn an_absent_argument_is_absent_rather_than_empty() {
        let weather = call("weather", "{}");
        assert_eq!(weather.argument("latitude"), None);
        assert!(!weather.malformed());
    }

    #[test]
    fn arguments_that_are_not_an_object_are_malformed() {
        assert!(call("recall", "\"scanner\"").malformed());
        assert!(call("recall", "{\"query\":").malformed());
        assert!(!call("recall", r#"{"query":"scanner"}"#).malformed());
    }

    #[test]
    fn the_same_call_written_two_ways_has_one_signature() {
        // What the repeated-call detector depends on: a model that reorders its
        // keys has not tried something different.
        let first = call("remember", r#"{"subject":"Zed","observation":"editor"}"#);
        let second = call("remember", r#"{ "observation":"editor", "subject":"Zed" }"#);
        assert_eq!(first.signature(), second.signature());

        let different = call("remember", r#"{"subject":"Zed","observation":"other"}"#);
        assert_ne!(first.signature(), different.signature());
    }

    fn aside(round: usize, text: &str) -> Aside {
        Aside {
            round,
            text: text.into(),
        }
    }

    #[test]
    fn a_step_reports_what_was_said_before_and_after_the_calls() {
        let step = Step {
            asides: vec![aside(0, "I'll write that to lists/shopping.md.")],
            answer: "Done.".into(),
            ..Step::default()
        };
        assert_eq!(step.said(), "I'll write that to lists/shopping.md.\nDone.");
    }

    #[test]
    fn saying_it_afterwards_is_not_saying_it_first() {
        // The distinction the mail and GitHub scenarios turn on. Both models
        // below say the word; only one of them said it while the message could
        // still be stopped.
        let warned = Trace {
            steps: vec![Step {
                asides: vec![aside(0, "I'll email Ada and ask about the scaffolding.")],
                calls: vec![Call {
                    name: "mail".into(),
                    ..Call::default()
                }],
                answer: "Sent.".into(),
                rounds: 2,
                ..Step::default()
            }],
            broken: None,
        };
        let reported = Trace {
            steps: vec![Step {
                asides: vec![aside(1, "I've sent Ada a note about the scaffolding.")],
                calls: vec![Call {
                    name: "mail".into(),
                    ..Call::default()
                }],
                answer: "Done.".into(),
                rounds: 3,
                ..Step::default()
            }],
            broken: None,
        };
        assert!(View::whole(&warned)
            .said_before("mail")
            .is_some_and(|said| said.contains("scaffolding")));
        assert_eq!(
            View::whole(&reported).said_before("mail").as_deref(),
            Some("")
        );
        // And a tool that was never called has no "before" to speak of.
        assert!(View::whole(&warned).said_before("gh").is_none());
    }

    #[test]
    fn a_view_of_one_step_sees_only_that_step() {
        let trace = Trace {
            steps: vec![
                Step {
                    user: "one".into(),
                    calls: vec![call("recall", "{}")],
                    rounds: 1,
                    ..Step::default()
                },
                Step {
                    user: "two".into(),
                    calls: vec![call("weather", "{}")],
                    rounds: 1,
                    ..Step::default()
                },
            ],
            broken: None,
        };
        assert_eq!(View::step(&trace, 1).names(), ["weather"]);
        assert_eq!(View::whole(&trace).names(), ["recall", "weather"]);
        assert_eq!(View::whole(&trace).rounds(), 2);
    }
}
