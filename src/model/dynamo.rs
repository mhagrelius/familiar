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
     watts, `usage <period>` totals energy by circuit in kWh, `series <circuit> <period>` is \
     one circuit over time.\n\n\
     Periods are `today`, `yesterday`, `week`, `month`, `year`, `all`. A day is a calendar \
     day in the user's own timezone, not the last 24 hours, and every timestamp that comes \
     back is already in their local time — read the clock time as written rather than \
     converting it.\n\n\
     **Do not add merged and branch figures together.** A 240 V circuit is wired across two \
     branch legs and also appears as one merged channel, so summing both counts every large \
     appliance twice. The default `kind=circuits` already counts each circuit once — leave it \
     alone unless a question is specifically about the legs.\n\n\
     **Most circuits have never been named**, and appear as the monitor plus a channel \
     number, like `basement (red) ch12`. That is not an appliance and does not tell the user \
     anything — when one of those is the answer, say plainly that the circuit is unnamed, \
     give its channel, and mention they can name it in the Inhab app so it reads properly \
     next time. Never invent a plausible appliance for one.\n\n\
     **A circuit is a breaker, not an appliance.** Several things can share one, so a \
     circuit named for the loudest thing on it still carries whatever else is wired to it — \
     which is why a named circuit rarely falls to zero. Attribute usage to the circuit, not \
     to the appliance, unless the user has said the two are the same.\n\n\
     Only one of the three monitors has mains CTs, so `kind=main` is that panel's total and \
     not the whole house. If someone asks what the house used, say which it is.\n\n\
     There is no tariff here and no price per kWh. If someone asks what something cost, ask \
     what they pay or say you would be guessing — do not put a currency figure on it."
        .to_string()
}

/// Whether a circuit label is Dynamo's fallback rather than a name somebody
/// chose.
///
/// The fallback is `<monitor> ch<number>` — `basement (red) ch12` — built when
/// `channel.name` is null, which is true of most of one monitor. Matching the
/// shape rather than asking the tool keeps this a property of the string, so a
/// circuit a person happens to have named "Shed ch2" is a false positive that
/// costs one unnecessary sentence, which is the right direction to be wrong in.
fn is_unnamed(label: &str) -> bool {
    label.rsplit_once(" ch").is_some_and(|(before, n)| {
        !before.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
    })
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

    // An unnamed circuit at the top of an answer is the most likely thing to be
    // dressed up as an appliance, and only the response knows whether one is
    // there. Checked before truncation because it changes what the answer says
    // rather than how complete it is.
    let unnamed: Vec<String> = parsed
        .get("circuits")
        .and_then(serde_json::Value::as_array)
        .map(|list| {
            list.iter()
                .take(3)
                .filter_map(|c| c.get("circuit").and_then(serde_json::Value::as_str))
                .filter(|name| is_unnamed(name))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if !unnamed.is_empty() {
        return Some(format!(
            "{} of the circuits near the top of this answer have never been named — {}. Those \
             are channel numbers, not appliances: say so plainly, give the channel, and \
             mention they can be named in the Inhab app. Do not guess what is on them.",
            unnamed.len(),
            unnamed.join(", ")
        ));
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
    fn the_guidance_covers_every_way_this_data_reads_wrong() {
        let g = guidance();
        // Each of these is a way to be confidently wrong about a real house,
        // and each was found by looking at what actually comes back rather
        // than by imagining what might.
        for (needle, why) in [
            ("twice", "merged and branch legs added together"),
            (
                "not the whole house",
                "one monitor has mains CTs, three exist",
            ),
            ("never been named", "32 of 40 circuits are a channel number"),
            ("breaker, not an appliance", "several loads share one CT"),
            ("local time", "timestamps read four hours out otherwise"),
            ("no tariff", "a model will otherwise invent a price"),
        ] {
            assert!(g.contains(needle), "guidance does not cover {why}:\n{g}");
        }
    }

    #[test]
    fn the_guidance_names_both_units() {
        // `now` is watts and `usage` is kWh. A model that mixes them reports a
        // kettle as using 2 kWh at this instant.
        let g = guidance();
        assert!(g.contains("watts"), "{g}");
        assert!(g.contains("kWh"), "{g}");
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
    fn an_unnamed_circuit_at_the_top_is_flagged_rather_than_dressed_up() {
        // The real shape of an answer here: the biggest consumer is a channel
        // number nobody has labelled. A model that reports "basement (red)
        // ch12 is your biggest draw" has told the user nothing, and one that
        // guesses "that'll be your dryer" has told them something false.
        let note = note_for(
            r#"{"ok":true,"unit":"W","count":2,"circuits":[
                {"circuit":"basement (red) ch12","watts":1254.0},
                {"circuit":"Water Heater","watts":812.0}]}"#,
        )
        .expect("a note");
        assert!(note.contains("basement (red) ch12"), "{note}");
        assert!(note.contains("Inhab app"), "{note}");
        assert!(note.contains("Do not guess"), "{note}");
    }

    #[test]
    fn an_answer_of_named_circuits_is_left_alone() {
        assert_eq!(
            note_for(
                r#"{"ok":true,"unit":"W","count":2,"circuits":[
                    {"circuit":"Water Heater","watts":812.0},
                    {"circuit":"Clothes Dryer","watts":40.0}]}"#
            ),
            None
        );
    }

    #[test]
    fn the_unnamed_shape_is_recognised_without_asking_the_tool() {
        assert!(is_unnamed("basement (red) ch12"));
        assert!(is_unnamed("basement (blank) ch3"));
        assert!(!is_unnamed("Water Heater"));
        assert!(!is_unnamed("Basement East"));
        // No number after `ch`, so not the fallback shape.
        assert!(!is_unnamed("Church"));
        assert!(!is_unnamed("ch4"));
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
