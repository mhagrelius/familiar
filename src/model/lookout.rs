//! Noticing something before being asked.
//!
//! The heartbeat in [`super::heartbeat`] wakes a thread with a prompt somebody
//! wrote. This is the other half: a run with no prompt at all, which gathers
//! what can be gathered cheaply — what is due, what has arrived, what the
//! weather is doing, what was being worked on — and decides whether any of it
//! is worth interrupting a person for.
//!
//! **Almost always it is not, and that is the design.** An assistant that
//! surfaces something every time it looks is a notification that gets turned
//! off in a week, and then it surfaces nothing for ever. So the instruction
//! below spends most of its length on reasons to stay quiet, the reply format
//! makes silence the shortest thing to write, and [`read`] throws away anything
//! vague enough to have been produced without looking. Half the eval family
//! scores staying quiet.
//!
//! One call, not a turn: no tools, no agentic loop. The signals are collected
//! by the application before anything is asked, because a proactive check that
//! could run tools is a proactive check that can spend money and change files
//! while nobody is watching.

use chrono::{DateTime, Local};

use super::instructions::THINK_OFF;
use super::wire::{ChatRequest, Message};

/// How long a headline may be. It has to fit a notification, and a headline
/// that needs two lines is a paragraph pretending.
pub const MAX_HEADLINE: usize = 90;

/// What could be found out without asking anybody.
///
/// Every field is optional and most runs have two or three. Nothing here costs
/// a network round trip that the application was not making anyway.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Signals {
    /// Tasks due today or overdue, as Planner reports them.
    pub tasks: Vec<String>,
    /// Unread mail, subject and sender only — never the body. A proactive
    /// check reads headers; it does not read somebody's post to them.
    pub mail: Vec<String>,
    /// Active weather warnings. Not the forecast: "it will be 22°C" is not a
    /// reason to interrupt anyone.
    pub alerts: Vec<String>,
    /// What the user has been working on lately, from their own notes.
    pub context: Vec<String>,
}

impl Signals {
    /// Whether there is anything at all to reason about.
    ///
    /// An empty check is not sent. Asking a model to find something in nothing
    /// is how a model invents something.
    pub fn worth_asking(&self) -> bool {
        !self.tasks.is_empty() || !self.mail.is_empty() || !self.alerts.is_empty()
    }

    fn render(&self) -> String {
        let mut out = String::new();
        let mut section = |title: &str, lines: &[String]| {
            if lines.is_empty() {
                return;
            }
            out.push_str(title);
            out.push('\n');
            for line in lines {
                out.push_str("- ");
                out.push_str(line);
                out.push('\n');
            }
            out.push('\n');
        };
        section("Tasks due or overdue:", &self.tasks);
        section("Unread mail (subjects only):", &self.mail);
        section("Active weather warnings:", &self.alerts);
        section("What they have been working on:", &self.context);
        out.trim_end().to_string()
    }
}

/// What the model is asked to do, and mostly not to.
pub const INSTRUCTIONS: &str = "\
You are looking over someone's day, to decide whether one thing in it is worth a short \
notification right now. They did not ask, and nobody is necessarily at the keyboard.

Speak when there is something that is **due today, happening today, already overdue, or about \
to stop being possible** — and that goes wrong if they do nothing about it today. A warning or \
an event that collides with something they have planned counts: the clash is the reason.

Otherwise answer QUIET. Things that are NOT a reason to speak, however many of them there are: \
mail existing; a lot of mail existing; tasks existing; something looking interesting; a \
deadline still comfortably far off; not having said anything for a while; wanting to be \
helpful.

Name the single most important thing. One notice, never a digest.

Three worked examples, which are examples of the *form* — do not reuse their words.

Signals:
- Tasks: \"Reply to Sam about lunch — due no due date\"
- Mail: \"A newsletter — issue 12\"
Answer:
QUIET

Signals:
- Tasks: \"Car MOT — expires today\"
- Mail: \"A newsletter — issue 12\"
Answer:
NOTICE: The car's MOT expires today
After midnight it is not legal to drive, and booking a retest takes days.

Signals:
- Tasks: \"Drive to the airport — today at 05:00\"
- Warnings: \"Freezing Rain Advisory until 08:00\"
Answer:
NOTICE: Freezing rain is forecast through the airport run at 05:00
The advisory covers the whole drive, so it is worth leaving earlier than planned.

Answer in exactly one of those two forms. To stay quiet the whole reply is the word QUIET. To \
speak, the first line is NOTICE: followed by one line under 90 characters naming the specific \
thing, then one or two sentences saying why it matters today. No questions — they cannot \
reply. No other text and no markdown.";

/// The request the application sends. Low temperature: this is a judgement
/// against a rubric, not a piece of writing.
pub fn request(signals: &Signals, now: DateTime<Local>) -> ChatRequest {
    let asked = format!(
        "It is {}.\n\n{}",
        now.format("%A %-d %B, %H:%M"),
        signals.render()
    );
    ChatRequest {
        messages: vec![
            // Thinking off, like every other one-shot call here. Without it the
            // reasoning eats the token budget and the reply comes back empty —
            // which the harness reports as a dead server, and which cost forty
            // minutes of retries at 45 seconds apiece before it was spotted.
            // The rubric above is explicit; deliberating about it costs more
            // than the answer.
            Message::system(format!("{THINK_OFF}\n{INSTRUCTIONS}")),
            Message::user(asked),
        ],
        temperature: Some(0.2),
        max_tokens: Some(300),
        ..ChatRequest::new(Vec::new())
    }
}

/// What came back, once it has been vetted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing worth saying — the ordinary answer.
    Quiet,
    Notice {
        headline: String,
        detail: String,
    },
}

impl Outcome {
    pub fn is_quiet(&self) -> bool {
        matches!(self, Self::Quiet)
    }
}

/// Phrases that mean the model found nothing and said so in prose.
///
/// A small model asked for one of two words will sometimes write a sentence
/// instead. "Nothing urgent today" is a QUIET, and treating it as a malformed
/// reply — or worse, as a notice — would be reading it backwards.
const QUIET_ENOUGH: &[&str] = &[
    "quiet",
    "nothing worth",
    "nothing urgent",
    "nothing to report",
    "nothing that needs",
    "no notice",
    "nothing pressing",
];

/// Words that make a headline empty however specific it looks.
///
/// These are what a model writes when it has decided to say something before
/// working out what. A notice has to name a thing.
const VAGUE: &[&str] = &[
    "you have some",
    "a few things",
    "several items",
    "some tasks",
    "some emails",
    "some mail",
    "your inbox",
    "keep an eye",
    "might want to",
    "you may want",
    "just checking",
    "checking in",
    "don't forget to stay",
    "have a productive",
];

/// Read the reply, and throw away anything that is not a real notice.
///
/// The vetting is the point. A model that has been told to be quiet will
/// nevertheless produce "You have a few things to look at today" often enough
/// that letting it through would train the user to dismiss the notification
/// without reading — after which a genuine one is invisible too.
pub fn read(reply: &str) -> Outcome {
    let trimmed = reply.trim();
    if trimmed.is_empty() {
        return Outcome::Quiet;
    }

    let Some(at) = trimmed.to_uppercase().find("NOTICE:") else {
        return Outcome::Quiet;
    };
    // Anything before the marker is preamble the format asked for none of.
    let body = &trimmed[at + "NOTICE:".len()..];
    let (headline, detail) = body.split_once('\n').unwrap_or((body, ""));
    let headline = headline.trim().trim_matches(['*', '#', '"']).trim();
    let detail = detail.trim().to_string();

    if headline.is_empty() || headline.chars().count() > MAX_HEADLINE {
        return Outcome::Quiet;
    }
    let lowered = headline.to_lowercase();
    if QUIET_ENOUGH.iter().any(|phrase| lowered.contains(phrase)) {
        return Outcome::Quiet;
    }
    if VAGUE.iter().any(|phrase| lowered.contains(phrase)) {
        return Outcome::Quiet;
    }
    // A notice with no detail is a headline somebody could not justify.
    if detail.is_empty() {
        return Outcome::Quiet;
    }
    // And it cannot ask a question: there is nobody there to answer one.
    if detail.contains('?') {
        return Outcome::Quiet;
    }

    Outcome::Notice {
        headline: headline.to_string(),
        detail,
    }
}

/// Planner's task JSON, reduced to the lines a check reasons about.
///
/// Titles and due dates, nothing else. The whole object would be four hundred
/// tokens of ids and project names for a judgement that turns on two words.
pub fn tasks_in(listed: &str) -> Vec<String> {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(listed) else {
        return Vec::new();
    };
    parsed
        .get("tasks")
        .and_then(serde_json::Value::as_array)
        .map(|tasks| {
            tasks
                .iter()
                .take(12)
                .filter_map(|task| {
                    let title = task.get("title")?.as_str()?;
                    let due = task
                        .get("due")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("no due date");
                    Some(format!("{title} — due {due}"))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Subjects and senders out of a mail listing. **Never the body**: a proactive
/// check reads headers, and reading somebody's post to them unasked is a
/// different thing from noticing that it arrived.
pub fn subjects_in(listing: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut lines = listing.lines();
    while let Some(line) = lines.next() {
        let Some(rest) = line.trim().strip_prefix('[') else {
            continue;
        };
        let Some((_, after)) = rest.split_once("] ") else {
            continue;
        };
        // `… — sender` on the header line, then the subject on the next.
        let sender = after.rsplit(" — ").next().unwrap_or(after).trim();
        let Some(subject) = lines.next().map(str::trim) else {
            break;
        };
        found.push(format!("{sender} — {subject}"));
        if found.len() >= 12 {
            break;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn signals() -> Signals {
        Signals {
            tasks: vec!["Renew the lease — due today".into()],
            mail: vec!["Ada Prins — Invoice 8871, payment due Friday".into()],
            alerts: Vec::new(),
            context: vec!["Familiar: a GTK 4 desktop assistant".into()],
        }
    }

    #[test]
    fn a_check_with_nothing_in_it_is_never_sent() {
        // Asking a model to find something in nothing is how a model invents
        // something.
        assert!(!Signals::default().worth_asking());
        assert!(!Signals {
            context: vec!["they write Rust".into()],
            ..Signals::default()
        }
        .worth_asking());
        assert!(signals().worth_asking());
    }

    #[test]
    fn quiet_is_the_answer_in_every_shape_a_model_writes_it() {
        for reply in [
            "QUIET",
            "quiet",
            "  QUIET  ",
            "Nothing worth reporting today.",
            "Nothing urgent.",
            "",
        ] {
            assert!(read(reply).is_quiet(), "{reply:?} was not read as quiet");
        }
    }

    #[test]
    fn a_real_notice_survives() {
        let outcome = read(
            "NOTICE: Invoice 8871 is due Friday and the lease renewal is today\n\
             Both land this week and the lease is the one with no slack in it.",
        );
        let Outcome::Notice { headline, detail } = outcome else {
            panic!("a specific, dated notice should survive: {outcome:?}");
        };
        assert!(headline.starts_with("Invoice 8871"), "{headline}");
        assert!(detail.contains("no slack"), "{detail}");
    }

    #[test]
    fn a_notice_that_names_nothing_is_thrown_away() {
        // The failure this whole module is built around. A notification that
        // says "you have a few things to look at" teaches the user to dismiss
        // notifications, after which a real one is invisible too.
        for reply in [
            "NOTICE: You have a few things to look at today\nWorth a glance.",
            "NOTICE: Some tasks are due\nYou might want to check them.",
            "NOTICE: Just checking in\nHave a productive day.",
            "NOTICE: Your inbox has unread mail\nThere are three messages.",
        ] {
            assert!(read(reply).is_quiet(), "{reply:?} got through");
        }
    }

    #[test]
    fn a_headline_too_long_to_be_a_headline_is_thrown_away() {
        let long = format!("NOTICE: {}\nBecause.", "a".repeat(MAX_HEADLINE + 1));
        assert!(read(&long).is_quiet());
    }

    #[test]
    fn a_notice_with_no_reason_behind_it_is_thrown_away() {
        assert!(read("NOTICE: The lease renewal is due today").is_quiet());
    }

    #[test]
    fn a_notice_cannot_ask_a_question() {
        // Nobody is at the keyboard. A notification that asks something is a
        // notification that will never be answered.
        assert!(read(
            "NOTICE: The lease renewal is due today\nShall I draft the email to the landlord?"
        )
        .is_quiet());
    }

    #[test]
    fn preamble_before_the_marker_is_ignored_rather_than_breaking_the_parse() {
        let outcome = read(
            "Here is my assessment.\n\nNOTICE: Storm warning until 9pm tonight\n\
             The roofers are due at four and the warning covers that window.",
        );
        assert!(matches!(outcome, Outcome::Notice { .. }), "{outcome:?}");
    }

    #[test]
    fn planner_json_becomes_the_two_words_a_check_turns_on() {
        let listed = r#"{"ok":true,"action":"list","tasks":[
            {"id":15,"title":"Renew the lease","project":"Admin","priority":"p1","due":"2026-08-03"},
            {"id":12,"title":"Put the bins out","project":"Home","priority":"p3"}],"count":2}"#;
        assert_eq!(
            tasks_in(listed),
            [
                "Renew the lease — due 2026-08-03",
                "Put the bins out — due no due date"
            ]
        );
        assert!(tasks_in("not json").is_empty());
    }

    #[test]
    fn a_mail_listing_gives_up_its_senders_and_subjects_and_nothing_else() {
        // Headers only. Reading somebody's post to them unasked is a different
        // thing from noticing that it arrived.
        let listing = "2 of 2 message(s) in INBOX:\n\n\
            [102] UNREAD Mon, 3 Aug 2026 — Ada Prins <billing@prins.example>\n  \
            Invoice 8871 — payment due Friday\n  \
            The balance of 13,850 is due on Friday. Bank details are on the invoice.\n";
        let found = subjects_in(listing);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("Ada Prins"), "{found:?}");
        assert!(found[0].contains("Invoice 8871"), "{found:?}");
        assert!(
            !found[0].contains("Bank details"),
            "the body leaked into the signal: {found:?}"
        );
    }

    #[test]
    fn the_request_carries_the_signals_and_the_time() {
        let now = Local
            .with_ymd_and_hms(2026, 8, 3, 8, 30, 0)
            .single()
            .expect("a time");
        let request = request(&signals(), now);
        let asked = format!("{:?}", request.messages);
        assert!(asked.contains("Monday 3 August"), "{asked}");
        assert!(asked.contains("Invoice 8871"), "{asked}");
        assert!(asked.contains("Tasks due or overdue"), "{asked}");
        // No tools, ever: a proactive check that could act is one that spends
        // money and changes files while nobody is watching.
        assert!(request.tools.is_empty());
    }

    #[test]
    fn the_instructions_say_when_to_speak_before_they_say_when_not_to() {
        // The order was measured. An earlier draft led with the bar and ruled
        // out the one case that should qualify: it said to stay quiet about
        // anything "already obvious", and a deadline in somebody's own task
        // list is obvious by that reading — so the model answered QUIET to a
        // lease due today, four times out of four.
        let speak = INSTRUCTIONS
            .find("Speak when there is something")
            .expect("the permission");
        let bar = INSTRUCTIONS
            .find("Otherwise answer QUIET")
            .expect("the bar");
        assert!(speak < bar, "the bar comes before the permission");
        assert!(INSTRUCTIONS.contains("goes wrong if they do nothing about it today"));
        assert!(INSTRUCTIONS.contains("a lot of mail existing"));
    }

    #[test]
    fn no_worked_example_matches_a_case_the_suite_asks_about() {
        // The examples are what actually moved this, and they have to be
        // examples of the *form*. With one that matched a scenario, the model
        // reproduced it word for word and the case passed for no reason at all.
        assert!(INSTRUCTIONS.contains("do not reuse their words"));
        for planted in [
            "lease",
            "passport",
            "roofer",
            "thunderstorm",
            "Kernel Weekly",
        ] {
            assert!(
                !INSTRUCTIONS
                    .to_lowercase()
                    .contains(&planted.to_lowercase()),
                "a worked example uses {planted:?}, which a scenario also uses"
            );
        }
    }
}
