//! Reaching the house's electricity through Dynamo's `dynamo agent` CLI.
//!
//! The same shape as [`super::planner`] and [`super::magpie`] — spawn the
//! sibling, do not link it — but for a different reason. Those two have to be
//! subprocesses because each holds its store in the memory of the running app
//! and a second writer loses. Dynamo's store is Postgres and would tolerate a
//! second reader happily; it is a subprocess because that is the shape this app
//! already gates, caps and frames, and because the alternative is asking a
//! service whose whole design note says it publishes no port to open one.
//!
//! **Every verb reads, and that is structural rather than a list to maintain.**
//! Planner gates an unknown verb because it might mutate. Nothing reachable from
//! `dynamo agent` can: it runs `SELECT`s as a Postgres role granted nothing
//! else, and the writes Dynamo performs come from a collector loop in a
//! container on the NAS that this cannot reach. So a known verb is
//! [`Gate::Never`] and an unknown one is *refused* rather than gated — a verb
//! Dynamo gains later costs a second call, never an unreviewed change.
//!
//! **No `--flags`, ever.** Not for GOption's sake, as with Planner — Dynamo has
//! no launcher to placate — but so that all three sibling CLIs answer to one
//! grammar and a model need not remember which is which.

use super::tools::Gate;

/// What may be done with a `dynamo agent` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Run(Gate),
    Refuse(String),
}

/// Every verb, because every verb reads.
///
/// Taken from `dynamo agent describe`, which is the authority. Unlike Planner's
/// `READS`, this list going stale cannot cost anything worse than a refusal:
/// there is no gated half for a verb to fall into.
const VERBS: &[&str] = &[
    "describe", "help", "channels", "circuits", "now", "current", "live", "usage", "energy",
    "series", "history",
];

/// How much of a response is kept.
///
/// Smaller than Planner's, deliberately. The long answers here are lists of
/// numbers — `series` at minute resolution, `usage` over sixty circuits — and a
/// model does not get better at arithmetic by being handed more rows. Dynamo
/// caps its own output too and says `truncated` when it does; this is the
/// backstop.
pub const MAX_OUTPUT: usize = 8_000;

/// The verb, with a leading `agent` tolerated.
pub fn verb(args: &[String]) -> Option<&str> {
    let mut words = args
        .iter()
        .map(String::as_str)
        .filter(|word| !word.trim().is_empty());
    match words.next() {
        Some("agent") => words.next(),
        other => other,
    }
}

/// Decide what to do with an invocation.
pub fn classify(args: &[String]) -> Decision {
    let Some(verb) = verb(args) else {
        return Decision::Refuse(
            "`dynamo` needs a verb. `channels` lists the circuits, `now` is what they are \
             drawing, `usage <period>` totals energy by circuit, `series <circuit> <period>` \
             is one circuit over time."
                .into(),
        );
    };
    if args.iter().any(|word| word.starts_with("--")) {
        return Decision::Refuse(format!(
            "`{verb}` takes no `--flags`. Use positional words and key=value pairs, like \
             `usage yesterday kind=circuits`."
        ));
    }
    if VERBS.contains(&verb) {
        // Nothing here can change anything, so nothing here asks.
        return Decision::Run(Gate::Never);
    }
    Decision::Refuse(format!(
        "`{verb}` is not a dynamo verb. `describe` lists them; they are all read-only."
    ))
}

/// The argv to spawn, with the fixed prefix put back on.
pub fn command(args: &[String]) -> Vec<String> {
    let mut command = vec!["dynamo".to_string(), "agent".to_string()];
    let rest = match args.first().map(String::as_str) {
        Some("agent") => &args[1..],
        _ => args,
    };
    command.extend(rest.iter().cloned());
    command
}

/// What the system prompt says about Dynamo.
///
/// The double-counting rule leads, because it is the one way to read this data
/// confidently and wrongly, and because the fix is a default the model only has
/// to *not override*.
pub fn guidance() -> String {
    "`dynamo` is the house's own electricity, measured per circuit by three panel monitors \
     and kept minute by minute. Everything it does is read-only. Call it with the arguments \
     after `dynamo agent`: `channels` lists the circuits, `now` is what each is drawing in \
     watts, `usage <period>` totals energy by circuit, `series <circuit> <period>` is one \
     circuit over time.\n\n\
     Periods are `today`, `yesterday`, `week`, `month`, `year`, `all`, and a day means a \
     calendar day rather than the last 24 hours.\n\n\
     **Do not add merged and branch figures together.** A 240 V circuit is wired across two \
     branch legs and also appears as one merged channel, so summing both counts every large \
     appliance twice. The default `kind=circuits` already counts each circuit once — leave it \
     alone unless a question is specifically about the legs.\n\n\
     Only one of the three monitors has mains CTs, so `kind=main` is that panel's total and \
     not the whole house. If someone asks what the house used, say which it is."
        .to_string()
}

/// What the model is told alongside a response, when the response has a shape
/// that is easy to report as the opposite of what it says.
pub fn note_for(response: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(response).ok()?;

    if parsed.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
        return match parsed.get("error").and_then(serde_json::Value::as_str)? {
            "ambiguous" => Some(
                "More than one circuit matches that name, so this is a question for the user \
                 rather than a guess. List the candidates and ask which they meant. Do not \
                 call `dynamo` again this turn — pass the exact circuit or its `channel` next \
                 turn."
                    .into(),
            ),
            "no-such-circuit" => Some(
                "Nothing is called that. Call `channels` and use a name from it — most \
                 circuits on one of the three monitors have never been named and appear as \
                 the monitor plus a channel number."
                    .into(),
            ),
            _ => None,
        };
    }

    // An empty `now` is the case that reads as good news and is not.
    if parsed.get("unit").and_then(serde_json::Value::as_str) == Some("W")
        && parsed.get("count").and_then(serde_json::Value::as_u64) == Some(0)
    {
        return Some(
            "No circuit has reported in the last half hour. That is not a quiet house — it \
             means the collector has stopped or lost its sign-in. Say so rather than \
             reporting zero usage."
                .into(),
        );
    }

    if parsed.get("truncated").and_then(serde_json::Value::as_bool) == Some(true) {
        let matched = parsed
            .get("matched")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default();
        return Some(format!(
            "This is a page, not the whole answer — {matched} rows matched. Say so rather \
             than implying it is everything, and narrow the period if the rest matter."
        ));
    }

    // The resolution is chosen from the period, and a long period is answered
    // from daily totals. Quoting a day's figure as though it were measured
    // minute by minute is the sort of overclaim worth heading off.
    if parsed.get("resolution").and_then(serde_json::Value::as_str) == Some("1D") {
        return Some(
            "These are daily totals — the period was long enough that Dynamo answered from \
             the daily rolls rather than from minutes. Fine for totals and comparisons; not \
             something to draw conclusions about a particular hour from."
                .into(),
        );
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn every_verb_runs_without_asking_because_none_of_them_can_change_anything() {
        for line in [
            "channels",
            "now",
            "usage yesterday",
            "usage month kind=main",
            "series Dryer week",
            "describe",
        ] {
            assert_eq!(
                classify(&args(line)),
                Decision::Run(Gate::Never),
                "{line} only reads"
            );
        }
    }

    #[test]
    fn an_unknown_verb_is_refused_rather_than_gated() {
        // The opposite of Planner, and for a reason: there is no gated half to
        // fall into, so gating an unknown verb would only mean asking the user
        // to approve something that does not exist.
        let Decision::Refuse(why) = classify(&args("delete-everything")) else {
            panic!("an unknown verb should be refused");
        };
        assert!(why.contains("read-only"), "{why}");
        assert!(why.contains("describe"), "{why}");
    }

    #[test]
    fn the_fixed_prefix_is_added_and_not_doubled() {
        assert_eq!(command(&args("channels")), ["dynamo", "agent", "channels"]);
        assert_eq!(
            command(&args("agent channels")),
            ["dynamo", "agent", "channels"]
        );
    }

    #[test]
    fn a_flag_is_refused_with_what_to_write_instead() {
        let Decision::Refuse(why) = classify(&args("usage --period today")) else {
            panic!("a flag should be refused");
        };
        assert!(why.contains("key=value"), "{why}");
    }

    #[test]
    fn no_verb_at_all_says_which_ones_to_start_with() {
        let Decision::Refuse(why) = classify(&[]) else {
            panic!("nothing should be refused");
        };
        assert!(why.contains("channels"), "{why}");
    }

    #[test]
    fn the_guidance_leads_with_the_double_counting_rule() {
        let g = guidance();
        assert!(g.contains("merged"), "{g}");
        assert!(g.contains("twice"), "{g}");
        // And says the whole-house caveat, which is the other confident wrong
        // answer available here.
        assert!(g.contains("not the whole house"), "{g}");
    }

    #[test]
    fn an_ambiguous_circuit_is_a_question_rather_than_a_guess() {
        let note = note_for(
            r#"{"ok":false,"error":"ambiguous","message":"2 circuits match",
                "candidates":[{"circuit":"Dryer"},{"circuit":"Dryer"}]}"#,
        )
        .expect("a note");
        assert!(note.contains("ask which they meant"), "{note}");
        // The clause Planner's eval put there for the same reason: without it
        // the model calls again and writes the call as prose.
        assert!(note.contains("Do not call `dynamo` again"), "{note}");
    }

    #[test]
    fn a_silent_house_is_reported_as_a_broken_collector_rather_than_as_no_usage() {
        // The failure this exists to stop: "nothing is drawing any power right
        // now", said about a service that stopped collecting last Tuesday.
        let note = note_for(r#"{"ok":true,"unit":"W","count":0,"circuits":[]}"#).expect("a note");
        assert!(note.contains("collector has stopped"), "{note}");
    }

    #[test]
    fn a_live_reading_needs_no_note() {
        assert_eq!(
            note_for(
                r#"{"ok":true,"unit":"W","count":2,"resolution":"1MIN",
                    "circuits":[{"circuit":"Dryer","watts":4200.0}]}"#
            ),
            None
        );
    }

    #[test]
    fn daily_rolls_are_labelled_as_such() {
        let note = note_for(r#"{"ok":true,"resolution":"1D","total_kwh":900.0}"#).expect("a note");
        assert!(note.contains("daily totals"), "{note}");
    }

    #[test]
    fn a_truncated_answer_says_how_many_there_really_were() {
        let note =
            note_for(r#"{"ok":true,"count":400,"matched":1440,"truncated":true}"#).expect("a note");
        assert!(note.contains("1440"), "{note}");
    }

    #[test]
    fn a_circuit_that_does_not_exist_points_at_channels() {
        let note =
            note_for(r#"{"ok":false,"error":"no-such-circuit","message":"nope"}"#).expect("a note");
        assert!(note.contains("channels"), "{note}");
    }
}
