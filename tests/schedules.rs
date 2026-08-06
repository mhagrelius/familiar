//! Schedules across a whole store, with no server and no display.
//!
//! The unit tests in `heartbeat.rs` pin the arithmetic on times passed in. This
//! drives the seam the application actually crosses: a heartbeat written to a
//! thread file, read back off disk, and then asked whether it is due — because
//! the two bugs worth catching here live in the join. A `Recovery` that does
//! not survive `serde` is a job that quietly reverts to the old behaviour, and
//! a scan that cannot tell a paused schedule from a live one wakes chats
//! somebody switched off.

use chrono::{Datelike, Duration, Local, TimeZone, Utc};

use familiar::model::heartbeat::{Due, Recovery, Schedule};
use familiar::model::project::{Store, DEFAULT_PROJECT};
use familiar::model::thread::{Heartbeat, Thread};

fn local(text: &str) -> chrono::DateTime<Local> {
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

/// A thread carrying a schedule that last ran at `last`.
fn scheduled(store: &Store, recovery: Recovery, last: chrono::DateTime<Local>) -> Thread {
    let mut thread = store.new_thread(DEFAULT_PROJECT).expect("a thread");
    let mut beat = Heartbeat::new(Schedule::Daily { at: at(7, 0) }, "the morning briefing");
    beat.recovery = recovery;
    beat.last_run = Some(last.with_timezone(&Utc));
    thread.heartbeat = Some(beat);
    // An empty thread is never written, so give it something to say.
    thread.push_turn(familiar::model::thread::StoredTurn {
        user: "set up a briefing".into(),
        answer: "Done.".into(),
        ..Default::default()
    });
    thread
}

#[test]
fn a_recovery_choice_survives_the_round_trip() {
    // The whole point of the field. Defaulting silently back to `OnTime` on
    // read would look exactly like the feature not working.
    let directory = tempfile::tempdir().expect("a directory");
    let store = Store::new(directory.path());
    let thread = scheduled(&store, Recovery::SameDay, local("2026-08-02 07:00"));
    store
        .save_thread(DEFAULT_PROJECT, &thread)
        .expect("save the thread");

    let read = store
        .load_thread(DEFAULT_PROJECT, &thread.id)
        .expect("read it back");
    let beat = read.heartbeat.expect("a heartbeat");
    assert_eq!(beat.recovery, Recovery::SameDay);
    assert_eq!(beat.prompt, "the morning briefing");
}

#[test]
fn a_thread_written_before_recovery_existed_still_reads() {
    // A file from an older build has no `recovery` key at all, and must come
    // back behaving exactly as it did rather than failing to parse.
    let json = r#"{
        "version": 1,
        "id": "2026-08-01T07-00-00.000",
        "created": "2026-08-01T07:00:00Z",
        "updated": "2026-08-01T07:00:00Z",
        "heartbeat": {
            "schedule": { "every": "daily", "at": "07:00" },
            "prompt": "the morning briefing",
            "enabled": true,
            "last_run": "2026-08-02T07:00:00Z"
        },
        "entries": []
    }"#;
    let thread: Thread = serde_json::from_str(json).expect("an older thread");
    let beat = thread.heartbeat.expect("a heartbeat");
    assert_eq!(
        beat.recovery,
        Recovery::OnTime,
        "an older file must not silently gain a more permissive policy"
    );
}

#[test]
fn the_power_cut_recovers_from_disk_and_then_owes_nothing() {
    // The case end to end: the machine was off at 07:00, the app comes up at
    // 08:40, and the schedule is read off disk rather than held in memory.
    let directory = tempfile::tempdir().expect("a directory");
    let store = Store::new(directory.path());
    let thread = scheduled(&store, Recovery::SameDay, local("2026-08-02 07:00"));
    store
        .save_thread(DEFAULT_PROJECT, &thread)
        .expect("save the thread");

    let mut read = store
        .load_thread(DEFAULT_PROJECT, &thread.id)
        .expect("read it back");
    let back = local("2026-08-03 08:40");
    let (due, scheduled_for) = read
        .heartbeat
        .as_ref()
        .expect("a heartbeat")
        .due(back)
        .expect("the briefing is owed");
    assert_eq!(due, Due::Recovered);
    assert_eq!(scheduled_for, local("2026-08-03 07:00"));

    // The application records the run before starting the turn, so that is what
    // the next tick sees. Nothing more is owed today.
    read.heartbeat.as_mut().expect("a heartbeat").last_run = Some(back.with_timezone(&Utc));
    assert_eq!(
        read.heartbeat.as_ref().expect("a heartbeat").due(back),
        None
    );
    assert_eq!(
        read.heartbeat
            .as_ref()
            .expect("a heartbeat")
            .due(back + Duration::minutes(30)),
        None,
        "a tick half an hour later must not run it again"
    );
}

#[test]
fn a_fortnight_of_downtime_owes_exactly_one_run() {
    let directory = tempfile::tempdir().expect("a directory");
    let store = Store::new(directory.path());
    let thread = scheduled(&store, Recovery::Whenever, local("2026-07-20 07:00"));
    store
        .save_thread(DEFAULT_PROJECT, &thread)
        .expect("save the thread");

    let mut read = store
        .load_thread(DEFAULT_PROJECT, &thread.id)
        .expect("read it back");
    let back = local("2026-08-03 09:00");

    // One run, for this morning — not one per missed morning.
    let (due, scheduled_for) = read
        .heartbeat
        .as_ref()
        .expect("a heartbeat")
        .due(back)
        .expect("one run is owed");
    assert_eq!(due, Due::Recovered);
    assert_eq!(scheduled_for, local("2026-08-03 07:00"));

    read.heartbeat.as_mut().expect("a heartbeat").last_run = Some(back.with_timezone(&Utc));
    assert_eq!(
        read.heartbeat.as_ref().expect("a heartbeat").due(back),
        None,
        "the other thirteen were let go, not queued"
    );
}

#[test]
fn a_paused_schedule_is_never_due_however_permissive_its_recovery() {
    // The scan wakes every chat now, so a schedule somebody switched off has to
    // stay off — this is the one that stops "pause" meaning "pause until the
    // next restart".
    let directory = tempfile::tempdir().expect("a directory");
    let store = Store::new(directory.path());
    let mut thread = scheduled(&store, Recovery::Whenever, local("2026-07-20 07:00"));
    thread.heartbeat.as_mut().expect("a heartbeat").enabled = false;
    store
        .save_thread(DEFAULT_PROJECT, &thread)
        .expect("save the thread");

    let read = store
        .load_thread(DEFAULT_PROJECT, &thread.id)
        .expect("read it back");
    assert_eq!(
        read.heartbeat
            .as_ref()
            .expect("a heartbeat")
            .due(local("2026-08-03 09:00")),
        None
    );
}

#[test]
fn same_day_stops_at_midnight_rather_than_after_a_fixed_window() {
    // The boundary that makes `SameDay` predictable. Late the same evening is
    // still that day's run; a minute past midnight is not, and must wait for
    // the morning rather than delivering yesterday's briefing.
    let daily = Schedule::Daily { at: at(7, 0) };
    let last = local("2026-08-02 07:00");
    assert_eq!(
        daily
            .due(Some(last), local("2026-08-03 23:59"), Recovery::SameDay)
            .map(|(due, _)| due),
        Some(Due::Recovered)
    );
    // Just after midnight the newest occurrence is still the 3rd's 07:00, and
    // it is now yesterday's.
    assert_eq!(
        daily.due(Some(last), local("2026-08-04 00:01"), Recovery::SameDay),
        None
    );
}

#[test]
fn an_hourly_schedule_coalesces_the_same_way() {
    // The one variant whose grid is relative to the last run rather than to the
    // wall clock, so it gets its own check that a gap collapses to one run.
    let every = Schedule::Hours { hours: 4 };
    let last = local("2026-08-01 06:00");
    let back = local("2026-08-03 09:00");
    let (due, scheduled_for) = every
        .due(Some(last), back, Recovery::Whenever)
        .expect("one run is owed");
    assert_eq!(due, Due::Recovered);
    // The newest whole step before `back`, not the first one after `last`.
    assert!(
        scheduled_for > local("2026-08-03 05:00") && scheduled_for <= back,
        "{scheduled_for} should be the most recent step"
    );
    assert_eq!(every.due(Some(back), back, Recovery::Whenever), None);
}

#[test]
fn weekly_recovery_survives_the_round_trip() {
    // Weekly is the schedule most likely to be missed by a machine being off,
    // and the one whose "most recent occurrence" arithmetic reaches furthest
    // back — so it gets the disk round trip too.
    let directory = tempfile::tempdir().expect("a directory");
    let store = Store::new(directory.path());
    let mut thread = store.new_thread(DEFAULT_PROJECT).expect("a thread");
    let mut beat = Heartbeat::new(
        Schedule::Weekly {
            day: chrono::Weekday::Mon,
            at: at(9, 0),
        },
        "the weekly review",
    );
    beat.recovery = Recovery::Whenever;
    // 2026-07-27 is a Monday.
    beat.last_run = Some(local("2026-07-27 09:00").with_timezone(&Utc));
    thread.heartbeat = Some(beat);
    thread.push_turn(familiar::model::thread::StoredTurn {
        user: "set up a review".into(),
        answer: "Done.".into(),
        ..Default::default()
    });
    store
        .save_thread(DEFAULT_PROJECT, &thread)
        .expect("save the thread");

    let read = store
        .load_thread(DEFAULT_PROJECT, &thread.id)
        .expect("read it back");
    // Asked on the Wednesday: the owed run is Monday the 3rd, not "today".
    let (due, scheduled_for) = read
        .heartbeat
        .as_ref()
        .expect("a heartbeat")
        .due(local("2026-08-05 12:00"))
        .expect("the review is owed");
    assert_eq!(due, Due::Recovered);
    assert_eq!(scheduled_for, local("2026-08-03 09:00"));
    assert_eq!(scheduled_for.weekday(), chrono::Weekday::Mon);
}

#[test]
fn on_time_is_still_the_default_for_a_schedule_nobody_configured() {
    // Every existing thread has to keep behaving exactly as it did, or this
    // change is a silent behavioural one for everybody who never asked for it.
    let beat = Heartbeat::new(Schedule::Daily { at: at(7, 0) }, "a briefing");
    assert_eq!(beat.recovery, Recovery::OnTime);

    let directory = tempfile::tempdir().expect("a directory");
    let store = Store::new(directory.path());
    let thread = scheduled(&store, Recovery::OnTime, local("2026-08-02 07:00"));
    store
        .save_thread(DEFAULT_PROJECT, &thread)
        .expect("save the thread");
    let read = store
        .load_thread(DEFAULT_PROJECT, &thread.id)
        .expect("read it back");
    // Seven hours late, which used to be discarded and still is.
    assert_eq!(
        read.heartbeat
            .as_ref()
            .expect("a heartbeat")
            .due(local("2026-08-03 14:00")),
        None
    );
}

#[test]
fn the_migration_finds_every_heartbeat_and_runs_only_once() {
    // This runs against real user data exactly once, so it gets the most
    // attention: every schedule has to arrive, none may arrive twice, and a
    // schedule the user deletes afterwards must not come back on next start.
    use familiar::model::jobs::{Destination, Jobs};

    let directory = tempfile::tempdir().expect("a directory");
    let store = Store::new(directory.path());
    assert!(!store.has_jobs_file(), "a fresh store has no jobs file");

    let one = scheduled(&store, Recovery::SameDay, local("2026-08-02 07:00"));
    store.save_thread(DEFAULT_PROJECT, &one).expect("save");
    let two = scheduled(&store, Recovery::Whenever, local("2026-08-01 07:00"));
    store.save_thread(DEFAULT_PROJECT, &two).expect("save");
    // A chat with no schedule, which must contribute nothing.
    let mut plain = store.new_thread(DEFAULT_PROJECT).expect("a thread");
    plain.push_turn(familiar::model::thread::StoredTurn {
        user: "hello".into(),
        answer: "Hello.".into(),
        ..Default::default()
    });
    store.save_thread(DEFAULT_PROJECT, &plain).expect("save");

    let slugs = vec![DEFAULT_PROJECT.to_string()];
    let found = store.heartbeats(&slugs);
    assert_eq!(
        found.len(),
        2,
        "both schedules should be found, and only those"
    );

    let jobs = Jobs::migrated(found);
    assert_eq!(jobs.jobs.len(), 2);
    // Each points at the chat it came from, and the recoveries carried over.
    for thread in [&one, &two] {
        let job = jobs
            .jobs
            .iter()
            .find(|job| job.destination.thread() == Some(thread.id.to_string().as_str()))
            .unwrap_or_else(|| panic!("no job for {}", thread.id));
        assert_eq!(
            job.destination,
            Destination::Chat {
                slug: DEFAULT_PROJECT.into(),
                thread: thread.id.to_string(),
            }
        );
        assert_eq!(
            job.recovery,
            thread.heartbeat.as_ref().expect("a heartbeat").recovery
        );
        assert_eq!(
            job.last_run,
            thread.heartbeat.as_ref().expect("a heartbeat").last_run,
            "the clock must carry over or everything fires at once"
        );
    }

    store.save_jobs(&jobs).expect("write the jobs file");
    assert!(store.has_jobs_file());

    // The user deletes one. The next start must not resurrect it, which is what
    // `has_jobs_file` guards — the heartbeats are still on the threads.
    let mut after = store.load_jobs();
    let dropped = after.jobs[0].id.clone();
    after.remove(&dropped);
    store.save_jobs(&after).expect("write it back");

    assert_eq!(
        store.heartbeats(&slugs).len(),
        2,
        "the threads still carry them"
    );
    let reloaded = store.load_jobs();
    assert_eq!(reloaded.jobs.len(), 1, "the deleted job must stay deleted");
    assert!(reloaded.get(&dropped).is_none());
}

#[test]
fn a_jobs_file_that_will_not_parse_reads_as_empty_rather_than_failing() {
    // The rule the rest of the store follows: a machine with one bad file
    // should still start. And it must not be overwritten until something is
    // saved, so a hand edit with a typo in it can still be fixed by hand.
    let directory = tempfile::tempdir().expect("a directory");
    let store = Store::new(directory.path());
    std::fs::write(store.jobs_path(), "{ this is not json").expect("write a bad file");

    let jobs = store.load_jobs();
    assert!(jobs.jobs.is_empty());
    let text = std::fs::read_to_string(store.jobs_path()).expect("still there");
    assert_eq!(text, "{ this is not json", "reading must not rewrite it");
}

#[test]
fn jobs_survive_the_round_trip_through_the_store() {
    use familiar::model::jobs::{Destination, Job, Jobs, Source};

    let directory = tempfile::tempdir().expect("a directory");
    let store = Store::new(directory.path());
    let mut jobs = Jobs::default();
    let mut job = Job::new(
        "",
        Schedule::Weekdays { at: at(8, 30) },
        "check the pull requests",
        Destination::FreshChat {
            slug: "work".into(),
        },
    );
    job.source = Source::Agent;
    job.recovery = Recovery::Whenever;
    job.name = "Weekday PRs".into();
    let id = jobs.add(job, Utc::now());
    assert!(!id.is_empty(), "an id should have been minted");
    store.save_jobs(&jobs).expect("save");

    let read = store.load_jobs();
    assert_eq!(read, jobs);
    let job = read.get(&id).expect("the job");
    assert_eq!(job.title(), "Weekday PRs");
    assert_eq!(job.source, Source::Agent);
    assert_eq!(job.recovery, Recovery::Whenever);
}

#[test]
fn a_fold_belongs_to_the_chat_it_summarises() {
    // A fold is a lossy rewrite of what gets *sent*. Installing one on the
    // wrong conversation silently shortens it — and a scheduled chat grows a
    // turn a day, so it is exactly the kind that folds, and exactly the kind
    // that is not on screen when it does.
    use familiar::model::compaction::Fold;

    let directory = tempfile::tempdir().expect("a directory");
    let store = Store::new(directory.path());

    let mut background = store.new_thread(DEFAULT_PROJECT).expect("a thread");
    background.push_turn(familiar::model::thread::StoredTurn {
        user: "the morning briefing".into(),
        answer: "Three pull requests are open.".into(),
        ..Default::default()
    });
    store
        .save_thread(DEFAULT_PROJECT, &background)
        .expect("save");

    let mut open = store.new_thread(DEFAULT_PROJECT).expect("a thread");
    open.push_turn(familiar::model::thread::StoredTurn {
        user: "something else entirely".into(),
        answer: "Unrelated.".into(),
        ..Default::default()
    });
    store.save_thread(DEFAULT_PROJECT, &open).expect("save");

    // What `install_fold` does to the chat it names.
    let mut folded = store
        .load_thread(DEFAULT_PROJECT, &background.id)
        .expect("read it back");
    folded.fold = Some(Fold {
        summary: "Earlier briefings.".into(),
        covers: 1,
    });
    store.save_thread(DEFAULT_PROJECT, &folded).expect("save");

    let background_again = store
        .load_thread(DEFAULT_PROJECT, &background.id)
        .expect("read it back");
    assert!(
        background_again.fold.is_some(),
        "the chat that was summarised should carry the fold"
    );
    let open_again = store
        .load_thread(DEFAULT_PROJECT, &open.id)
        .expect("read it back");
    assert!(
        open_again.fold.is_none(),
        "the chat that happened to be open must not have gained one"
    );
    // And the two are genuinely different chats, so the assertion above is not
    // vacuous.
    assert_ne!(background.id, open.id);
}

#[test]
fn a_chat_with_no_schedule_is_skipped_by_the_scan() {
    // Most chats have no heartbeat, and the scan reads all of them.
    let directory = tempfile::tempdir().expect("a directory");
    let store = Store::new(directory.path());
    let mut thread = store.new_thread(DEFAULT_PROJECT).expect("a thread");
    thread.push_turn(familiar::model::thread::StoredTurn {
        user: "hello".into(),
        answer: "Hello.".into(),
        ..Default::default()
    });
    store
        .save_thread(DEFAULT_PROJECT, &thread)
        .expect("save the thread");

    let read = store
        .load_thread(DEFAULT_PROJECT, &thread.id)
        .expect("read it back");
    assert!(read.heartbeat.is_none());
}
