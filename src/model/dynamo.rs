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
/// Everything here is a decision the model makes *before* a call, which is the
/// only reason it is in the prompt rather than in a tool result — where the
/// rest of this module's guidance lives, and where it is worth more. Reaching
/// for `series` after `usage`, leaving `scale=` alone, and not asking for July
/// are choices no response can come back and correct.
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
     **Which circuit is `usage`; when it ran is `series`.** Anything about a jump, a spike, \
     a habit or where the power went is two calls — `usage` over the period to find the \
     circuit, then `series` on that circuit to find the hours. A single call can name a \
     suspect and cannot say when it ran.\n\n\
     Periods are `today`, `yesterday`, `week`, `month`, `year`, `all`, and there is nothing \
     else: `month` means the last 30 days rather than a calendar month, and there is no way \
     to ask for July, or for a range of dates. A day is a calendar day in the user's own \
     timezone, not the last 24 hours, and every timestamp that comes back is already in \
     their local time — read the clock time as written rather than converting it.\n\n\
     **Leave `scale=` off and let Dynamo pick.** Minute readings only go back about a week, \
     so a long period at `scale=1MIN` quietly answers from that week alone and still reports \
     the result under the period's own name — a month of one circuit comes back four times \
     too small, from a question that looked entirely ordinary.\n\n\
     **Do not add merged and branch figures together.** A 240 V circuit is wired across two \
     branch legs and also appears as one merged channel, so summing both counts every large \
     appliance twice. The default `kind=circuits` already counts each circuit once — leave it \
     alone unless a question is specifically about the legs. A `usage` list does not say \
     which of its rows are merged, so a subtotal for the 240 V circuits has to come from \
     `kind=merged` rather than from picking likely-looking rows out of the default.\n\n\
     **A `usage` list leaves out whatever used nothing.** A circuit missing from it drew no \
     power over that period. That is not the same as the circuit being unnamed, unmonitored \
     or absent, and `channels` is what settles which — check it before telling the user \
     something is not measured.\n\n\
     **Most circuits have never been named**, and appear as the monitor plus a channel \
     number, like `basement (red) ch12`. That is not an appliance and does not tell the user \
     anything — when one of those is the answer, say plainly that the circuit is unnamed, \
     give its channel, and mention they can name it in the Inhab app so it reads properly \
     next time. Never invent a plausible appliance for one.\n\n\
     **A circuit is a breaker, not an appliance.** Several things can share one, so a \
     circuit named for the loudest thing on it still carries whatever else is wired to it — \
     which is why a named circuit rarely falls to zero. Attribute usage to the circuit, not \
     to the appliance, unless the user has said the two are the same, and do not diagnose an \
     appliance from a breaker's total.\n\n\
     Only one of the three monitors has mains CTs, so `kind=main` is that panel's total and \
     not the whole house. If someone asks what the house used, say which it is.\n\n\
     There is no tariff here and no price per kWh. If someone asks what something cost, give \
     them the energy and ask what they pay — do not put a currency figure on it, not even a \
     worked example at an assumed rate, because that is the number they will repeat."
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
///
/// Notes accumulate rather than compete. An early version returned the first
/// branch that matched, so a truncated mains-only total was reported as one
/// problem and never the other — and which one depended on the order the
/// branches happened to be written in.
///
/// A refusal is the exception and returns alone: there is no answer to say
/// anything else about.
pub fn note_for(response: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(response).ok()?;

    if parsed.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
        return match parsed.get("error").and_then(serde_json::Value::as_str)? {
            "ambiguous" => Some(
                "More than one circuit matches that name, so this is a question for the user \
                 rather than a guess. List the candidates and ask which they meant. Do not \
                 call `dynamo` again this turn — pass the exact circuit or its `channel` next \
                 turn.\n\n\
                 Read the candidates before you list them: a 240 V circuit appears three \
                 times, once as the merged channel and once for each of its two branch legs, \
                 all under the same name. The merged one is the whole circuit and a leg is \
                 half of it, so offer the merged channel unless the user asked about the legs."
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

    let mut notes: Vec<String> = Vec::new();

    // An answer longer than the cap loses its tail, and Dynamo sorts its keys
    // alphabetically — so `points` comes before `resolution`, `total_kwh` and
    // `truncated`, and every summary field is on the wrong side of the cut. A
    // plain `series <circuit> week` is 9.3 KB against a cap of 8, which makes
    // this an ordinary question rather than an edge case: the model is handed a
    // severed array of hourly rows, no total, and a generic footer telling it to
    // retry with `limit=N` — an argument Dynamo accepts and silently ignores.
    // Restating the headline here puts it back, because a note is appended after
    // the cut rather than caught by it.
    if response.chars().count() > MAX_OUTPUT {
        let field = |key: &str| match parsed.get(key) {
            Some(serde_json::Value::String(text)) => Some(text.clone()),
            Some(serde_json::Value::Number(number)) => Some(number.to_string()),
            _ => None,
        };
        let mut headline: Vec<String> = Vec::new();
        for (key, label) in [
            ("circuit", "circuit"),
            ("period", "period"),
            ("kind", "kind"),
            ("resolution", "resolution"),
            ("total_kwh", "total kWh"),
            ("matched", "readings matched"),
        ] {
            if let Some(value) = field(key) {
                headline.push(format!("{label} {value}"));
            }
        }
        if !headline.is_empty() {
            notes.push(format!(
                "The rows above were cut to fit and the figures that summarised them went with \
                 the tail, so here they are: {}. Those are whole — it is only the row-by-row \
                 detail that is short. **Ignore the `limit=N` in the line about being cut off**; \
                 Dynamo has no such argument and accepts it without doing anything. Ask for a \
                 coarser `scale=1H` or `scale=1D`, or a shorter period, if you need the rows.",
                headline.join(", ")
            ));
        }
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
    // there. Five rather than three because that is where they actually land:
    // against the real house the top of a day's usage is a named water heater,
    // a named bedroom, and then two channel numbers.
    let unnamed: Vec<String> = parsed
        .get("circuits")
        .and_then(serde_json::Value::as_array)
        .map(|list| {
            list.iter()
                .take(5)
                .filter_map(|c| c.get("circuit").and_then(serde_json::Value::as_str))
                .filter(|name| is_unnamed(name))
                .map(str::to_string)
                .collect()
        })
        .or_else(|| {
            // `series` answers about one circuit, and names it at the top level.
            parsed
                .get("circuit")
                .and_then(serde_json::Value::as_str)
                .filter(|name| is_unnamed(name))
                .map(|name| vec![name.to_string()])
        })
        .unwrap_or_default();
    if !unnamed.is_empty() {
        notes.push(format!(
            "{} of the circuits near the top of this answer have never been named — {}. Those \
             are channel numbers, not appliances: say so plainly, give the channel, and \
             mention they can be named in the Inhab app. Do not guess what is on them.",
            unnamed.len(),
            unnamed.join(", ")
        ));
    }

    // Which slice of the panel this is. Only one of the three monitors has mains
    // CTs and the merged channels are the 240 V circuits alone, so both of these
    // are subtotals that read exactly like a house total.
    match parsed.get("kind").and_then(serde_json::Value::as_str) {
        Some("main") => notes.push(
            "This is the mains CTs, and only one of the three monitors has them — so it is \
             that panel's total and not the whole house. Say which it is."
                .into(),
        ),
        Some("merged") => notes.push(
            "These are the merged 240 V circuits only — the water heater, the dryer, the \
             heat pump. Everything on a single leg is missing, so this total is well under \
             the house. `kind=circuits` is the one that counts each circuit exactly once."
                .into(),
        ),
        _ => {}
    }

    // Minute readings are kept for about a week; hourly and daily go back
    // years. So a long period at `scale=1MIN` answers from as far back as
    // minutes exist and reports *that* as the period's total, under the
    // period's own name. Measured against the real house: a month of the water
    // heater is 568.9 kWh at the resolution Dynamo picks and 135.4 kWh at
    // `scale=1MIN`, both labelled "the last 30 days". Four times out, from a
    // question nobody would call unusual, and the only clue in the response is
    // the first timestamp — inside the array that has just been cut.
    if parsed.get("resolution").and_then(serde_json::Value::as_str) == Some("1MIN") {
        let period = parsed
            .get("period")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if period.contains("30 days") || period.contains("year") || period.contains("all") {
            notes.push(format!(
                "**This total is not for {period}.** Minute readings only go back about a \
                 week, so asking for `scale=1MIN` over a long period silently answers from \
                 the week that has them — while still calling itself {period}. Do not quote \
                 this figure as the period's. Ask again without `scale=`, or with `scale=1H` \
                 or `scale=1D`, and use that instead."
            ));
        }
    }

    if parsed.get("truncated").and_then(serde_json::Value::as_bool) == Some(true) {
        let matched = parsed
            .get("matched")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default();
        notes.push(format!(
            "The rows are a page, not the whole answer — {matched} matched. `total_kwh` is \
             still the total for the whole period, so quote that as it stands; it is the \
             shape over time that is incomplete. Ask for a coarser `scale=` or a shorter \
             period if the missing rows matter."
        ));
    }

    // The resolution is chosen from the period, and a long period is answered
    // from daily totals. Quoting a day's figure as though it were measured
    // minute by minute is the sort of overclaim worth heading off.
    if parsed.get("resolution").and_then(serde_json::Value::as_str) == Some("1D") {
        notes.push(
            "These are daily totals — the period was long enough that Dynamo answered from \
             the daily rolls rather than from minutes. Fine for totals and comparisons; not \
             something to draw conclusions about a particular hour from."
                .into(),
        );
    }

    (!notes.is_empty()).then(|| notes.join("\n\n"))
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
            ("never been named", "27 of 40 circuits are a channel number"),
            ("breaker, not an appliance", "several loads share one CT"),
            ("local time", "timestamps read four hours out otherwise"),
            ("no tariff", "a model will otherwise invent a price"),
            // The four the eval found, each from a run rather than a hunch.
            (
                "when it ran is `series`",
                "an anomaly is two calls and one call cannot say when",
            ),
            (
                "Leave `scale=` off",
                "a month at scale=1MIN answers from a week and says otherwise",
            ),
            (
                "leaves out whatever used nothing",
                "a circuit absent from `usage` was read as a circuit that is not measured",
            ),
            (
                "last 30 days rather than a calendar month",
                "`usage july` is refused and `month` is not July",
            ),
        ] {
            assert!(g.contains(needle), "guidance does not cover {why}:\n{g}");
        }
    }

    #[test]
    fn the_guidance_only_carries_what_a_response_cannot_say_later() {
        // Prompt length is a measured lever in this project, and this paragraph
        // rides on every turn of every conversation with the capability on. The
        // rule for what earns a place: it has to change a decision made *before*
        // the call, because anything else belongs in `note_for`, where it is
        // read at the moment it applies and costs nothing the rest of the time.
        //
        // So these are deliberately absent — each is a response's own business.
        let g = guidance();
        for (absent, lives_in) in [
            ("ambiguous", "note_for, when more than one circuit matches"),
            (
                "collector has stopped",
                "note_for, when `now` comes back empty",
            ),
            ("limit=N", "note_for, when the cap actually cut something"),
            ("daily rolls", "note_for, when the resolution came back 1D"),
        ] {
            assert!(
                !g.contains(absent),
                "{absent:?} is in the prompt and belongs in {lives_in}:\n{g}"
            );
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

    #[test]
    fn an_answer_too_long_for_the_cap_has_its_summary_put_back_after_the_cut() {
        // Measured, not imagined: `dynamo agent series "Water Heater" week`
        // against the real house is 9,355 bytes and the cap is 8,000. Dynamo
        // sorts its keys, so `points` precedes `resolution`, `total_kwh` and
        // `truncated` — the cut lands inside the array and takes every figure
        // with it. Without this the model gets hourly rows, no total, and a
        // footer recommending an argument that does nothing.
        let long = format!(
            r#"{{"ok":true,"circuit":"Water Heater","channel":"415375/99",
                "period":"the last 7 days","resolution":"1H","count":153,"matched":153,
                "total_kwh":119.27,"truncated":false,"points":[{}]}}"#,
            (0..900)
                .map(|n| format!(r#"{{"at":"2026-07-31T00:00:00-04:00","kwh":0.5,"watts":{n}.0}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(
            long.chars().count() > MAX_OUTPUT,
            "the fixture has to be long"
        );

        let note = note_for(&long).expect("a note");
        assert!(note.contains("119.27"), "the total has to survive:\n{note}");
        assert!(note.contains("Water Heater"), "{note}");
        assert!(note.contains("1H"), "{note}");
        // And the footer `framed` appends is wrong for this tool specifically.
        assert!(note.contains("limit=N"), "{note}");
        assert!(note.contains("scale="), "{note}");

        // The note is appended after the cut, so it is what the model actually
        // reads — which is the only reason putting the figures here works.
        let whole = crate::model::tools::framed(&long, MAX_OUTPUT, Some(note));
        assert!(whole.contains("119.27"), "the cut ate the note");
        assert!(whole.contains("cut off"), "the footer is still there too");
    }

    #[test]
    fn an_answer_that_fits_is_not_given_a_summary_it_does_not_need() {
        // The note costs tokens on every call it appears in, so it appears only
        // when something was actually lost.
        assert_eq!(
            note_for(
                r#"{"ok":true,"circuit":"Water Heater","period":"today","resolution":"1H",
                    "total_kwh":4.85,"count":9,"matched":9,"truncated":false,
                    "points":[{"at":"2026-08-01T00:00:00-04:00","kwh":0.787,"watts":787.0}]}"#
            ),
            None
        );
    }

    #[test]
    fn a_truncated_page_does_not_cast_doubt_on_the_period_total() {
        // Measured against the real tool: `series … month scale=1MIN` answers
        // 400 of 9,661 rows and a `total_kwh` for the whole month regardless.
        // The earlier wording — "this is a page, not the whole answer" — sat
        // directly above that figure and invited the model to hedge a number
        // that was never partial.
        let note = note_for(
            r#"{"ok":true,"count":400,"matched":9661,"truncated":true,"total_kwh":511.2}"#,
        )
        .expect("a note");
        assert!(note.contains("9661"), "{note}");
        assert!(note.contains("whole period"), "{note}");
    }

    #[test]
    fn the_ambiguity_note_says_which_of_the_candidates_is_the_whole_circuit() {
        // The real shape of an ambiguous answer, and the reason it is not just
        // "pick one": a 240 V circuit comes back three times under one name,
        // once merged and once per leg. A leg is exactly half, so guessing one
        // is a wrong answer that survives a sanity check.
        let note = note_for(
            r#"{"ok":false,"error":"ambiguous","message":"8 circuits match","candidates":[
                {"circuit":"GeoThermal","channel":"415375/97"},
                {"circuit":"GeoThermal","channel":"415375/3"},
                {"circuit":"GeoThermal","channel":"415375/4"}]}"#,
        )
        .expect("a note");
        assert!(
            note.contains("merged channel is the whole circuit") || note.contains("half of it")
        );
        assert!(note.contains("ask which they meant"), "{note}");
    }

    #[test]
    fn a_subtotal_says_which_slice_of_the_panel_it_is() {
        // Both of these read exactly like a house total. `main` is one monitor
        // of three; `merged` is the 240 V circuits and nothing on a single leg.
        let mains =
            note_for(r#"{"ok":true,"kind":"main","total_kwh":140.33,"count":1}"#).expect("a note");
        assert!(mains.contains("not the whole house"), "{mains}");

        let merged =
            note_for(r#"{"ok":true,"kind":"merged","total_kwh":41.6,"count":6}"#).expect("a note");
        assert!(merged.contains("kind=circuits"), "{merged}");
    }

    #[test]
    fn the_default_slice_gets_no_slice_note_at_all() {
        // `kind=circuits` is the one that counts each circuit once, so there is
        // nothing to warn about — and a note on every answer is a note the
        // model stops reading.
        assert_eq!(
            note_for(
                r#"{"ok":true,"kind":"circuits","total_kwh":140.7,"count":2,"circuits":[
                    {"circuit":"Water Heater","kwh":25.7},{"circuit":"GeoThermal","kwh":12.5}]}"#
            ),
            None
        );
    }

    #[test]
    fn a_series_about_an_unnamed_circuit_is_flagged_the_same_way_a_list_is() {
        // `series` names its circuit at the top level rather than in a
        // `circuits` array, so the branch that catches a channel number in a
        // list walked straight past the answer that is *entirely* about one.
        let note = note_for(
            r#"{"ok":true,"circuit":"basement (blank) ch3","channel":"422818/3",
                "resolution":"1H","total_kwh":22.167,"count":24,"truncated":false,
                "points":[{"at":"2026-07-31T11:00:00-04:00","kwh":1.94,"watts":1940.0}]}"#,
        )
        .expect("a note");
        assert!(note.contains("basement (blank) ch3"), "{note}");
        assert!(note.contains("Do not guess"), "{note}");
    }

    #[test]
    fn an_answer_with_two_things_wrong_with_it_says_both() {
        // A truncated page of a mains-only total. Returning one note and
        // dropping the other meant whichever branch happened to be checked
        // first won, and the model was told half of what it needed.
        let note = note_for(
            r#"{"ok":true,"kind":"main","count":400,"matched":9661,"truncated":true,
                "total_kwh":140.33}"#,
        )
        .expect("a note");
        assert!(note.contains("not the whole house"), "{note}");
        assert!(note.contains("9661"), "{note}");
    }

    #[test]
    fn an_unnamed_circuit_is_caught_further_down_than_the_top_three() {
        // Where they actually land: against the real house a day's usage opens
        // with a named water heater and a named bedroom, and the channel
        // numbers start at position three.
        let note = note_for(
            r#"{"ok":true,"kind":"circuits","unit":"kWh","count":4,"circuits":[
                {"circuit":"Water Heater","kwh":25.693},
                {"circuit":"Hannah's Bedroom","kwh":24.396},
                {"circuit":"basement (blank) ch3","kwh":22.167},
                {"circuit":"basement (red) ch6","kwh":18.53}]}"#,
        )
        .expect("a note");
        assert!(note.contains("basement (blank) ch3"), "{note}");
        assert!(note.contains("basement (red) ch6"), "{note}");
    }
}
