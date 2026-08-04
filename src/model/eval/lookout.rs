//! What the proactive check should surface, and — far more often — not.
//!
//! The same shape as [`super::memory`]: one call, not a turn, judged through
//! exactly the vetting the application applies before a notification is ever
//! raised. Scoring the raw reply would grade the model; scoring
//! [`lookout::read`] grades what ships.
//!
//! **Two thirds of these expect silence.** That is not timidity, it is the
//! whole product decision: a notification that fires on an ordinary Tuesday
//! gets muted, and a muted notification is worth less than none, because the
//! one genuinely useful interruption in a month is now invisible too. The
//! failure mode being measured is eagerness.

use chrono::{DateTime, Local, TimeZone};

use super::check::Verdict;
use crate::model::lookout::{self, Outcome, Signals};

/// The moment every case is checked at. Fixed, like the rest of the suite —
/// half of what makes something worth surfacing is what day it is.
pub fn today() -> DateTime<Local> {
    Local
        .with_ymd_and_hms(2026, 8, 3, 8, 30, 0)
        .single()
        .expect("a real local time")
}

/// What a case asserts about the outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// Nothing was surfaced. The commonest correct answer.
    Quiet,
    /// Something was surfaced.
    Speaks,
    /// The headline names this, case-insensitively. Only checked when it spoke.
    Names(&'static str),
    /// The headline does not name this.
    NeverNames(&'static str),
}

impl std::fmt::Display for Expect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Quiet => f.write_str("stays quiet"),
            Self::Speaks => f.write_str("surfaces something"),
            Self::Names(what) => write!(f, "names {what:?}"),
            Self::NeverNames(what) => write!(f, "does not name {what:?}"),
        }
    }
}

impl Expect {
    fn judge(&self, outcome: &Outcome) -> (bool, String) {
        let said = match outcome {
            Outcome::Quiet => String::new(),
            Outcome::Notice { headline, detail } => format!("{headline} — {detail}"),
        };
        match self {
            Self::Quiet => (outcome.is_quiet(), format!("surfaced {said:?}")),
            Self::Speaks => (!outcome.is_quiet(), "stayed quiet".into()),
            Self::Names(what) => (
                said.to_lowercase().contains(&what.to_lowercase()),
                format!("said {said:?}"),
            ),
            Self::NeverNames(what) => (
                !said.to_lowercase().contains(&what.to_lowercase()),
                format!("said {said:?}"),
            ),
        }
    }
}

/// One proactive check, and what it should decide.
#[derive(Debug, Clone)]
pub struct Case {
    pub name: &'static str,
    pub about: &'static str,
    pub signals: Signals,
    pub checks: Vec<Expect>,
}

impl Case {
    pub fn weight(&self) -> usize {
        self.checks.len()
    }

    /// The request, or `None` when the application would not have asked at all.
    ///
    /// A check with nothing in it is not sent, and that is a *result*: the
    /// right behaviour for an empty day is silence, reached without spending a
    /// call on it.
    pub fn request(&self) -> Option<crate::model::wire::ChatRequest> {
        self.signals
            .worth_asking()
            .then(|| lookout::request(&self.signals, today()))
    }

    /// What the application would do with the reply.
    pub fn read(&self, reply: Option<&str>) -> Outcome {
        match reply {
            Some(reply) => lookout::read(reply),
            // Never asked, so nothing was surfaced.
            None => Outcome::Quiet,
        }
    }

    pub fn judge(&self, outcome: &Outcome) -> Vec<Verdict> {
        self.checks
            .iter()
            .map(|check| {
                let (passed, detail) = check.judge(outcome);
                Verdict {
                    check: check.to_string(),
                    passed,
                    detail: if passed { String::new() } else { detail },
                }
            })
            .collect()
    }
}

fn case(name: &'static str, about: &'static str, signals: Signals, checks: Vec<Expect>) -> Case {
    Case {
        name,
        about,
        signals,
        checks,
    }
}

/// Every case, in a stable order.
pub fn all() -> Vec<Case> {
    vec![
        // -- the ordinary day, which is most days -------------------------
        case(
            "lookout/an-ordinary-morning-is-quiet",
            "mail and tasks exist every day; neither is news, and surfacing them is how a \
             notification gets muted",
            Signals {
                tasks: vec![
                    "Put the bins out — no due date".into(),
                    "Draft the quarterly report — due in 9 days".into(),
                ],
                mail: vec![
                    "Kernel Weekly — #412: what shipped this week".into(),
                    "GitHub — [familiar] 2 new notifications".into(),
                ],
                alerts: Vec::new(),
                context: vec!["Familiar: a GTK 4 desktop assistant".into()],
            },
            vec![Expect::Quiet],
        ),
        case(
            "lookout/an-empty-day-is-never-even-asked",
            "nothing to reason about means no call — asking a model to find something in \
             nothing is how a model invents something",
            Signals {
                context: vec!["Familiar: a GTK 4 desktop assistant".into()],
                ..Signals::default()
            },
            vec![Expect::Quiet],
        ),
        case(
            "lookout/a-full-inbox-is-not-an-event",
            "there is always mail; volume is not urgency",
            Signals {
                mail: (1..=9)
                    .map(|n| format!("Sender {n} — a message about something"))
                    .collect(),
                ..Signals::default()
            },
            vec![Expect::Quiet],
        ),
        case(
            "lookout/a-distant-deadline-can-wait",
            "a task due in nine days is not today's problem, and saying so daily for nine days \
             is the definition of nagging",
            Signals {
                tasks: vec!["Renew the lease — due in 9 days".into()],
                ..Signals::default()
            },
            vec![Expect::Quiet],
        ),
        case(
            "lookout/nothing-interesting-is-not-a-reason",
            "an interesting-looking message is not a time-bound one",
            Signals {
                mail: vec![
                    "Andrew Kelley — Zig 0.16 release notes".into(),
                    "A conference — call for papers, closes in three months".into(),
                ],
                ..Signals::default()
            },
            vec![Expect::Quiet],
        ),
        // -- the days that are worth a word -------------------------------
        case(
            "lookout/a-deadline-today-is-worth-a-word",
            "specific, time-bound, and the kind of thing that stops being actionable — the \
             one shape that earns an interruption",
            Signals {
                tasks: vec!["Renew the lease — due today".into()],
                mail: vec!["Kernel Weekly — #412: what shipped this week".into()],
                alerts: Vec::new(),
                context: vec!["Contractors: Vandenberg did the roof".into()],
            },
            vec![Expect::Speaks, Expect::Names("lease")],
        ),
        case(
            "lookout/a-warning-that-changes-the-day",
            "a weather warning is worth saying when it collides with something they are \
             doing, and the collision is the reason rather than the weather",
            Signals {
                tasks: vec!["Meet the roofer on site — today at 16:00".into()],
                mail: Vec::new(),
                // As the application now renders one: the event, when it ends,
                // and the service's own sentence about it. The bare headline
                // this used to be scored 0 of 6, and it was measuring a fixture
                // rather than the model — `Signals.alerts` was filled in by
                // this suite and by nothing in the application at all, so the
                // shape being scored was one the model would never be sent.
                alerts: vec![
                    "Severe Thunderstorm Warning until 21:00 — Damaging winds up to \
                              60 mph and frequent lightning expected. Move indoors and away \
                              from elevated work."
                        .into(),
                ],
                context: vec!["Roof: the north slope was replaced in April".into()],
            },
            vec![Expect::Speaks],
        ),
        case(
            "lookout/an-overdue-thing-that-is-in-the-way",
            "overdue *and* connected to what they are working on now, which is what makes it \
             different from the pile of things that are merely overdue",
            Signals {
                tasks: vec!["Send the signed roofing contract back — overdue by 4 days".into()],
                mail: vec!["Ada Prins — chasing the signed contract, work starts Monday".into()],
                alerts: Vec::new(),
                context: vec!["Contractors: Vandenberg Roofing did the roof".into()],
            },
            vec![Expect::Speaks, Expect::Names("contract")],
        ),
        case(
            "lookout/it-picks-the-one-that-matters",
            "with one real thing among the noise, the notice is about the real thing — a \
             headline naming the newsletter would be worse than silence",
            Signals {
                tasks: vec![
                    "Put the bins out — no due date".into(),
                    "Passport renewal — expires tomorrow".into(),
                ],
                mail: vec![
                    "Kernel Weekly — #412".into(),
                    "LinkedIn — you appeared in 4 searches".into(),
                ],
                alerts: Vec::new(),
                context: vec!["Travel: flying to Amsterdam next month".into()],
            },
            vec![
                Expect::Speaks,
                Expect::Names("passport"),
                Expect::NeverNames("kernel weekly"),
                Expect::NeverNames("linkedin"),
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn most_of_the_suite_expects_silence() {
        // The balance is the product decision, so it is asserted rather than
        // left to whoever adds the next case.
        let cases = all();
        let quiet = cases
            .iter()
            .filter(|case| case.checks.contains(&Expect::Quiet))
            .count();
        assert!(
            quiet * 2 > cases.len(),
            "only {quiet} of {} cases expect silence",
            cases.len()
        );
    }

    #[test]
    fn a_case_with_nothing_in_it_is_never_sent_and_still_scores() {
        let empty = all()
            .into_iter()
            .find(|case| case.name.contains("empty-day"))
            .expect("the empty case");
        assert!(empty.request().is_none());
        // Not asking is the right answer, and it passes rather than skipping.
        let verdicts = empty.judge(&empty.read(None));
        assert!(verdicts.iter().all(|verdict| verdict.passed));
    }

    #[test]
    fn a_vague_notice_fails_a_case_that_wanted_one() {
        // The vetting is in the path: a model that speaks but says nothing is
        // scored as having stayed quiet, which is what it amounts to.
        let deadline = all()
            .into_iter()
            .find(|case| case.name.contains("deadline-today"))
            .expect("the deadline case");
        let outcome = deadline.read(Some("NOTICE: You have a few things today\nWorth a look."));
        assert!(outcome.is_quiet());
        assert!(deadline.judge(&outcome).iter().any(|v| !v.passed));

        let good = deadline.read(Some(
            "NOTICE: The lease renewal is due today\nIt is the only thing today with a \
             deadline attached.",
        ));
        assert!(deadline.judge(&good).iter().all(|v| v.passed));
    }

    #[test]
    fn every_case_names_a_family_and_carries_checks() {
        for case in all() {
            assert!(case.name.starts_with("lookout/"), "{}", case.name);
            assert!(case.weight() > 0, "{}", case.name);
            assert!(!case.about.is_empty(), "{}", case.name);
        }
    }
}
