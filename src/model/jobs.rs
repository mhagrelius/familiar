//! What runs on its own, as a list rather than a field.
//!
//! A schedule used to be `Option<Heartbeat>` on a [`Thread`](super::thread::Thread),
//! which put the ownership the wrong way round. A chat is short-lived and a
//! schedule is not: you set up a morning briefing once and it outlives every
//! conversation it ever writes into. As a property of a chat it could only ever
//! be singular, setting a second one silently destroyed the first, and the
//! whole thing only fired while its own chat happened to be on screen.
//!
//! So the schedule owns the chat. A [`Job`] names where its answer lands; a
//! chat may be the destination of several, or of none.
//!
//! **Three sources, one engine.** The application had grown three schedulers —
//! nightly consolidation, the proactive lookout, and the heartbeat — each with
//! its own storage and its own notion of being due. [`Source`] is what lets them
//! be one list without pretending they are the same kind of thing: the user's
//! jobs are editable and the system's are not, and a surface can say so.
//!
//! Everything here is pure. The clock is passed in, nothing touches a vault, and
//! the tests need no display.

use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};

use super::heartbeat::{Due, Recovery, Schedule};

/// Who asked for this job, which decides what may edit it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Set up in the app by the person using it.
    #[default]
    User,
    /// The model set it up mid-conversation, through the `schedule` tool. Still
    /// the user's to edit — the distinction is provenance, not ownership, and
    /// it is worth keeping because "why is this running?" is a real question.
    Agent,
    /// The application's own upkeep: consolidation, the lookout. Not editable,
    /// and shown separately or not at all.
    System,
}

impl Source {
    /// Whether a person may change this job's schedule and prompt.
    ///
    /// The system's jobs have their cadence in Preferences, where it belongs;
    /// offering the same thing twice in two places is how the two come to
    /// disagree.
    pub fn editable(&self) -> bool {
        !matches!(self, Self::System)
    }
}

/// Where a job's answer goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "into", rename_all = "snake_case")]
pub enum Destination {
    /// Append to one chat, run after run, so a week of briefings reads back in
    /// one place. The default, and the reason a heartbeat was a thread's
    /// property in the first place — that instinct was right, it was only the
    /// direction of ownership that was wrong.
    Chat { slug: String, thread: String },
    /// A fresh chat each time, reported by notification. For work whose runs
    /// are genuinely independent, where one long thread is a worse record than
    /// many short ones.
    FreshChat { slug: String },
    /// No chat at all — the job is its own thing and reports by notification.
    /// What the lookout and consolidation are.
    Nothing,
}

impl Destination {
    /// The project this job belongs to, if it belongs to one.
    pub fn slug(&self) -> Option<&str> {
        match self {
            Self::Chat { slug, .. } | Self::FreshChat { slug } => Some(slug),
            Self::Nothing => None,
        }
    }

    /// The chat it appends to, if it appends to one.
    pub fn thread(&self) -> Option<&str> {
        match self {
            Self::Chat { thread, .. } => Some(thread),
            _ => None,
        }
    }
}

/// One thing that runs on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// Stable across edits, because every surface — a notification, a D-Bus
    /// call, a panel row — needs to name one job and have it still mean the
    /// same job tomorrow. A thread id could not do that: two jobs may share a
    /// chat.
    pub id: String,
    /// What it is called in a list. Empty is allowed and read as the prompt's
    /// first line, so a job made in a hurry is still findable.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub source: Source,
    pub schedule: Schedule,
    /// The instruction to run each time, written as if the user had typed it.
    pub prompt: String,
    pub destination: Destination,
    /// Off without being forgotten. A schedule you have paused is one you
    /// intend to resume, and deleting it to stop it for a week is how people
    /// lose the prompt they spent time on.
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub recovery: Recovery,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<String>,
}

fn yes() -> bool {
    true
}

impl Job {
    pub fn new(id: impl Into<String>, schedule: Schedule, prompt: &str, into: Destination) -> Self {
        Self {
            id: id.into(),
            name: String::new(),
            source: Source::User,
            schedule,
            prompt: prompt.trim().to_string(),
            destination: into,
            enabled: true,
            recovery: Recovery::default(),
            last_run: None,
            last_outcome: None,
        }
    }

    /// An id nothing else will have.
    ///
    /// The clock to the millisecond plus a counter, which is what thread ids
    /// already do and for the same reason: jobs are created one at a time by a
    /// person, never in bulk, and a readable id is worth more here than a UUID.
    pub fn mint(now: DateTime<Utc>, taken: &[String]) -> String {
        let base = now.format("job-%Y%m%d-%H%M%S").to_string();
        if !taken.iter().any(|had| had == &base) {
            return base;
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|candidate| !taken.iter().any(|had| had == candidate))
            .unwrap_or(base)
    }

    /// What this is called in a list.
    pub fn title(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.trim().to_string();
        }
        let first = self
            .prompt
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("Untitled job");
        if first.chars().count() > 60 {
            format!("{}…", first.chars().take(59).collect::<String>())
        } else {
            first.to_string()
        }
    }

    /// Whether to run now, why, and which occurrence it is for.
    pub fn due(&self, now: DateTime<Local>) -> Option<(Due, DateTime<Local>)> {
        if !self.enabled || self.prompt.trim().is_empty() {
            return None;
        }
        self.schedule.due(
            self.last_run.map(|last| last.with_timezone(&Local)),
            now,
            self.recovery,
        )
    }

    /// The next time this is expected to run, for a list that says so.
    pub fn next_run(&self, now: DateTime<Local>) -> Option<DateTime<Local>> {
        self.enabled.then(|| {
            self.schedule.next_after(
                self.last_run
                    .map(|last| last.with_timezone(&Local))
                    .unwrap_or(now),
            )
        })
    }
}

/// Every job, as one file.
///
/// One list rather than a job per project, because "what is running on this
/// machine?" is the question people actually ask and answering it should not
/// mean walking every project. The project a job belongs to is on its
/// destination.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Jobs {
    #[serde(default)]
    pub jobs: Vec<Job>,
}

impl Jobs {
    pub fn get(&self, id: &str) -> Option<&Job> {
        self.jobs.iter().find(|job| job.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|job| job.id == id)
    }

    /// Add a job, minting an id for it.
    pub fn add(&mut self, mut job: Job, now: DateTime<Utc>) -> String {
        let taken: Vec<String> = self.jobs.iter().map(|job| job.id.clone()).collect();
        if job.id.trim().is_empty() || taken.iter().any(|had| had == &job.id) {
            job.id = Job::mint(now, &taken);
        }
        let id = job.id.clone();
        self.jobs.push(job);
        id
    }

    pub fn remove(&mut self, id: &str) -> Option<Job> {
        let at = self.jobs.iter().position(|job| job.id == id)?;
        Some(self.jobs.remove(at))
    }

    /// The job that should run now, if any.
    ///
    /// One at a time: there is one local server and a person waiting on an
    /// answer must not queue behind two briefings. The most overdue goes first
    /// so a backlog drains in the order it accumulated rather than by whichever
    /// happens to be earliest in the file.
    pub fn next_due(&self, now: DateTime<Local>) -> Option<(&Job, Due, DateTime<Local>)> {
        self.jobs
            .iter()
            .filter_map(|job| job.due(now).map(|(why, at)| (job, why, at)))
            .min_by_key(|(_, _, at)| *at)
    }

    /// Every job that lands in this chat, which is what a chat's own header can
    /// say. Several may.
    pub fn for_chat<'a>(&'a self, slug: &'a str, thread: &'a str) -> impl Iterator<Item = &'a Job> {
        self.jobs.iter().filter(move |job| {
            job.destination.slug() == Some(slug) && job.destination.thread() == Some(thread)
        })
    }

    /// Drop every job whose chat is gone.
    ///
    /// A job pointing at a deleted chat would run forever against nothing, and
    /// silently: the turn would be written to a thread nobody can reach.
    /// ChatGPT pauses these; deleting is the same decision made louder, and the
    /// prompt is in the file the user is deleting anyway.
    pub fn forget_chat(&mut self, slug: &str, thread: &str) -> usize {
        let before = self.jobs.len();
        self.jobs.retain(|job| {
            !(job.destination.slug() == Some(slug) && job.destination.thread() == Some(thread))
        });
        before - self.jobs.len()
    }

    /// Build a list from the heartbeats threads used to carry.
    ///
    /// Run once, when there is no jobs file yet. Every field carries over,
    /// including `last_run` — a migration that reset the clock would make every
    /// schedule on the machine fire at once, or (with the old rule) go quiet
    /// for a day. The thread keeps its `heartbeat` afterwards rather than
    /// having it stripped: a file that an older build can still open is worth
    /// more than a tidy one, and [`Self::migrated`] is only consulted when the
    /// jobs file is absent.
    pub fn migrated(
        found: impl IntoIterator<Item = (String, String, super::thread::Heartbeat)>,
    ) -> Self {
        let mut jobs = Jobs::default();
        for (slug, thread, beat) in found {
            let id = format!("job-{thread}");
            jobs.jobs.push(Job {
                id,
                name: String::new(),
                // The old heartbeat had no provenance. `User` rather than
                // `Agent` because it is the safe answer: it stays editable.
                source: Source::User,
                schedule: beat.schedule,
                prompt: beat.prompt,
                destination: Destination::Chat { slug, thread },
                enabled: beat.enabled,
                recovery: beat.recovery,
                last_run: beat.last_run,
                last_outcome: beat.last_outcome,
            });
        }
        jobs
    }

    /// Drop every job belonging to a project that is gone.
    pub fn forget_project(&mut self, slug: &str) -> usize {
        let before = self.jobs.len();
        self.jobs.retain(|job| job.destination.slug() != Some(slug));
        before - self.jobs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn local(text: &str) -> DateTime<Local> {
        Local
            .from_local_datetime(
                &chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").expect("a time"),
            )
            .earliest()
            .expect("a local time")
    }

    fn at(hour: u32, minute: u32) -> chrono::NaiveTime {
        chrono::NaiveTime::from_hms_opt(hour, minute, 0).expect("a time")
    }

    fn briefing(id: &str, thread: &str) -> Job {
        Job::new(
            id,
            Schedule::Daily { at: at(7, 0) },
            "the morning briefing",
            Destination::Chat {
                slug: "default".into(),
                thread: thread.into(),
            },
        )
    }

    #[test]
    fn a_chat_can_be_the_destination_of_several_jobs() {
        // The whole reason for the change: two schedules on one chat used to be
        // impossible, and setting the second silently destroyed the first.
        let mut jobs = Jobs::default();
        jobs.add(briefing("morning", "chat-1"), Utc::now());
        let mut evening = briefing("evening", "chat-1");
        evening.schedule = Schedule::Daily { at: at(18, 0) };
        jobs.add(evening, Utc::now());

        assert_eq!(jobs.jobs.len(), 2);
        assert_eq!(jobs.for_chat("default", "chat-1").count(), 2);
        assert!(jobs.get("morning").is_some(), "the first must survive");
    }

    #[test]
    fn adding_a_job_with_a_taken_id_mints_a_new_one_instead_of_replacing() {
        let mut jobs = Jobs::default();
        jobs.add(briefing("same", "chat-1"), Utc::now());
        let id = jobs.add(briefing("same", "chat-2"), Utc::now());
        assert_ne!(id, "same");
        assert_eq!(jobs.jobs.len(), 2, "nothing may be silently overwritten");
    }

    #[test]
    fn the_most_overdue_job_runs_first() {
        // A backlog drains in the order it accumulated. Picking by file order
        // would starve whichever job happened to be written last.
        let mut jobs = Jobs::default();
        let mut early = briefing("early", "chat-1");
        early.recovery = Recovery::Whenever;
        early.schedule = Schedule::Daily { at: at(6, 0) };
        early.last_run = Some(local("2026-08-02 06:00").with_timezone(&Utc));
        let mut late = briefing("late", "chat-2");
        late.recovery = Recovery::Whenever;
        late.schedule = Schedule::Daily { at: at(8, 0) };
        late.last_run = Some(local("2026-08-02 08:00").with_timezone(&Utc));
        // Deliberately out of order in the file.
        jobs.add(late, Utc::now());
        jobs.add(early, Utc::now());

        let (job, _, at) = jobs
            .next_due(local("2026-08-03 09:00"))
            .expect("one is due");
        assert_eq!(job.id, "early");
        assert_eq!(at, local("2026-08-03 06:00"));
    }

    #[test]
    fn a_paused_job_is_never_due() {
        let mut job = briefing("paused", "chat-1");
        job.enabled = false;
        job.recovery = Recovery::Whenever;
        job.last_run = Some(local("2026-07-20 07:00").with_timezone(&Utc));
        assert_eq!(job.due(local("2026-08-03 09:00")), None);
    }

    #[test]
    fn a_job_with_an_empty_prompt_never_runs() {
        // There is nothing to submit, and submitting nothing spends a turn to
        // produce an answer to no question.
        let mut job = briefing("blank", "chat-1");
        job.prompt = "   ".into();
        job.last_run = Some(local("2026-08-02 07:00").with_timezone(&Utc));
        assert_eq!(job.due(local("2026-08-03 07:00")), None);
    }

    #[test]
    fn deleting_a_chat_takes_its_jobs_with_it() {
        // A job pointing at a chat that is gone would run forever against
        // nothing, writing turns nobody can reach.
        let mut jobs = Jobs::default();
        jobs.add(briefing("a", "chat-1"), Utc::now());
        jobs.add(briefing("b", "chat-1"), Utc::now());
        jobs.add(briefing("c", "chat-2"), Utc::now());

        assert_eq!(jobs.forget_chat("default", "chat-1"), 2);
        assert_eq!(jobs.jobs.len(), 1);
        assert_eq!(jobs.jobs[0].id, "c");
    }

    #[test]
    fn deleting_a_project_takes_its_jobs_with_it() {
        let mut jobs = Jobs::default();
        jobs.add(briefing("a", "chat-1"), Utc::now());
        let mut elsewhere = briefing("b", "chat-9");
        elsewhere.destination = Destination::Chat {
            slug: "other".into(),
            thread: "chat-9".into(),
        };
        jobs.add(elsewhere, Utc::now());

        assert_eq!(jobs.forget_project("other"), 1);
        assert_eq!(jobs.jobs.len(), 1);
        assert_eq!(jobs.jobs[0].id, "a");
    }

    #[test]
    fn a_job_names_itself_from_its_prompt_when_nobody_named_it() {
        // A scheduled job is the kind you go looking for weeks later in a list,
        // and "Untitled" in a list of five is useless.
        let job = briefing("x", "chat-1");
        assert_eq!(job.title(), "the morning briefing");

        let mut named = briefing("y", "chat-1");
        named.name = "Morning Briefing".into();
        assert_eq!(named.title(), "Morning Briefing");
    }

    #[test]
    fn a_long_prompt_is_cut_rather_than_filling_the_list() {
        let mut job = briefing("x", "chat-1");
        job.prompt = "a".repeat(200);
        let title = job.title();
        assert!(title.chars().count() <= 60, "{title}");
        assert!(title.ends_with('…'));
    }

    #[test]
    fn the_systems_own_jobs_are_not_editable() {
        // Consolidation's cadence lives in Preferences. Offering it twice is
        // how two places come to disagree.
        assert!(!Source::System.editable());
        assert!(Source::User.editable());
        assert!(
            Source::Agent.editable(),
            "the model setting it up does not make it the model's"
        );
    }

    #[test]
    fn a_job_round_trips_through_json() {
        let mut job = briefing("morning", "chat-1");
        job.recovery = Recovery::SameDay;
        job.source = Source::Agent;
        let json = serde_json::to_string(&job).expect("json");
        assert_eq!(
            serde_json::from_str::<Job>(&json).expect("read back"),
            job,
            "{json}"
        );
    }

    #[test]
    fn a_job_file_written_without_the_optional_fields_still_reads() {
        // Forward compatibility in the direction that matters: a file hand
        // edited, or written by an older build, must not fail to parse.
        let json = r#"{
            "jobs": [{
                "id": "morning",
                "schedule": { "every": "daily", "at": "07:00" },
                "prompt": "the morning briefing",
                "destination": { "into": "chat", "slug": "default", "thread": "chat-1" }
            }]
        }"#;
        let jobs: Jobs = serde_json::from_str(json).expect("an older jobs file");
        let job = jobs.get("morning").expect("the job");
        assert!(job.enabled, "a job with no `enabled` key is on");
        assert_eq!(job.recovery, Recovery::OnTime);
        assert_eq!(job.source, Source::User);
    }

    #[test]
    fn a_fresh_chat_job_belongs_to_a_project_but_to_no_thread() {
        let job = Job::new(
            "weekly",
            Schedule::Weekly {
                day: chrono::Weekday::Mon,
                at: at(9, 0),
            },
            "the weekly review",
            Destination::FreshChat {
                slug: "work".into(),
            },
        );
        assert_eq!(job.destination.slug(), Some("work"));
        assert_eq!(job.destination.thread(), None);
        // And deleting a chat must not take it with them.
        let mut jobs = Jobs::default();
        jobs.add(job, Utc::now());
        assert_eq!(jobs.forget_chat("work", "chat-1"), 0);
        assert_eq!(jobs.forget_project("work"), 1);
    }

    #[test]
    fn migrating_a_heartbeat_keeps_its_clock_and_its_recovery() {
        // Resetting `last_run` would make every schedule on the machine fire at
        // once the first time the new build starts.
        use crate::model::thread::Heartbeat;
        let mut beat = Heartbeat::new(Schedule::Daily { at: at(7, 0) }, "the morning briefing");
        beat.recovery = Recovery::SameDay;
        beat.enabled = false;
        beat.last_run = Some(local("2026-08-02 07:00").with_timezone(&Utc));
        beat.last_outcome = Some("Nothing to report.".into());

        let jobs = Jobs::migrated([("default".to_string(), "chat-1".to_string(), beat.clone())]);
        assert_eq!(jobs.jobs.len(), 1);
        let job = &jobs.jobs[0];
        assert_eq!(job.last_run, beat.last_run);
        assert_eq!(job.recovery, Recovery::SameDay);
        assert!(!job.enabled, "a paused schedule stays paused");
        assert_eq!(job.last_outcome.as_deref(), Some("Nothing to report."));
        assert_eq!(
            job.destination,
            Destination::Chat {
                slug: "default".into(),
                thread: "chat-1".into()
            }
        );
        assert!(
            job.source.editable(),
            "a migrated schedule must stay editable"
        );
    }

    #[test]
    fn migrating_gives_each_chat_a_distinct_job_id() {
        use crate::model::thread::Heartbeat;
        let beat = Heartbeat::new(Schedule::Daily { at: at(7, 0) }, "a briefing");
        let jobs = Jobs::migrated([
            ("default".to_string(), "chat-1".to_string(), beat.clone()),
            ("default".to_string(), "chat-2".to_string(), beat.clone()),
            ("work".to_string(), "chat-3".to_string(), beat),
        ]);
        let mut ids: Vec<&str> = jobs.jobs.iter().map(|job| job.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 3, "ids collided: {ids:?}");
    }

    #[test]
    fn minting_an_id_twice_in_the_same_second_still_gives_two() {
        let now = Utc::now();
        let first = Job::mint(now, &[]);
        let second = Job::mint(now, std::slice::from_ref(&first));
        assert_ne!(first, second);
    }
}
