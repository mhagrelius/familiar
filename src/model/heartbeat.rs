//! A thread that wakes up on a schedule.
//!
//! The vocabulary is OpenClaw's, and so is the distinction that makes this
//! small: an *automation* is an exact-time job that runs in a fresh session and
//! reports through a notification, while a *heartbeat* wakes an **existing**
//! conversation with its whole context intact. This is the second. So a
//! schedule is a property of a [`Thread`](super::thread::Thread), not a job
//! system beside one — the standing prompt is submitted as an ordinary turn,
//! down the ordinary pipeline, and lands in the thread you can open and read.
//!
//! **The schedule model is hand-rolled and deliberately small.** Not because a
//! cron parser is hard to add — `cron` and `croner` are both good — but because
//! cron is the wrong shape for the question. Neither ChatGPT's tasks nor Claude
//! Code's scheduler exposes cron as its primary model; both ship a preset list,
//! because "weekdays at 7am" is what people actually want. And the hard part is
//! not parsing `0 7 * * 1-5`, it is [`Schedule::due`]: given the last run, the
//! time now, and a laptop that was asleep in between, should this fire? No cron
//! crate answers that, so a dependency would buy the easy half.
//!
//! **A missed run is coalesced, and then it is the job's call.** Only the most
//! recent missed occurrence is ever a candidate ([`Schedule::latest_due`]), so
//! a fortnight away produces one run and not fourteen — that is what makes
//! recovery safe to offer at all, and it is true whatever policy a job picks.
//! What is left is staleness, which differs per job: a 07:00 briefing at 14:00
//! is worse than no briefing, but a power cut overnight should not cost you the
//! briefing at 08:40, and "did the backup finish" is worth knowing whenever you
//! next sit down. [`Recovery`] is that choice. Claude Code Desktop and OpenClaw
//! both skip unconditionally; systemd's `Persistent=` and anacron both
//! coalesce, and this is the second shape.
//!
//! Everything here is a pure function over `chrono`, so it is tested with no
//! display, no timer and no clock — the times are passed in.

use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, TimeZone, Weekday};
use serde::{Deserialize, Serialize};

/// How late a run may be and still count as merely late.
///
/// The tick is every minute and a busy turn can defer a run, so a few minutes
/// of slack is ordinary. Beyond this the machine was asleep or the app was
/// shut, and whether the moment has passed is [`Recovery`]'s question rather
/// than a constant's — this is the answer for [`Recovery::OnTime`] only.
pub const GRACE: Duration = Duration::minutes(20);

/// When a thread wakes.
///
/// Mirrors the preset list Claude Code Desktop offers, which is the same list
/// anyone reaches for. Anything more exotic is a reason to revisit this, not a
/// reason to accept a cron string nobody can read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "every", rename_all = "lowercase")]
pub enum Schedule {
    /// Every `hours` hours, on the hour it first ran.
    Hours { hours: u32 },
    Daily {
        #[serde(with = "clock")]
        at: NaiveTime,
    },
    /// Monday to Friday.
    Weekdays {
        #[serde(with = "clock")]
        at: NaiveTime,
    },
    Weekly {
        day: Weekday,
        #[serde(with = "clock")]
        at: NaiveTime,
    },
}

/// Why a run is happening, which the prompt is allowed to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// On time.
    Regular,
    /// Within the grace window but not on the minute — the app was busy, or
    /// the tick was late.
    Late,
    /// The occurrence passed while nothing was running — the machine was off,
    /// asleep, or the app was not up — and the job's [`Recovery`] says it is
    /// still worth doing. Only ever the *most recent* missed occurrence, so a
    /// fortnight away produces one run rather than fourteen.
    Recovered,
}

/// How late a missed occurrence may be and still be worth running.
///
/// A power cut overnight should not cost you the morning briefing; a fortnight
/// away should not produce a fortnight of them. Those are two different
/// questions and only the first one is a policy — the second is answered by
/// [`Schedule::latest_due`] considering *only* the most recent missed
/// occurrence, so coalescing happens whatever is chosen here.
///
/// What is left is per job: how stale is too stale. A briefing is worthless by
/// the evening; "did the backup finish" is worth knowing whenever you next sit
/// down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Recovery {
    /// Only if it is essentially on time — [`GRACE`], and no more. What every
    /// schedule did before this existed, and still the right answer for
    /// anything whose whole value is the hour it arrives at.
    #[default]
    OnTime,
    /// Later the same calendar day still counts. The briefing missed because
    /// the power was out at 07:00 is worth having at 08:40; tomorrow's is not
    /// worth having as well, and this is what stops it.
    ///
    /// Literally the same date rather than a window of hours, because that is
    /// what the label promises and a scheduling rule people cannot predict is
    /// worse than one that is occasionally generous. Turning the machine on at
    /// 22:00 does deliver the morning's run — the preamble tells the model it
    /// is a recovery and what time it actually is.
    SameDay,
    /// Whenever the machine is next on. For work that does not go stale.
    Whenever,
}

impl Recovery {
    /// Whether an occurrence at `scheduled` is still worth running at `now`.
    ///
    /// `now` is assumed to be at or after `scheduled`; the caller has already
    /// established the occurrence has passed.
    pub fn allows(&self, scheduled: DateTime<Local>, now: DateTime<Local>) -> bool {
        match self {
            Self::OnTime => now - scheduled <= GRACE,
            // Calendar-anchored rather than a duration, because "the same day"
            // is what people mean and a 23:50 job would otherwise recover into
            // the following morning under any fixed window wide enough to be
            // useful at 07:00.
            Self::SameDay => now.date_naive() == scheduled.date_naive(),
            Self::Whenever => true,
        }
    }

    /// How this reads in the editor.
    pub fn describe(&self) -> &'static str {
        match self {
            Self::OnTime => "Only if on time",
            Self::SameDay => "Later the same day",
            Self::Whenever => "Whenever the computer is next on",
        }
    }
}

impl Schedule {
    /// The next time this fires strictly after `after`.
    ///
    /// Local time throughout, because "7am" means the user's 7am. The awkward
    /// cases are the daylight-saving ones and they are handled where they
    /// arise, in [`at_local`].
    pub fn next_after(&self, after: DateTime<Local>) -> DateTime<Local> {
        match self {
            Self::Hours { hours } => {
                let hours = (*hours).clamp(1, 24) as i64;
                after + Duration::hours(hours)
            }
            Self::Daily { at } => {
                let today = at_local(after.date_naive(), *at);
                if today > after {
                    today
                } else {
                    at_local(after.date_naive() + Duration::days(1), *at)
                }
            }
            Self::Weekdays { at } => {
                let mut day = after.date_naive();
                loop {
                    let candidate = at_local(day, *at);
                    if candidate > after && is_weekday(day.weekday()) {
                        return candidate;
                    }
                    day += Duration::days(1);
                }
            }
            Self::Weekly { day: wanted, at } => {
                let mut day = after.date_naive();
                loop {
                    let candidate = at_local(day, *at);
                    if candidate > after && day.weekday() == *wanted {
                        return candidate;
                    }
                    day += Duration::days(1);
                }
            }
        }
    }

    /// The most recent occurrence at or before `now` that `last` has not
    /// already covered.
    ///
    /// This is the coalescing step, and it is what keeps recovery safe: however
    /// many firings were missed, only the newest is ever a candidate, so a
    /// fortnight away collapses to one run rather than fourteen. `None` when
    /// nothing has come round since `last`.
    pub fn latest_due(
        &self,
        last: DateTime<Local>,
        now: DateTime<Local>,
    ) -> Option<DateTime<Local>> {
        let candidate = match self {
            // Relative to the last run rather than to a wall-clock grid —
            // that is what `next_after` means for this variant — so the most
            // recent occurrence is the largest whole number of steps that
            // still fits before `now`.
            Self::Hours { hours } => {
                let hours = (*hours).clamp(1, 24) as i64;
                let steps = (now - last).num_seconds() / (hours * 3600);
                if steps < 1 {
                    return None;
                }
                last + Duration::hours(steps * hours)
            }
            Self::Daily { at } => {
                let today = at_local(now.date_naive(), *at);
                if today <= now {
                    today
                } else {
                    at_local(now.date_naive() - Duration::days(1), *at)
                }
            }
            Self::Weekdays { at } => back_from(now, *at, is_weekday)?,
            Self::Weekly { day: wanted, at } => back_from(now, *at, |day| day == *wanted)?,
        };
        (candidate > last).then_some(candidate)
    }

    /// Whether to run now, why, and which occurrence it is for.
    ///
    /// `last` is when this thread last woke — `None` if it never has. Find the
    /// most recent occurrence that has passed and has not already run, then ask
    /// `recovery` whether it is still worth doing.
    ///
    /// Measuring from the *most recent* missed occurrence rather than the first
    /// one is what makes recovery possible at all. `next_after(last)` is the
    /// oldest thing missed, so after a fortnight away it is a fortnight stale
    /// and every policy discards it — which read as "a gap is skipped" but was
    /// really "lateness was measured against the wrong occurrence".
    ///
    /// The occurrence comes back with the verdict because the caller needs it
    /// for [`preamble`], and computing it a second time is how the two came to
    /// disagree.
    pub fn due(
        &self,
        last: Option<DateTime<Local>>,
        now: DateTime<Local>,
        recovery: Recovery,
    ) -> Option<(Due, DateTime<Local>)> {
        // Never run. The first occurrence is scheduled from *now*, so setting
        // up a 7am daily at 9am does not immediately fire for a 7am that was
        // already gone.
        let last = last?;
        let scheduled = self.latest_due(last, now)?;
        if !recovery.allows(scheduled, now) {
            return None;
        }
        let behind = now - scheduled;
        let why = if behind < Duration::minutes(2) {
            Due::Regular
        } else if behind <= GRACE {
            Due::Late
        } else {
            Due::Recovered
        };
        Some((why, scheduled))
    }

    /// The first run after a schedule is set, so `due` has somewhere to start.
    ///
    /// Returned rather than assumed: a thread that has never woken has no last
    /// run, and treating "never" as the epoch would fire every occurrence since
    /// 1970 in one tick.
    pub fn first_after(&self, set_up: DateTime<Local>) -> DateTime<Local> {
        self.next_after(set_up)
    }

    /// How this reads in a sentence, for the sidebar and the thread header.
    pub fn describe(&self) -> String {
        match self {
            Self::Hours { hours: 1 } => "Every hour".to_string(),
            Self::Hours { hours } => format!("Every {hours} hours"),
            Self::Daily { at } => format!("Daily at {}", clock_face(*at)),
            Self::Weekdays { at } => format!("Weekdays at {}", clock_face(*at)),
            Self::Weekly { day, at } => format!("{day:?}s at {}", clock_face(*at)),
        }
    }
}

/// A schedule from the words the model wrote.
///
/// The preset list is short enough that a grammar would be more machinery than
/// it deserves: what has to be understood is "daily at 07:00", "weekdays at
/// 8am", "every 4 hours" and "Mondays at 9", which is a shape and a time. A
/// phrasing that is not one of those is refused rather than guessed at — a
/// briefing that silently lands on the wrong day is worse than one that was not
/// set up, because the user believes in it.
pub fn parse(when: &str) -> Option<Schedule> {
    let text = when.trim().to_lowercase();
    if text.is_empty() {
        return None;
    }

    // "every 4 hours", "every hour", "hourly".
    if text.starts_with("hourly") {
        return Some(Schedule::Hours { hours: 1 });
    }
    if let Some(rest) = text.strip_prefix("every ") {
        let rest = rest.trim();
        if rest.starts_with("hour") {
            return Some(Schedule::Hours { hours: 1 });
        }
        if let Some(count) = rest.split_whitespace().next() {
            if let Ok(hours) = count.parse::<u32>() {
                if rest.contains("hour") {
                    return Some(Schedule::Hours {
                        hours: hours.clamp(1, 24),
                    });
                }
            }
        }
        // "every weekday at 7am", "every Monday at 9" fall through to the day
        // matching below, which is what they mean.
    }

    let at = clock_in(&text)?;
    if text.contains("weekday") {
        return Some(Schedule::Weekdays { at });
    }
    for (name, day) in [
        ("monday", Weekday::Mon),
        ("tuesday", Weekday::Tue),
        ("wednesday", Weekday::Wed),
        ("thursday", Weekday::Thu),
        ("friday", Weekday::Fri),
        ("saturday", Weekday::Sat),
        ("sunday", Weekday::Sun),
    ] {
        if text.contains(name) {
            return Some(Schedule::Weekly { day, at });
        }
    }
    if text.contains("daily") || text.contains("every day") || text.contains("each day") {
        return Some(Schedule::Daily { at });
    }
    // A bare time is a daily one. "at 7am" with no other qualifier is the
    // commonest thing anybody asks for and refusing it would be pedantry.
    Some(Schedule::Daily { at })
}

/// The first clock time in a phrase: `07:00`, `7am`, `7 am`, `19:30`.
fn clock_in(text: &str) -> Option<NaiveTime> {
    let cleaned = text.replace("a.m.", "am").replace("p.m.", "pm");
    for token in cleaned.split(|c: char| c.is_whitespace() || c == ',') {
        let token = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != ':');
        if token.is_empty() {
            continue;
        }
        let (digits, suffix) = match token.find(['a', 'p']) {
            Some(at) => (&token[..at], &token[at..]),
            None => (token, ""),
        };
        let (hour, minute) = match digits.split_once(':') {
            Some((hour, minute)) => (hour.parse::<u32>().ok()?, minute.parse::<u32>().ok()?),
            None => match digits.parse::<u32>() {
                Ok(hour) => (hour, 0),
                Err(_) => continue,
            },
        };
        // A bare number with no colon and no am/pm is only a time if it could
        // be one — "every 4 hours" already returned above, but "briefing 2026"
        // must not become 20:26.
        if suffix.is_empty() && !digits.contains(':') && hour > 23 {
            continue;
        }
        let hour = match suffix {
            s if s.starts_with('p') && hour < 12 => hour + 12,
            s if s.starts_with('a') && hour == 12 => 0,
            _ => hour,
        };
        if hour > 23 || minute > 59 {
            continue;
        }
        return NaiveTime::from_hms_opt(hour, minute, 0);
    }
    None
}

/// What the model is told about scheduling a thread.
///
/// The sentence that matters is the first one, and it exists because of a real
/// exchange: asked to set up a morning briefing, the assistant created a
/// *Planner task* — a reminder for the user to ask for the briefing — and then
/// said plainly that it had "no scheduling capability that auto-triggers" and
/// "no cron or background scheduler I can tap into". All three claims were
/// wrong. The capability had simply never been offered to it, so it reached for
/// the nearest thing in the menu and then explained its absence.
pub fn guidance() -> String {
    "`schedule` makes *this conversation* wake up and run on its own. It is not a reminder \
     for the user and not a task in a task list: the standing prompt is submitted as an \
     ordinary turn at the time you set, with the tools this conversation has, and the answer \
     lands here where they can read it back. That is what somebody means by \"a morning \
     briefing\" or \"check this every Monday\".\n\n\
     `schedule set` takes `when` and `prompt`. `when` is one of: `daily at 07:00`, \
     `weekdays at 08:30`, `Mondays at 09:00`, or `every 4 hours`. Write the standing prompt \
     as the instruction you want to be given each time, not as a description of it — it \
     arrives as if the user had typed it. `schedule show` says what this conversation is set \
     to do; `schedule clear` stops it.\n\n\
     Reach for it when the user asks for something *recurring that you would do*. A one-off \
     nudge for the user to do something themselves is a task, not this. A missed run is \
     skipped rather than delivered stale, and the user can pause or remove any of them."
        .to_string()
}

pub fn preamble(due: Due, scheduled_for: DateTime<Local>) -> String {
    let when = scheduled_for.format("%A %-d %B at %H:%M");
    match due {
        Due::Late => format!(
            "This is a scheduled run for {when}, running a little late. Nobody is necessarily \
             at the keyboard. Do the work and report it plainly; if anything you found is \
             time-sensitive, say what time it is true as of."
        ),
        // Said here rather than in the system prompt because this is the only
        // place the model finds out, and getting it wrong is loud: a briefing
        // recovered at 08:40 that opens with "good morning" as though it were
        // 07:00, or reports last night's forecast as today's, reads as the
        // assistant not knowing what time it is.
        Due::Recovered => format!(
            "This is a scheduled run for {when}, recovered late — the computer was off or \
             asleep when it came round, and this is the first chance to do it. Only this one \
             is being run; any earlier occurrences were let go. Nobody is necessarily at the \
             keyboard. Do the work against the time it is *now*, not the time it was due: \
             greet the hour it actually is, refetch anything that moves, and say what each \
             time-sensitive thing is true as of."
        ),
        _ => format!(
            "This is a scheduled run for {when}. Nobody is necessarily at the keyboard, so do \
             the work and report it plainly rather than asking a follow-up question. If there \
             is nothing worth reporting, say so in one line."
        ),
    }
}

fn is_weekday(day: Weekday) -> bool {
    !matches!(day, Weekday::Sat | Weekday::Sun)
}

/// The newest occurrence of `at` on a matching day, at or before `now`.
///
/// Eight days rather than seven: when today matches but its time of day is
/// still ahead of `now`, the answer is the same weekday a week ago, and a
/// seven-step walk stops one day short of it.
fn back_from(
    now: DateTime<Local>,
    at: NaiveTime,
    matches: impl Fn(Weekday) -> bool,
) -> Option<DateTime<Local>> {
    let mut day = now.date_naive();
    for _ in 0..8 {
        let candidate = at_local(day, at);
        if candidate <= now && matches(day.weekday()) {
            return Some(candidate);
        }
        day -= Duration::days(1);
    }
    None
}

fn clock_face(at: NaiveTime) -> String {
    at.format("%H:%M").to_string()
}

/// A local date and time, through the two daylight-saving cases.
///
/// Spring forward makes 02:30 nonexistent and autumn back makes it happen
/// twice. Both are real and both arrive once a year on a schedule set months
/// earlier, so they are decided here rather than left to `unwrap`: a
/// nonexistent time runs at the first valid instant after the gap, an ambiguous
/// one takes the earlier of the two.
fn at_local(day: chrono::NaiveDate, at: NaiveTime) -> DateTime<Local> {
    match Local.from_local_datetime(&day.and_time(at)) {
        chrono::LocalResult::Single(when) => when,
        chrono::LocalResult::Ambiguous(earlier, _) => earlier,
        chrono::LocalResult::None => {
            // The clock skipped over this time. Walk forward a minute at a
            // time to the first instant that exists — at most an hour, and in
            // practice one or two steps.
            let mut minute = at;
            for _ in 0..120 {
                minute += Duration::minutes(1);
                // Past midnight is the next day's problem, not this one's.
                if minute < at {
                    break;
                }
                if let chrono::LocalResult::Single(when) =
                    Local.from_local_datetime(&day.and_time(minute))
                {
                    return when;
                }
            }
            // Nothing in that hour exists, which should be impossible; put it
            // at the start of the next day rather than panicking on a clock.
            Local
                .from_local_datetime(&(day + Duration::days(1)).and_hms_opt(0, 0, 0).unwrap())
                .earliest()
                .unwrap_or_else(Local::now)
        }
    }
}

/// `NaiveTime` as `HH:MM`, so a thread file stays readable.
mod clock {
    use super::NaiveTime;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(at: &NaiveTime, to: S) -> Result<S::Ok, S::Error> {
        to.serialize_str(&at.format("%H:%M").to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(from: D) -> Result<NaiveTime, D::Error> {
        let text = String::deserialize(from)?;
        NaiveTime::parse_from_str(&text, "%H:%M")
            .or_else(|_| NaiveTime::parse_from_str(&text, "%H:%M:%S"))
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod schedule_words {
    use super::*;

    #[test]
    fn the_four_shapes_are_understood_however_the_time_is_written() {
        assert_eq!(
            parse("daily at 07:00"),
            Some(Schedule::Daily {
                at: NaiveTime::from_hms_opt(7, 0, 0).unwrap()
            })
        );
        assert_eq!(
            parse("weekdays at 8am"),
            Some(Schedule::Weekdays {
                at: NaiveTime::from_hms_opt(8, 0, 0).unwrap()
            })
        );
        assert_eq!(
            parse("Mondays at 9:30"),
            Some(Schedule::Weekly {
                day: Weekday::Mon,
                at: NaiveTime::from_hms_opt(9, 30, 0).unwrap()
            })
        );
        assert_eq!(parse("every 4 hours"), Some(Schedule::Hours { hours: 4 }));
        assert_eq!(parse("hourly"), Some(Schedule::Hours { hours: 1 }));
    }

    #[test]
    fn afternoon_times_are_the_afternoon() {
        // "7pm" becoming 07:00 is the kind of wrong that is only noticed twelve
        // hours later, by which time the user has stopped believing in it.
        assert_eq!(
            parse("daily at 7pm"),
            Some(Schedule::Daily {
                at: NaiveTime::from_hms_opt(19, 0, 0).unwrap()
            })
        );
        assert_eq!(
            parse("daily at 12am"),
            Some(Schedule::Daily {
                at: NaiveTime::from_hms_opt(0, 0, 0).unwrap()
            })
        );
        assert_eq!(
            parse("every day at 19:30"),
            Some(Schedule::Daily {
                at: NaiveTime::from_hms_opt(19, 30, 0).unwrap()
            })
        );
    }

    #[test]
    fn a_bare_time_is_a_daily_one() {
        // The commonest thing anybody asks for, and refusing it would be
        // pedantry rather than safety.
        assert_eq!(
            parse("at 7am"),
            Some(Schedule::Daily {
                at: NaiveTime::from_hms_opt(7, 0, 0).unwrap()
            })
        );
    }

    #[test]
    fn a_phrase_with_no_time_in_it_is_refused_rather_than_guessed_at() {
        // A briefing that silently lands at midnight is worse than one that was
        // never set up, because the user believes in it.
        assert_eq!(parse("sometimes"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("when the roof is finished"), None);
    }

    #[test]
    fn the_guidance_says_what_it_is_not() {
        // The whole reason this exists: the assistant made a Planner task for a
        // morning briefing and then said it had no scheduler at all.
        let note = guidance();
        assert!(note.contains("not a reminder"), "{note}");
        assert!(note.contains("not a task in a task list"), "{note}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(text: &str) -> DateTime<Local> {
        // A fixed instant in the machine's zone, which for these tests only has
        // to be self-consistent.
        Local
            .from_local_datetime(
                &chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").expect("a time"),
            )
            .earliest()
            .expect("a local time")
    }

    fn at(hour: u32, minute: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(hour, minute, 0).expect("a time")
    }

    #[test]
    fn daily_fires_at_the_next_occurrence_of_the_time() {
        let daily = Schedule::Daily { at: at(7, 0) };
        // Before today's, it is today's.
        assert_eq!(
            daily.next_after(local("2026-08-03 06:00")),
            local("2026-08-03 07:00")
        );
        // After today's, it is tomorrow's.
        assert_eq!(
            daily.next_after(local("2026-08-03 07:30")),
            local("2026-08-04 07:00")
        );
        // Exactly on it, it is tomorrow's — strictly after, or a run would
        // immediately be due again.
        assert_eq!(
            daily.next_after(local("2026-08-03 07:00")),
            local("2026-08-04 07:00")
        );
    }

    #[test]
    fn weekdays_skips_the_weekend() {
        let weekdays = Schedule::Weekdays { at: at(7, 0) };
        // 2026-08-07 is a Friday.
        assert_eq!(
            weekdays.next_after(local("2026-08-07 08:00")),
            local("2026-08-10 07:00"),
            "Friday evening should go to Monday"
        );
        assert_eq!(
            weekdays.next_after(local("2026-08-08 12:00")),
            local("2026-08-10 07:00"),
            "Saturday should go to Monday"
        );
    }

    #[test]
    fn weekly_lands_on_its_day() {
        let weekly = Schedule::Weekly {
            day: Weekday::Mon,
            at: at(9, 0),
        };
        let next = weekly.next_after(local("2026-08-05 12:00"));
        assert_eq!(next.weekday(), Weekday::Mon);
        assert_eq!(next, local("2026-08-10 09:00"));
    }

    #[test]
    fn hourly_counts_from_the_last_run() {
        let every = Schedule::Hours { hours: 4 };
        assert_eq!(
            every.next_after(local("2026-08-03 06:00")),
            local("2026-08-03 10:00")
        );
    }

    #[test]
    fn a_nonsense_interval_is_clamped_rather_than_looping_forever() {
        // `Hours { hours: 0 }` would make every tick due, which is a busy loop
        // against the GPU.
        let broken = Schedule::Hours { hours: 0 };
        let next = broken.next_after(local("2026-08-03 06:00"));
        assert!(next > local("2026-08-03 06:00"), "{next}");
    }

    #[test]
    fn a_schedule_that_has_never_run_does_not_fire_immediately() {
        // Setting up a 07:00 daily at 09:00 must not instantly fire for the
        // 07:00 that has already gone.
        let daily = Schedule::Daily { at: at(7, 0) };
        assert_eq!(
            daily.due(None, local("2026-08-03 09:00"), Recovery::OnTime),
            None
        );
    }

    #[test]
    fn a_run_fires_once_the_moment_has_passed() {
        let daily = Schedule::Daily { at: at(7, 0) };
        let last = local("2026-08-02 07:00");
        assert_eq!(
            daily.due(Some(last), local("2026-08-03 06:59"), Recovery::OnTime),
            None
        );
        assert_eq!(
            daily.due(Some(last), local("2026-08-03 07:00"), Recovery::OnTime),
            Some((Due::Regular, local("2026-08-03 07:00")))
        );
    }

    #[test]
    fn a_run_a_few_minutes_late_still_happens_and_says_so() {
        // The tick is every minute and a streaming turn defers a run, so a few
        // minutes of slack is ordinary rather than exceptional.
        let daily = Schedule::Daily { at: at(7, 0) };
        let last = local("2026-08-02 07:00");
        assert_eq!(
            daily.due(Some(last), local("2026-08-03 07:10"), Recovery::OnTime),
            Some((Due::Late, local("2026-08-03 07:00")))
        );
    }

    #[test]
    fn a_run_the_machine_slept_through_is_skipped_when_the_job_says_on_time() {
        // A 07:00 briefing delivered at 14:00 is worse than none if that is
        // what the job asked for: the weather is stale, the pull requests have
        // moved, and nobody asked for it now.
        let daily = Schedule::Daily { at: at(7, 0) };
        let last = local("2026-08-02 07:00");
        assert_eq!(
            daily.due(Some(last), local("2026-08-03 14:00"), Recovery::OnTime),
            None
        );
    }

    #[test]
    fn the_power_cut_case_recovers_the_same_morning() {
        // The case this was built for: the machine was off at 07:00 and comes
        // back at 08:40. That is not a missed briefing, it is a late one.
        let daily = Schedule::Daily { at: at(7, 0) };
        let last = local("2026-08-02 07:00");
        assert_eq!(
            daily.due(Some(last), local("2026-08-03 08:40"), Recovery::SameDay),
            Some((Due::Recovered, local("2026-08-03 07:00")))
        );
        // The next day is a different day, and one briefing per day is the
        // whole point.
        assert_eq!(
            daily.due(Some(last), local("2026-08-04 08:40"), Recovery::SameDay),
            Some((Due::Recovered, local("2026-08-04 07:00"))),
            "should recover *that* day's, not the one it missed"
        );
    }

    #[test]
    fn a_fortnight_away_produces_one_run_and_not_fourteen() {
        // Coalescing, which is what makes recovery safe to offer at all. The
        // most recent occurrence is the only candidate however many were
        // missed — so even the most permissive policy fires once.
        let daily = Schedule::Daily { at: at(7, 0) };
        let last = local("2026-07-20 07:00");
        let back = local("2026-08-03 09:00");
        assert_eq!(
            daily.due(Some(last), back, Recovery::Whenever),
            Some((Due::Recovered, local("2026-08-03 07:00"))),
            "the candidate is this morning's, not the fourteen before it"
        );
        // And once that run is recorded, nothing else is owed until tomorrow.
        assert_eq!(daily.due(Some(back), back, Recovery::Whenever), None);
    }

    #[test]
    fn lateness_is_measured_from_the_newest_missed_occurrence() {
        // The defect this replaced: `next_after(last)` is the *oldest* thing
        // missed, so after a gap every policy discarded it as impossibly
        // overdue and recovery could not be built on top.
        let daily = Schedule::Daily { at: at(7, 0) };
        let last = local("2026-07-20 07:00");
        assert_eq!(
            daily.latest_due(last, local("2026-08-03 09:00")),
            Some(local("2026-08-03 07:00"))
        );
        assert_eq!(
            daily.next_after(last),
            local("2026-07-21 07:00"),
            "the old measure, kept for scheduling forwards"
        );
    }

    #[test]
    fn the_grace_window_is_the_boundary_for_on_time() {
        let daily = Schedule::Daily { at: at(7, 0) };
        let last = local("2026-08-02 07:00");
        let due_at = local("2026-08-03 07:00");
        let inside = due_at + GRACE - Duration::minutes(1);
        let outside = due_at + GRACE + Duration::minutes(1);
        assert_eq!(
            daily.due(Some(last), inside, Recovery::OnTime),
            Some((Due::Late, due_at))
        );
        assert_eq!(daily.due(Some(last), outside, Recovery::OnTime), None);
        // The same moment is a recovery rather than nothing, for a job that
        // asked for one.
        assert_eq!(
            daily.due(Some(last), outside, Recovery::SameDay),
            Some((Due::Recovered, due_at))
        );
    }

    #[test]
    fn latest_due_agrees_with_walking_next_after_forwards() {
        // `next_after` defines the occurrence grid, so the newest occurrence
        // at or before now must be the last step of a walk along it. Checked
        // rather than assumed, because the two are computed separately.
        for schedule in [
            Schedule::Hours { hours: 4 },
            Schedule::Daily { at: at(7, 0) },
            Schedule::Weekdays { at: at(8, 15) },
            Schedule::Weekly {
                day: Weekday::Mon,
                at: at(9, 0),
            },
        ] {
            let last = local("2026-07-20 05:00");
            let now = local("2026-08-03 09:00");
            let mut walked = None;
            let mut cursor = last;
            loop {
                let next = schedule.next_after(cursor);
                if next > now {
                    break;
                }
                walked = Some(next);
                cursor = next;
            }
            assert_eq!(
                schedule.latest_due(last, now),
                walked,
                "{schedule:?} disagreed with the walk"
            );
        }
    }

    #[test]
    fn weekly_recovery_does_not_reach_back_past_its_own_day() {
        // A Monday 09:00 job, asked on the Wednesday. The most recent
        // occurrence is Monday — not "today at nine", which never happens.
        let weekly = Schedule::Weekly {
            day: Weekday::Mon,
            at: at(9, 0),
        };
        let last = local("2026-07-27 09:00");
        assert_eq!(
            weekly.latest_due(last, local("2026-08-05 12:00")),
            Some(local("2026-08-03 09:00")),
            "2026-08-03 is a Monday"
        );
    }

    #[test]
    fn weekdays_recovery_skips_back_over_the_weekend() {
        let weekdays = Schedule::Weekdays { at: at(7, 0) };
        // 2026-08-09 is a Sunday; the newest weekday occurrence is Friday's.
        let last = local("2026-08-06 07:00");
        assert_eq!(
            weekdays.latest_due(last, local("2026-08-09 11:00")),
            Some(local("2026-08-07 07:00")),
            "2026-08-07 is a Friday"
        );
    }

    #[test]
    fn a_recovered_run_tells_the_model_it_is_catching_up() {
        // Without this a briefing recovered at 08:40 opens with "good morning"
        // as though it were 07:00 and reports last night's forecast as today's.
        let note = preamble(Due::Recovered, local("2026-08-03 07:00"));
        assert!(note.contains("recovered late"), "{note}");
        assert!(note.contains("off or asleep"), "{note}");
        assert!(
            note.contains("earlier occurrences were let go"),
            "the model must not think it owes thirteen more: {note}"
        );
    }

    #[test]
    fn a_schedule_round_trips_through_json_readably() {
        // A thread file is meant to be readable, so the time is HH:MM rather
        // than a serialised struct.
        let daily = Schedule::Daily { at: at(7, 30) };
        let json = serde_json::to_string(&daily).expect("json");
        assert!(json.contains(r#""at":"07:30""#), "{json}");
        assert!(json.contains(r#""every":"daily""#), "{json}");
        assert_eq!(
            serde_json::from_str::<Schedule>(&json).expect("read back"),
            daily
        );
    }

    #[test]
    fn every_variant_round_trips() {
        for schedule in [
            Schedule::Hours { hours: 6 },
            Schedule::Daily { at: at(7, 0) },
            Schedule::Weekdays { at: at(8, 15) },
            Schedule::Weekly {
                day: Weekday::Wed,
                at: at(17, 0),
            },
        ] {
            let json = serde_json::to_string(&schedule).expect("json");
            assert_eq!(
                serde_json::from_str::<Schedule>(&json).expect("read back"),
                schedule,
                "{json}"
            );
        }
    }

    #[test]
    fn a_schedule_reads_as_a_sentence() {
        assert_eq!(
            Schedule::Daily { at: at(7, 0) }.describe(),
            "Daily at 07:00"
        );
        assert_eq!(
            Schedule::Weekdays { at: at(8, 15) }.describe(),
            "Weekdays at 08:15"
        );
        assert_eq!(Schedule::Hours { hours: 1 }.describe(), "Every hour");
        assert_eq!(Schedule::Hours { hours: 6 }.describe(), "Every 6 hours");
    }

    #[test]
    fn the_model_is_told_the_turn_came_from_the_clock() {
        // Otherwise it opens with "as you asked" when nobody asked.
        let regular = preamble(Due::Regular, local("2026-08-03 07:00"));
        assert!(regular.contains("scheduled run"), "{regular}");
        assert!(regular.contains("Nobody is necessarily"), "{regular}");

        let late = preamble(Due::Late, local("2026-08-03 07:00"));
        assert!(late.contains("late"), "{late}");
        assert!(late.contains("time-sensitive"), "{late}");
    }

    #[test]
    fn next_after_always_moves_forward() {
        // A schedule that returned a time in the past would fire every tick.
        let now = local("2026-08-03 07:00");
        for schedule in [
            Schedule::Hours { hours: 1 },
            Schedule::Daily { at: at(7, 0) },
            Schedule::Weekdays { at: at(7, 0) },
            Schedule::Weekly {
                day: Weekday::Mon,
                at: at(7, 0),
            },
        ] {
            assert!(
                schedule.next_after(now) > now,
                "{} went backwards",
                schedule.describe()
            );
        }
    }
}
