//! A night of consolidation, end to end, with no server and no display.
//!
//! The unit tests pin each piece on its own: what a line parses to, what the
//! ledger counts, what the policy allows, what a reply parses to. This one
//! drives the path the application actually takes — a vault on disk, a
//! conversation on disk, a plan computed from both, a canned model reply, and
//! the files afterwards — because the interesting bugs live in the joins.
//!
//! The joins are also where this subsystem is most dangerous. Everything here
//! removes text from a person's notes while they are asleep, so the assertions
//! are as much about what *survived* as about what went.

use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use familiar::model::memory::dream::{self, Policy};
use familiar::model::memory::observation::Kind;
use familiar::model::memory::Memory;
use familiar::model::project::{Store, DEFAULT_PROJECT};
use familiar::model::thread::StoredTurn;

fn now() -> DateTime<Utc> {
    "2026-08-02T03:00:00Z".parse().expect("a date")
}

/// A vault with its usage ledger inside it, so nothing here can reach the real
/// one under `$XDG_DATA_HOME`.
fn vault(root: &Path) -> Memory {
    Memory::open_with(root, root.join(".ledger.json"))
}

fn saved(memory: &mut Memory, subject: &str, text: &str, kind: Kind, days_ago: i64) {
    memory
        .remember(subject, text, kind, None, now() - Duration::days(days_ago))
        .expect("remember");
}

/// Enough ordinary, plainly-alive observations that the night's budget — a
/// quarter of everything held — is not what a test is measuring. A quarter of
/// four is one and a quarter of two is none, which is right for a real memory
/// and makes a small fixture unpassable for a reason that has nothing to do
/// with the code under test.
fn padded(memory: &mut Memory) {
    for n in 0..16 {
        saved(
            memory,
            &format!("Padding{n}"),
            &format!("a recent and entirely unremarkable fact number {n}"),
            Kind::Fact,
            2,
        );
    }
}

/// Every observation the vault holds, as `Subject: text`.
fn held(memory: &Memory) -> Vec<String> {
    let mut lines: Vec<String> = memory
        .observations()
        .iter()
        .map(|observation| observation.line())
        .collect();
    lines.sort();
    lines
}

#[test]
fn a_night_collapses_a_duplicate_and_leaves_everything_else_where_it_was() {
    let directory = tempfile::tempdir().expect("temp dir");
    let mut memory = vault(directory.path());

    // A note of the user's own, with a line of theirs in it. Nothing tonight
    // may touch it.
    std::fs::write(
        directory.path().join("Roof.md"),
        "# Roof\n\nThe quotes are in the drawer.\n",
    )
    .expect("write");
    memory.rescan();

    saved(
        &mut memory,
        "Roof",
        "was replaced in April 2026",
        Kind::Fact,
        200,
    );
    // The second wording goes in by hand. `remember` refuses a duplicate now,
    // so the only way a vault holds one is that it predates that check or a
    // person pasted it — which is exactly the case the arithmetic pass exists
    // for, and it has to keep working on the vaults that have one.
    // In `Roof.md`, not `Familiar/Roof.md`: the vault already has a note for
    // this subject, so `remember` joined it rather than starting a rival copy.
    let path = directory.path().join("Roof.md");
    let note = std::fs::read_to_string(&path).expect("the note");
    std::fs::write(
        &path,
        format!(
            "{note}- Was replaced in April 2026. #familiar <!-- familiar fact 2026-04-24 -->\n"
        ),
    )
    .expect("write");
    memory.rescan();

    saved(
        &mut memory,
        "Matthew",
        "lives in Ashford, Ohio",
        Kind::Profile,
        900,
    );
    saved(
        &mut memory,
        "Matthew",
        "prefers small, single-purpose commits",
        Kind::Preference,
        400,
    );
    padded(&mut memory);

    let corpus = memory.held(&Default::default());
    let plan = dream::arithmetic(&corpus, now(), &Policy::default());
    let applied = memory.dream(&plan, now());

    assert_eq!(applied.merged, 1, "{applied:?}");
    assert_eq!(applied.failed, 0, "{applied:?}");
    let kept = held(&memory);
    assert!(
        kept.contains(&"Matthew: lives in Ashford, Ohio".to_string()),
        "{kept:?}"
    );
    assert!(
        kept.contains(&"Matthew: prefers small, single-purpose commits".to_string()),
        "{kept:?}"
    );
    // One Roof line, and it is the fuller wording.
    let roof: Vec<&String> = kept
        .iter()
        .filter(|line| line.starts_with("Roof:"))
        .collect();
    assert_eq!(roof, ["Roof: Was replaced in April 2026."], "{kept:?}");
    // And the user's own prose is byte-for-byte where it was.
    let note = std::fs::read_to_string(directory.path().join("Roof.md")).expect("the note");
    assert!(note.contains("The quotes are in the drawer."), "{note}");
}

#[test]
fn a_night_cannot_touch_a_line_the_user_wrote_however_the_plan_names_it() {
    // The rule the whole subsystem rests on, at the one place text is removed
    // unsupervised. A plan naming a sentence of the user's own fails rather
    // than acting, and says it failed.
    let directory = tempfile::tempdir().expect("temp dir");
    let mut memory = vault(directory.path());
    std::fs::write(
        directory.path().join("Rust.md"),
        "# Rust\n\n- moves are destructive\n- the borrow checker proves lifetimes\n",
    )
    .expect("write");
    memory.rescan();

    let plan = dream::Plan {
        operations: vec![dream::Operation::Drop {
            key: "Rust.md\u{1f}moves are destructive".into(),
            note: "Rust.md".into(),
            subject: "Rust".into(),
            text: "moves are destructive".into(),
            why: dream::Why::Trivial,
        }],
    };
    let applied = memory.dream(&plan, now());

    assert_eq!(applied.failed, 1);
    assert!(applied.dropped.is_empty());
    let note = std::fs::read_to_string(directory.path().join("Rust.md")).expect("the note");
    assert!(note.contains("- moves are destructive"), "{note}");
    assert!(
        note.contains("- the borrow checker proves lifetimes"),
        "{note}"
    );
}

#[test]
fn what_has_come_up_in_conversation_is_read_off_disk_and_keeps_a_memory_alive() {
    // The signal only a slow pass can gather. Nobody searched for this by name,
    // and it has been in the room all month — so decay alone would have taken
    // it and the whole point of running at night is that it does not.
    let directory = tempfile::tempdir().expect("temp dir");
    let mut memory = vault(directory.path());
    saved(
        &mut memory,
        "Cogsworth",
        "is the WebSocket transport between Familiar and the model",
        Kind::Fact,
        300,
    );
    saved(
        &mut memory,
        "Kubernetes",
        "was never taken up",
        Kind::Fact,
        300,
    );
    padded(&mut memory);

    let threads = tempfile::tempdir().expect("temp dir");
    let store = Store::new(threads.path());
    let mut thread = store.new_thread(DEFAULT_PROJECT).expect("a thread");
    thread.push_turn(StoredTurn {
        at: Some(Utc::now()),
        user: "How is Cogsworth coming along?".into(),
        answer: "The transport is nearly done.".into(),
        ..Default::default()
    });
    store.save_thread(DEFAULT_PROJECT, &thread).expect("save");

    let transcripts = store.recent_conversations(Utc::now() - Duration::days(60), 200);
    let mentions = dream::mentions(memory.observations(), &transcripts);
    let corpus = memory.held(&mentions);
    let applied = memory.dream(
        &dream::arithmetic(&corpus, now(), &Policy::default()),
        now(),
    );

    assert_eq!(applied.dropped.len(), 1, "{applied:?}");
    assert_eq!(applied.dropped[0].subject, "Kubernetes");
    let kept = held(&memory);
    assert!(
        kept.contains(
            &"Cogsworth: is the WebSocket transport between Familiar and the model".to_string()
        ),
        "{kept:?}"
    );
}

#[test]
fn refiling_a_misfiled_preference_puts_it_in_front_of_the_model() {
    // The most useful operation of the night, and the one with nothing to lose
    // by it: the sentence and its date are unchanged, and it moves from the
    // half of memory that has to be looked up to the half that always rides.
    let directory = tempfile::tempdir().expect("temp dir");
    let mut memory = vault(directory.path());
    saved(
        &mut memory,
        "Matthew",
        "wants every measurement in metric, never imperial",
        Kind::Fact,
        50,
    );
    assert!(
        memory.ranked(now()).0.is_empty(),
        "it should not be core yet"
    );

    let corpus = memory.held(&Default::default());
    let reply = r#"{"reclassify":[{"id":0,"kind":"preference"}]}"#;
    let plan = dream::parse(reply, &corpus).bounded(&corpus, &Policy::default(), now());
    let applied = memory.dream(&plan, now());

    assert_eq!(applied.reclassified, 1, "{applied:?}");
    assert!(applied.dropped.is_empty(), "{applied:?}");

    let refiled = &memory.observations()[0];
    assert_eq!(refiled.kind, Kind::Preference);
    assert_eq!(
        refiled.text,
        "wants every measurement in metric, never imperial"
    );
    // The date survives: refiling is not saving again, and resetting the clock
    // would make every reclassified observation immortal.
    assert_eq!(
        refiled.saved,
        Some((now() - Duration::days(50)).date_naive())
    );

    let block = memory.ambient(now()).expect("a block");
    assert!(block.contains("About the user"), "{block}");
    assert!(block.contains("metric"), "{block}");
}

#[test]
fn a_model_that_answers_drop_them_all_costs_a_fraction_rather_than_the_lot() {
    // The rails, together, on the answer they exist for.
    let directory = tempfile::tempdir().expect("temp dir");
    let mut memory = vault(directory.path());
    for n in 0..12 {
        saved(
            &mut memory,
            &format!("Subject{n}"),
            &format!("an observation about the {n}th thing"),
            if n < 3 { Kind::Profile } else { Kind::Fact },
            300,
        );
    }

    let corpus = memory.held(&Default::default());
    let everything: Vec<String> = (0..corpus.len())
        .map(|id| format!(r#"{{"id":{id},"why":"stale"}}"#))
        .collect();
    let reply = format!(r#"{{"drop":[{}]}}"#, everything.join(","));
    let plan = dream::parse(&reply, &corpus).bounded(&corpus, &Policy::default(), now());
    let applied = memory.dream(&plan, now());

    // A quarter of twelve, and not one of the profile facts among them.
    assert_eq!(applied.dropped.len(), 3, "{applied:?}");
    assert_eq!(memory.observations().len(), 9);
    assert_eq!(
        memory
            .observations()
            .iter()
            .filter(|held| held.kind == Kind::Profile)
            .count(),
        3,
        "who someone is does not expire"
    );
}

#[test]
fn the_ambient_block_stays_the_same_size_however_much_is_remembered() {
    // The property the budget exists for. Six months of use has to produce the
    // same prompt as six days, or the running cost of remembering grows without
    // anything ever deciding that it should.
    let directory = tempfile::tempdir().expect("temp dir");
    let mut memory = vault(directory.path());

    saved(
        &mut memory,
        "Matthew",
        "lives in Ashford, Ohio",
        Kind::Profile,
        10,
    );
    let small = memory.ambient(now()).expect("a block").chars().count();

    for n in 0..300 {
        saved(
            &mut memory,
            &format!("Subject{n}"),
            &format!("a durable and reasonably wordy observation about the {n}th thing"),
            if n % 3 == 0 {
                Kind::Preference
            } else {
                Kind::Fact
            },
            n as i64 % 40,
        );
    }
    let large = memory.ambient(now()).expect("a block").chars().count();

    assert!(large >= small);
    assert!(
        large < small + familiar::model::memory::ambient::BUDGET,
        "the block grew to {large} characters"
    );
}

#[test]
fn a_night_that_finds_nothing_to_do_writes_nothing() {
    let directory = tempfile::tempdir().expect("temp dir");
    let mut memory = vault(directory.path());
    saved(
        &mut memory,
        "Matthew",
        "lives in Ashford",
        Kind::Profile,
        10,
    );
    saved(
        &mut memory,
        "Roof",
        "was replaced in April 2026",
        Kind::Fact,
        10,
    );

    let before = std::fs::read_to_string(directory.path().join("Familiar/Roof.md")).expect("note");
    let corpus = memory.held(&Default::default());
    let applied = memory.dream(
        &dream::arithmetic(&corpus, now(), &Policy::default()),
        now(),
    );

    assert!(applied.is_quiet());
    assert_eq!(applied.describe(), None);
    let after = std::fs::read_to_string(directory.path().join("Familiar/Roof.md")).expect("note");
    assert_eq!(before, after);
}
