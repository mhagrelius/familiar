//! Reaching Planner's task list through its `planner agent` CLI.
//!
//! The same shape as [`super::github`], for the same reason: Planner is already
//! installed and already holds the user's tasks, so the useful thing is not a
//! second implementation of its store but permission to run the one binary.
//!
//! **A subprocess rather than a crate, and this one is not a preference.**
//! Planner keeps its whole store in the memory of the running application and
//! flushes it on a two-second tick. A second process writing that file is
//! overwritten, silently, within two seconds. `planner agent` rides Planner's
//! own command line: the app sets `HANDLES_COMMAND_LINE`, so when it is running
//! the invocation is forwarded over D-Bus and the *running* instance mutates its
//! own store and redraws. When it is not running, the invoked process becomes
//! the primary instance and does the work. Either way there is exactly one
//! writer.
//!
//! **The verb decides the gate.** `list` reads and is ungated, the same as
//! `read_file`. `add` and `complete` change the user's task list and stop at the
//! approval dialog with their exact argv on screen. An unknown verb is gated,
//! for the same reason an unknown tool is — and because the list here can only
//! ever go stale in the safe direction: a verb Planner gains that this does not
//! know about costs an approval click, never an unreviewed write.
//!
//! **No `--flags`, ever.** GOption parses the command line before Planner's own
//! code runs and rejects any option it was not told about in advance, so a
//! `--limit 10` is refused by the launcher rather than by the verb, with a
//! message about the wrong thing. Arguments are positional words and
//! `key=value` pairs.

use super::tools::Gate;

/// What may be done with a `planner agent` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Run(Gate),
    Refuse(String),
}

/// Verbs that only read, with their aliases. Ungated, like `read_file`.
///
/// Taken from `planner agent describe`, whose `mutates` field is the authority.
/// Anything absent is gated, so this being out of date costs an approval rather
/// than an unreviewed change to someone's tasks.
const READS: &[&str] = &[
    "help", "describe", "overview", "projects", "list", "tasks", "show", "task", "search", "find",
];

/// How much of a response is kept. A task list is small; a runaway `list` on a
/// vault of thousands is not, and the context window is shared with the
/// conversation.
pub const MAX_OUTPUT: usize = 12_000;

/// The verb, with a leading `agent` tolerated.
///
/// The tool asks for the arguments *after* `planner agent`, because the prefix
/// is fixed and every character of it a model has to reproduce is a character
/// it can get wrong. A model that writes it anyway is not wrong enough to fail.
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
            "`planner` needs a verb. `overview` lists the projects and labels that exist, \
             `list` takes a filter query, `add` takes a quick-add line."
                .into(),
        );
    };
    if args.iter().any(|word| word.starts_with("--")) {
        return Decision::Refuse(format!(
            "`{verb}` takes no `--flags` — Planner's launcher rejects an option it was not \
             told about before the verb ever runs. Use positional words and key=value pairs, \
             like `list 'due: today' limit=10`."
        ));
    }
    if READS.contains(&verb) {
        return Decision::Run(Gate::Never);
    }
    Decision::Run(Gate::Always)
}

/// The argv to spawn, with the fixed prefix put back on.
pub fn command(args: &[String]) -> Vec<String> {
    let mut command = vec!["planner".to_string(), "agent".to_string()];
    let rest = match args.first().map(String::as_str) {
        Some("agent") => &args[1..],
        _ => args,
    };
    command.extend(rest.iter().cloned());
    command
}

/// What the system prompt says about Planner.
///
/// Short on purpose. Everything that depends on what a call *returned* — the
/// repeating task, the ambiguous title, the project that did not exist — is in
/// [`note_for`], attached to the response that raises it, where it is read at
/// the moment it applies instead of on every unrelated turn.
pub fn guidance() -> String {
    "`planner` is the user's own task list. Call it with the arguments after `planner \
     agent`: `overview`, `list <query>`, `show <task>`, `search <text>` read it; `add`, \
     `subtask`, `complete`, `reopen`, `update` and `delete` change it and will ask them \
     first.\n\n\
     Start with `overview` before adding anything. It is the only call that says which \
     projects and labels exist, and a `#Project` that does not exist is not created — the \
     task lands in the Inbox instead.\n\n\
     Two small languages, both the app's own. A task is created from a quick-add line, \
     where the tokens are stripped out of the title: `#Project` `/Section` `@label` \
     `p1`–`p4` `!30m`, dates like `tomorrow`, `next friday`, `27th`, `9am`, and repeats \
     like `every other monday`. A list is filtered with a query: `due: today`, `overdue`, \
     `no date`, `p1`, `#Work`, `@errand`, joined by `&`, `|` and `!`.\n\n\
     Every task comes back with an id. Pass the id you were given rather than the title \
     again — a title matching two open tasks is an error, not a guess. There are no \
     `--flags`: arguments are positional words and `key=value` pairs."
        .to_string()
}

/// What the model is told alongside a response, when the response has a shape
/// that is easy to report as the opposite of what it says.
///
/// These live here rather than in the system prompt on purpose: a rule attached
/// to the result that triggers it is read exactly when it applies, and costs
/// nothing on every other turn.
pub fn note_for(response: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(response).ok()?;

    if parsed.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
        return match parsed.get("error").and_then(serde_json::Value::as_str)? {
            "ambiguous" => Some(
                "More than one open task matches, so this is a question for the user rather \
                 than a guess. Stop here: list the candidates by title and ask which they \
                 meant. Do not call `planner` again this turn — you cannot resolve it \
                 without them, and the id to pass is the one they give you next turn."
                    .into(),
            ),
            "bad-query" => Some(
                "That filter query did not parse. The terms are things like `due: today`, \
                 `overdue`, `#Project`, `@label`, `p1`, `no date`, combined with `&`, `|`, \
                 `!` and parentheses."
                    .into(),
            ),
            _ => None,
        };
    }

    // A repeating task that was ticked off is open again on a later date. Both
    // halves are true and reporting either alone tells the user the opposite of
    // what happened.
    if parsed.get("outcome").and_then(serde_json::Value::as_str) == Some("completed-and-repeats") {
        let next = parsed
            .get("next_due")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("a later date");
        return Some(format!(
            "That task repeats, so completing it succeeded *and* it is back on {next}. Say \
             both. Calling it done leaves the user thinking it is finished; calling it a \
             reschedule tells them the opposite of what they asked for. Give the date as \
             {next} — copy it, do not work it out. Naming the weekday is fine, but the number \
             is the one above and not one you calculate from today."
        ));
    }

    if parsed.get("truncated").and_then(serde_json::Value::as_bool) == Some(true) {
        let matched = parsed
            .get("matched")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default();
        return Some(format!(
            "This is a page, not the whole list — {matched} tasks matched. Say so rather \
             than implying it is everything, and pass `limit=N` if the rest genuinely matter."
        ));
    }

    // A `#Project` that does not exist is not created; the task lands in the
    // Inbox. That is where a misspelling shows up, and only the response knows.
    if parsed.get("action").and_then(serde_json::Value::as_str) == Some("added") {
        let project = parsed
            .get("task")
            .and_then(|task| task.get("project"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if project.eq_ignore_ascii_case("inbox") {
            return Some(
                "It went to the Inbox, which is where a task goes when its `#Project` does \
                 not exist — Planner does not create one. If a project was meant, check the \
                 name with `overview` and move it with `update`."
                    .into(),
            );
        }
        return Some(
            "The date and project above are what Planner actually parsed out of the line. \
             Report those rather than restating what you asked for."
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
    fn reading_is_ungated_and_changing_is_not() {
        for line in ["overview", "list due: today", "show 12", "search lease"] {
            assert_eq!(
                classify(&args(line)),
                Decision::Run(Gate::Never),
                "{line} should read freely"
            );
        }
        for line in [
            "add Ring the plumber",
            "complete 12",
            "delete 12",
            "update 12 due=friday",
            "remove-project Home",
        ] {
            assert_eq!(
                classify(&args(line)),
                Decision::Run(Gate::Always),
                "{line} changes the task list"
            );
        }
    }

    #[test]
    fn an_unknown_verb_is_gated_rather_than_run() {
        // The list can only go stale in the safe direction: a verb Planner
        // gains costs an approval click, never an unreviewed write.
        assert_eq!(
            classify(&args("archive-everything")),
            Decision::Run(Gate::Always)
        );
    }

    #[test]
    fn the_fixed_prefix_is_added_and_not_doubled() {
        assert_eq!(command(&args("list")), ["planner", "agent", "list"]);
        assert_eq!(command(&args("agent list")), ["planner", "agent", "list"]);
    }

    #[test]
    fn a_flag_is_refused_with_what_to_write_instead() {
        // GOption rejects it before the verb runs, so the error the model would
        // otherwise see is about the launcher and says nothing useful.
        let Decision::Refuse(why) = classify(&args("list --limit 10")) else {
            panic!("a flag should be refused");
        };
        assert!(why.contains("key=value"), "{why}");
        assert!(why.contains("limit=10"), "{why}");
    }

    #[test]
    fn no_verb_at_all_says_which_ones_to_start_with() {
        let Decision::Refuse(why) = classify(&[]) else {
            panic!("nothing should be refused");
        };
        assert!(why.contains("overview"), "{why}");
    }

    #[test]
    fn a_shell_metacharacter_is_an_argument_and_not_an_operator() {
        // There is no shell. The filter query's own or-operator is a literal
        // `|` argument, which is exactly why this has to keep working.
        let built = command(&args("list due: today | overdue"));
        assert_eq!(
            built,
            ["planner", "agent", "list", "due:", "today", "|", "overdue"]
        );
        assert_eq!(
            classify(&args("list due: today | overdue")),
            Decision::Run(Gate::Never)
        );
    }

    #[test]
    fn completing_a_repeating_task_is_reported_as_both_things() {
        // The failure this exists to stop: "I moved that to Thursday", which is
        // the opposite of what the user asked for.
        let note = note_for(
            r#"{"ok":true,"action":"completed","outcome":"completed-and-repeats",
                "next_due":"2026-08-06","task":{"id":4,"title":"Bins"}}"#,
        )
        .expect("a note");
        assert!(note.contains("2026-08-06"), "{note}");
        assert!(note.contains("Say"), "{note}");
    }

    #[test]
    fn an_ambiguous_reference_is_a_question_rather_than_a_guess() {
        let note = note_for(
            r#"{"ok":false,"error":"ambiguous","message":"more than one",
                "candidates":[{"id":1},{"id":2}]}"#,
        )
        .expect("a note");
        assert!(note.contains("ask which they meant"), "{note}");
        // The clause that matters most, and the one the eval put there: an
        // earlier version ended "…or pass the id of the one they pick", and the
        // model read that as licence to call again — then wrote the call as
        // prose, which the app strips, which left the turn with no answer at
        // all in five runs out of six.
        assert!(note.contains("Do not call `planner` again"), "{note}");
    }

    #[test]
    fn a_task_that_landed_in_the_inbox_says_the_project_did_not_exist() {
        let note = note_for(
            r#"{"ok":true,"action":"added","task":{"id":9,"title":"Ring","project":"Inbox"}}"#,
        )
        .expect("a note");
        assert!(note.contains("does not exist"), "{note}");
        assert!(note.contains("overview"), "{note}");
    }

    #[test]
    fn a_task_that_landed_somewhere_real_says_to_read_the_response_back() {
        let note = note_for(
            r#"{"ok":true,"action":"added","task":{"id":9,"title":"Ring","project":"Home"}}"#,
        )
        .expect("a note");
        assert!(note.contains("actually parsed"), "{note}");
    }

    #[test]
    fn a_truncated_list_says_how_many_there_really_were() {
        let note = note_for(r#"{"ok":true,"tasks":[],"count":50,"matched":137,"truncated":true}"#)
            .expect("a note");
        assert!(note.contains("137"), "{note}");
    }

    #[test]
    fn an_ordinary_response_needs_no_note() {
        assert_eq!(
            note_for(r#"{"ok":true,"tasks":[],"count":3,"matched":3,"truncated":false}"#),
            None
        );
        // Not JSON at all — `help` prints text on purpose.
        assert_eq!(note_for("PLANNER AGENT\n\nVerbs:"), None);
    }
}
