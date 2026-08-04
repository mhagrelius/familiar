//! What a scenario asserts about the model's behaviour.
//!
//! Every check here is about *how* the model worked, never about whether a
//! tool's answer was right — the harness makes those answers up. So there is
//! nothing that compares a forecast to the sky, and quite a lot that compares
//! the shape of a call to what the prompt told the model to do: the query it
//! wrote, the order it did things in, whether it read the skill before writing
//! the document, whether it asked twice for the same thing.
//!
//! Checks are values rather than closures so a run can be serialised, compared
//! against a baseline, and read in a report by someone who did not write it.

use super::trace::View;

/// One assertion about a [`View`] of a trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// The tool was called at least once.
    Calls(&'static str),
    /// The tool was called at least this many times. For guidance that asks for
    /// persistence — two or three `recall` attempts before giving up.
    CallsAtLeast(&'static str, usize),
    /// The tool was called at most this many times.
    ///
    /// Not the same as [`Check::AtMostCalls`], and the difference is what a
    /// scenario about *one* tool needs: a decline is answered by not writing
    /// again, and a total ceiling either forbids the orienting `list_dir` that
    /// has nothing to do with it or leaves room for a second write. Not the
    /// same as [`Check::NoRepeatOf`] either — that one only sees a retry that
    /// is byte-identical, and a model rewording the file it was just refused
    /// has still ignored the refusal.
    CallsAtMost(&'static str, usize),
    /// The tool was never called. Most of the value in the suite is here: a
    /// prompt regression usually shows up as reaching for the wrong tool, not
    /// as failing to reach for the right one.
    NeverCalls(&'static str),
    /// Exactly one of these, and not the others.
    CallsOnly(&'static [&'static str]),
    /// At least one of these was called. For a question where going to look is
    /// the point and either door is a reasonable one — `web_search` or `news`
    /// both count as refusing to answer a current question from memory.
    CallsAny(&'static [&'static str]),
    /// No tool at all. The answer was already in the model, in the context, or
    /// in the question.
    NoTools,
    /// The first thing it reached for.
    FirstCallIs(&'static str),
    /// `a` was called before `b` was. Vacuously true if `b` never was, so pair
    /// it with `Calls(b)` when the ordering is the point.
    Before(&'static str, &'static str),
    /// Some call to the tool has this key, and its rendered value contains the
    /// needle, case-insensitively.
    ArgContains {
        tool: &'static str,
        key: &'static str,
        needle: &'static str,
    },
    /// No call to the tool has a value containing the needle, under any key.
    ArgNever {
        tool: &'static str,
        needle: &'static str,
    },
    /// No call to the tool had *this key* set to something containing the
    /// needle.
    ///
    /// The keyed negative, and the one to reach for when the needle is a short
    /// word. `ArgNever { tool: "workflow", needle: "plan" }` looks like "it
    /// never planned" and is not: it searches every argument of every call, so
    /// a model quoting a budget table with a **Plan**ned column into an
    /// `outcome` fails it. That happened, and the trace it failed was perfect.
    ArgNeverAt {
        tool: &'static str,
        key: &'static str,
        needle: &'static str,
    },
    /// Every call to the tool carries this key.
    ArgPresent {
        tool: &'static str,
        key: &'static str,
    },
    /// No call to the tool carries this key.
    ArgAbsent {
        tool: &'static str,
        key: &'static str,
    },
    /// Every call's value for this key is at most this many words. What tells a
    /// bare `news` topic from a search query written into it.
    ArgWordsAtMost {
        tool: &'static str,
        key: &'static str,
        words: usize,
    },
    /// Every call's value for this key is at least this many words. What tells
    /// a semantic `web_search` phrase from two keywords.
    ArgWordsAtLeast {
        tool: &'static str,
        key: &'static str,
        words: usize,
    },
    /// No two calls to this tool were the same call.
    NoRepeatOf(&'static str),
    AtMostCalls(usize),
    AtMostRounds(usize),
    /// The model said something, rather than running tools and going quiet.
    Answers,
    /// Something it said — before, between or after the calls — contains one of
    /// these, case-insensitively.
    Says(&'static [&'static str]),
    /// Something it said *before* the first call to this tool contains one of
    /// these.
    ///
    /// The difference between saying what you are about to do and reporting
    /// what you did, and the only way to score the guidance that asks for the
    /// first. `Says` cannot: a model that sends the mail and then tells the
    /// user what it sent passes it, and the whole point of "say what it will
    /// say first" is that a sent message cannot be recalled. Fails if the tool
    /// was never called, because there is then nothing this is about.
    SaysBefore {
        tool: &'static str,
        words: &'static [&'static str],
    },
    /// Nothing it said contains any of these.
    NeverSays(&'static [&'static str]),
}

impl std::fmt::Display for Check {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Calls(tool) => write!(f, "calls {tool}"),
            Self::CallsAtLeast(tool, n) => write!(f, "calls {tool} at least {n}×"),
            Self::CallsAtMost(tool, n) => write!(f, "calls {tool} at most {n}×"),
            Self::NeverCalls(tool) => write!(f, "never calls {tool}"),
            Self::CallsOnly(tools) => write!(f, "calls only {}", tools.join("/")),
            Self::CallsAny(tools) => write!(f, "calls one of {}", tools.join("/")),
            Self::NoTools => write!(f, "uses no tools"),
            Self::FirstCallIs(tool) => write!(f, "reaches for {tool} first"),
            Self::Before(a, b) => write!(f, "{a} before {b}"),
            Self::ArgContains { tool, key, needle } => {
                write!(f, "{tool}.{key} contains {needle:?}")
            }
            Self::ArgNever { tool, needle } => write!(f, "{tool} never mentions {needle:?}"),
            Self::ArgNeverAt { tool, key, needle } => {
                write!(f, "{tool}.{key} is never {needle:?}")
            }
            Self::ArgPresent { tool, key } => write!(f, "{tool} passes {key}"),
            Self::ArgAbsent { tool, key } => write!(f, "{tool} omits {key}"),
            Self::ArgWordsAtMost { tool, key, words } => {
                write!(f, "{tool}.{key} is at most {words} words")
            }
            Self::ArgWordsAtLeast { tool, key, words } => {
                write!(f, "{tool}.{key} is at least {words} words")
            }
            Self::NoRepeatOf(tool) => write!(f, "never repeats a {tool} call"),
            Self::AtMostCalls(n) => write!(f, "at most {n} tool calls"),
            Self::AtMostRounds(n) => write!(f, "at most {n} rounds"),
            Self::Answers => write!(f, "answers"),
            Self::Says(words) => write!(f, "says one of {words:?}"),
            Self::SaysBefore { tool, words } => {
                write!(f, "says one of {words:?} before calling {tool}")
            }
            Self::NeverSays(words) => write!(f, "says none of {words:?}"),
        }
    }
}

/// A check, judged, with enough of what happened to explain itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub check: String,
    pub passed: bool,
    /// What the model did instead, when it failed. Empty when it passed.
    pub detail: String,
}

impl Check {
    pub fn judge(&self, view: &View) -> Verdict {
        let (passed, detail) = self.assess(view);
        Verdict {
            check: self.to_string(),
            passed,
            detail: if passed { String::new() } else { detail },
        }
    }

    fn assess(&self, view: &View) -> (bool, String) {
        match self {
            Self::Calls(tool) => {
                let count = view.calls_to(tool).count();
                (count > 0, format!("called {}", called(view)))
            }
            Self::CallsAtLeast(tool, least) => {
                let count = view.calls_to(tool).count();
                (
                    count >= *least,
                    format!("called {tool} {count}× — {}", called(view)),
                )
            }
            Self::CallsAtMost(tool, most) => {
                let count = view.calls_to(tool).count();
                (
                    count <= *most,
                    format!("called {tool} {count}× — {}", called(view)),
                )
            }
            Self::NeverCalls(tool) => {
                let count = view.calls_to(tool).count();
                (count == 0, format!("called {tool} {count}×"))
            }
            Self::CallsOnly(allowed) => {
                let stray: Vec<&str> = view
                    .names()
                    .into_iter()
                    .filter(|name| !allowed.contains(name))
                    .collect();
                (
                    stray.is_empty(),
                    format!("also called {}", stray.join(", ")),
                )
            }
            Self::CallsAny(wanted) => {
                let names = view.names();
                let found = names.iter().any(|name| wanted.contains(name));
                (found, format!("called {}", called(view)))
            }
            Self::NoTools => {
                let names = view.names();
                (names.is_empty(), format!("called {}", names.join(", ")))
            }
            Self::FirstCallIs(tool) => match view.names().first() {
                Some(first) => (first == tool, format!("reached for {first} first")),
                None => (false, "called nothing".into()),
            },
            Self::Before(first, second) => {
                let names = view.names();
                let at = |wanted: &str| names.iter().position(|name| *name == wanted);
                match (at(first), at(second)) {
                    (_, None) => (true, String::new()),
                    (None, Some(_)) => (false, format!("{second} without {first}")),
                    (Some(a), Some(b)) => (a < b, called(view)),
                }
            }
            Self::ArgContains { tool, key, needle } => {
                let values: Vec<String> = view
                    .calls_to(tool)
                    .filter_map(|call| call.argument(key))
                    .collect();
                let found = values
                    .iter()
                    .any(|value| value.to_lowercase().contains(&needle.to_lowercase()));
                // Distinguished, because `read_skill.name was ""` reads as the
                // model having sent an empty name — which is alarming, and is
                // not what happened. It never called `read_skill` at all, and
                // that is a different failure with a different fix. Two
                // reviewers of the same report stopped on that line.
                (found, missing_or(view, tool, key, &values))
            }
            Self::ArgNeverAt { tool, key, needle } => {
                let offender = view
                    .calls_to(tool)
                    .filter_map(|call| call.argument(key))
                    .find(|value| value.to_lowercase().contains(&needle.to_lowercase()));
                (
                    offender.is_none(),
                    format!("{key} was {:?}", offender.unwrap_or_default()),
                )
            }
            Self::ArgNever { tool, needle } => {
                let offender = view
                    .calls_to(tool)
                    .map(|call| call.all_arguments())
                    .find(|arguments| arguments.to_lowercase().contains(&needle.to_lowercase()));
                (
                    offender.is_none(),
                    format!("called {tool} with {:?}", offender.unwrap_or_default()),
                )
            }
            Self::ArgPresent { tool, key } => {
                let mut missing = 0;
                let mut total = 0;
                for call in view.calls_to(tool) {
                    total += 1;
                    if call.argument(key).is_none() {
                        missing += 1;
                    }
                }
                (
                    total > 0 && missing == 0,
                    if total == 0 {
                        format!("never called {tool}")
                    } else {
                        format!("{missing} of {total} calls left {key} out")
                    },
                )
            }
            Self::ArgAbsent { tool, key } => {
                let present: Vec<String> = view
                    .calls_to(tool)
                    .filter_map(|call| call.argument(key))
                    .collect();
                (
                    present.is_empty(),
                    format!("passed {key}={:?}", present.join(" | ")),
                )
            }
            Self::ArgWordsAtMost { tool, key, words } => {
                let offender = view
                    .calls_to(tool)
                    .filter_map(|call| call.argument(key))
                    .find(|value| count_words(value) > *words);
                (
                    offender.is_none(),
                    format!("{key} was {:?}", offender.unwrap_or_default()),
                )
            }
            Self::SaysBefore { tool, words } => {
                let Some(said) = view.said_before(tool) else {
                    return (false, format!("never called {tool}"));
                };
                let lowered = said.to_lowercase();
                let found = words
                    .iter()
                    .any(|word| lowered.contains(&word.to_lowercase()));
                (
                    found,
                    if said.trim().is_empty() {
                        format!("said nothing before calling {tool}")
                    } else {
                        format!("said {:?} first", elide(&said))
                    },
                )
            }
            Self::ArgWordsAtLeast { tool, key, words } => {
                let values: Vec<String> = view
                    .calls_to(tool)
                    .filter_map(|call| call.argument(key))
                    .collect();
                let offender = values.iter().find(|value| count_words(value) < *words);
                (
                    !values.is_empty() && offender.is_none(),
                    match offender {
                        Some(value) => format!("{key} was {value:?}"),
                        None => missing_or(view, tool, key, &values),
                    },
                )
            }
            Self::NoRepeatOf(tool) => {
                let mut seen = Vec::new();
                let mut repeated = None;
                for call in view.calls_to(tool) {
                    let signature = call.signature();
                    if seen.contains(&signature) {
                        repeated = Some(signature);
                        break;
                    }
                    seen.push(signature);
                }
                (
                    repeated.is_none(),
                    format!("asked twice for {}", repeated.unwrap_or_default()),
                )
            }
            Self::AtMostCalls(most) => {
                let count = view.calls().count();
                (count <= *most, format!("made {count}: {}", called(view)))
            }
            Self::AtMostRounds(most) => {
                let rounds = view.rounds();
                (rounds <= *most, format!("took {rounds} rounds"))
            }
            Self::Answers => {
                let answer = view.answer().trim();
                (!answer.is_empty(), "finished with no answer".into())
            }
            Self::Says(words) => {
                let said = view.said().to_lowercase();
                let found = words.iter().any(|word| said.contains(&word.to_lowercase()));
                (found, elide(&view.said()))
            }
            Self::NeverSays(words) => {
                let said = view.said().to_lowercase();
                let found = words
                    .iter()
                    .find(|word| said.contains(&word.to_lowercase()));
                (
                    found.is_none(),
                    format!("said {:?}", found.copied().unwrap_or_default()),
                )
            }
        }
    }
}

/// Why an argument check failed, told apart from *whether the call was made*.
///
/// `read_skill.name was ""` and `create_pdf.path was ""` both appeared in a
/// report where neither tool had been called once, and both read as the model
/// having sent an empty string. They are different failures — one is "it did not
/// do the thing", the other is "it did the thing wrong" — and only one of them
/// is about the argument the check names.
fn missing_or(view: &View, tool: &str, key: &str, values: &[String]) -> String {
    if view.calls_to(tool).count() == 0 {
        return format!("never called {tool} — called {}", called(view));
    }
    if values.is_empty() {
        return format!("called {tool}, but never with {key}");
    }
    format!("{key} was {:?}", values.join(" | "))
}

/// Words as a person counts them, so `"gh pr list"` is three and `"AI"` is one.
fn count_words(value: &str) -> usize {
    value
        .split_whitespace()
        .filter(|word| !word.is_empty())
        .count()
}

fn called(view: &View) -> String {
    let names = view.names();
    if names.is_empty() {
        "nothing".to_string()
    } else {
        names.join(" → ")
    }
}

/// Enough of an answer to see what went wrong, and no more.
fn elide(text: &str) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= 160 {
        return flattened;
    }
    let cut: String = flattened.chars().take(157).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::super::trace::{Call, Step, Trace};
    use super::*;

    fn call(name: &str, arguments: &str) -> Call {
        Call {
            name: name.into(),
            arguments: arguments.into(),
            ..Call::default()
        }
    }

    fn trace(calls: Vec<Call>, answer: &str) -> Trace {
        Trace {
            steps: vec![Step {
                user: "…".into(),
                rounds: 1,
                calls,
                answer: answer.into(),
                ..Step::default()
            }],
            broken: None,
        }
    }

    fn judge(check: Check, trace: &Trace) -> Verdict {
        check.judge(&View::whole(trace))
    }

    #[test]
    fn a_failing_check_says_what_happened_instead() {
        // The report is only useful if a failure names the wrong thing the
        // model reached for.
        let trace = trace(vec![call("web_search", r#"{"query":"weather Tokyo"}"#)], "");
        let verdict = judge(Check::Calls("weather"), &trace);
        assert!(!verdict.passed);
        assert_eq!(verdict.detail, "called web_search");
        assert_eq!(verdict.check, "calls weather");
    }

    #[test]
    fn a_passing_check_carries_no_detail() {
        let trace = trace(vec![call("weather", "{}")], "Rain this afternoon.");
        let verdict = judge(Check::Calls("weather"), &trace);
        assert!(verdict.passed);
        assert!(verdict.detail.is_empty());
    }

    #[test]
    fn ordering_is_vacuous_when_the_later_tool_was_never_called() {
        // `read_skill before create_document` must not fail a run that
        // reasonably made no document at all; the Calls check is what says the
        // document was required.
        let trace = trace(vec![call("list_dir", r#"{"path":"."}"#)], "");
        assert!(judge(Check::Before("read_skill", "create_document"), &trace).passed);
    }

    #[test]
    fn ordering_fails_when_the_later_tool_ran_without_the_earlier_one() {
        let trace = trace(
            vec![call(
                "create_document",
                r#"{"path":"a.docx","markdown":"x"}"#,
            )],
            "",
        );
        let verdict = judge(Check::Before("read_skill", "create_document"), &trace);
        assert!(!verdict.passed);
        assert_eq!(verdict.detail, "create_document without read_skill");
    }

    #[test]
    fn a_bare_topic_passes_the_word_ceiling_and_a_query_written_into_it_does_not() {
        // The `news` antipattern the guidance is explicitly about.
        let bare = trace(vec![call("news", r#"{"topic":"Zig"}"#)], "");
        assert!(
            judge(
                Check::ArgWordsAtMost {
                    tool: "news",
                    key: "topic",
                    words: 3
                },
                &bare
            )
            .passed
        );

        let query = trace(
            vec![call(
                "news",
                r#"{"topic":"recent news about the Zig programming language release"}"#,
            )],
            "",
        );
        let verdict = judge(
            Check::ArgWordsAtMost {
                tool: "news",
                key: "topic",
                words: 3,
            },
            &query,
        );
        assert!(!verdict.passed);
        assert!(verdict.detail.contains("Zig programming language"));
    }

    #[test]
    fn either_door_counts_when_the_point_is_going_to_look() {
        let searched = trace(vec![call("web_search", r#"{"query":"latest Zig"}"#)], "");
        let asked_news = trace(vec![call("news", r#"{"topic":"Zig"}"#)], "");
        let neither = trace(vec![call("recall", r#"{"query":"Zig"}"#)], "");
        let check = Check::CallsAny(&["web_search", "news"]);
        assert!(judge(check.clone(), &searched).passed);
        assert!(judge(check.clone(), &asked_news).passed);
        let verdict = judge(check, &neither);
        assert!(!verdict.passed);
        assert_eq!(verdict.detail, "called recall");
    }

    #[test]
    fn a_word_floor_fails_a_tool_that_was_never_called_with_the_key() {
        // Otherwise "every query is long enough" is trivially true of a run
        // that searched for nothing.
        let empty = trace(Vec::new(), "");
        assert!(
            !judge(
                Check::ArgWordsAtLeast {
                    tool: "web_search",
                    key: "query",
                    words: 5
                },
                &empty
            )
            .passed
        );
    }

    /// The keyed negative exists because the unkeyed one is a trap for a short
    /// needle, and the trap was sprung on a real run: the model started a saved
    /// workflow, worked through it correctly, and quoted a budget table with a
    /// `Planned` column into an `outcome`. `ArgNever { needle: "plan" }` read
    /// that as "it planned" and failed a perfect trace.
    #[test]
    fn the_keyed_negative_reads_one_key_and_the_unkeyed_one_reads_everything() {
        let trace = trace(
            vec![call(
                "workflow",
                r#"{"action":"advance","outcome":"Roof: Planned 14000, Spent 13850"}"#,
            )],
            "",
        );
        assert!(
            judge(
                Check::ArgNeverAt {
                    tool: "workflow",
                    key: "action",
                    needle: "plan"
                },
                &trace
            )
            .passed,
            "the action was `advance`, whatever the outcome says"
        );
        assert!(
            !judge(
                Check::ArgNever {
                    tool: "workflow",
                    needle: "plan"
                },
                &trace
            )
            .passed,
            "the unkeyed form sees `Planned` — which is exactly why it is the wrong instrument"
        );
        // And it still catches the thing it is for.
        assert!(
            !judge(
                Check::ArgNeverAt {
                    tool: "workflow",
                    key: "action",
                    needle: "advance"
                },
                &trace
            )
            .passed
        );
    }

    #[test]
    fn an_argv_is_searched_as_a_command_line() {
        let trace = trace(
            vec![call(
                "gh",
                r#"{"args":["pr","list","--json","number,title"]}"#,
            )],
            "",
        );
        assert!(
            judge(
                Check::ArgContains {
                    tool: "gh",
                    key: "args",
                    needle: "--json"
                },
                &trace
            )
            .passed
        );
        assert!(
            judge(
                Check::ArgNever {
                    tool: "gh",
                    needle: "|"
                },
                &trace
            )
            .passed
        );
    }

    #[test]
    fn asking_twice_for_the_same_thing_is_caught_and_rephrasing_is_not() {
        let repeated = trace(
            vec![
                call("recall", r#"{"query":"roof"}"#),
                call("recall", r#"{"query":"roof"}"#),
            ],
            "",
        );
        let verdict = judge(Check::NoRepeatOf("recall"), &repeated);
        assert!(!verdict.passed);
        assert!(verdict.detail.contains("roof"));

        let rephrased = trace(
            vec![
                call("recall", r#"{"query":"roof"}"#),
                call("recall", r#"{"query":"gutters"}"#),
            ],
            "",
        );
        assert!(judge(Check::NoRepeatOf("recall"), &rephrased).passed);
    }

    #[test]
    fn what_the_model_said_before_a_call_counts_as_having_said_it() {
        // The workspace guidance asks it to say what it is about to write
        // *before* writing, which is prose in the same round as the call.
        let trace = Trace {
            steps: vec![Step {
                asides: vec![super::super::trace::Aside {
                    round: 0,
                    text: "I'll write lists/shopping.md with the six items.".into(),
                }],
                answer: "Done.".into(),
                calls: vec![call("write_file", r#"{"path":"lists/shopping.md"}"#)],
                rounds: 2,
                ..Step::default()
            }],
            broken: None,
        };
        assert!(judge(Check::Says(&["lists/shopping.md"]), &trace).passed);
    }

    #[test]
    fn a_long_answer_is_elided_in_the_detail() {
        let trace = trace(Vec::new(), &"word ".repeat(200));
        let verdict = judge(Check::Says(&["nothing like this"]), &trace);
        assert!(!verdict.passed);
        assert!(verdict.detail.ends_with('…'));
        assert!(verdict.detail.chars().count() <= 160);
    }

    #[test]
    fn a_check_on_one_step_ignores_the_others() {
        let trace = Trace {
            steps: vec![
                Step {
                    calls: vec![call("news", r#"{"topic":"Zig"}"#)],
                    ..Step::default()
                },
                Step {
                    answer: "The Bun one was from Hacker News.".into(),
                    ..Step::default()
                },
            ],
            broken: None,
        };
        // The follow-up should be answered from what is already in context.
        assert!(Check::NoTools.judge(&View::step(&trace, 1)).passed);
        assert!(!Check::NoTools.judge(&View::whole(&trace)).passed);
    }
}
