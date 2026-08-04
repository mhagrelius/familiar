//! One thing the assistant saved, and what it is worth now.
//!
//! Every line Familiar writes into a note is one of these. The line is still
//! Markdown a person can read and delete — that has not changed — but it now
//! carries a **kind** in the HTML comment that already held the date, and the
//! kind is what the rest of the memory subsystem reasons with.
//!
//! Why a kind at all: the three questions the system has to answer are "does
//! this ride in every prompt", "when does this go stale", and "is this still
//! worth keeping". A flat list of sentences answers none of them. Assistant
//! memory elsewhere splits the same way — MemGPT's core versus archival blocks,
//! ChatGPT's saved memories versus referenced history — and the split is always
//! between *what the assistant needs in front of it* and *what it can go and
//! look up*. [`Kind`] is that split, made explicit and given a half-life.
//!
//! The comment rather than a tag, because `#familiar/preference` would render
//! in the user's note as visible clutter and the comment is already invisible.
//! Lines written before kinds existed parse as [`Kind::Fact`], which is the
//! honest reading of an unlabelled observation and needs no migration.

use chrono::{DateTime, NaiveDate, Utc};

/// What sort of thing was saved, and therefore how long it stays true.
///
/// Ordered from most durable to least, which is also the order they claim space
/// in the ambient block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// Who the user is: name, where they live, what they do. Changes about as
    /// often as a passport does.
    Profile,
    /// How they want things done — a standing instruction, a preference, a
    /// thing they have asked for twice. Durable, and the most valuable thing an
    /// assistant can hold, because it changes every answer rather than one.
    Preference,
    /// What they are working on. True for a season and then quietly not.
    Project,
    /// Everything else worth keeping about a subject. The default, and the one
    /// with the shortest half-life — a fact nobody has needed in two months is
    /// the commonest thing in a neglected memory.
    Fact,
}

impl Kind {
    /// How it is written in the note's comment, and read back.
    pub fn label(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::Preference => "preference",
            Self::Project => "project",
            Self::Fact => "fact",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_lowercase().as_str() {
            "profile" | "identity" => Some(Self::Profile),
            "preference" | "instruction" => Some(Self::Preference),
            "project" | "work" => Some(Self::Project),
            "fact" | "observation" => Some(Self::Fact),
            _ => None,
        }
    }

    /// Every kind, most durable first. The order the ambient block fills in.
    pub fn all() -> [Self; 4] {
        [Self::Profile, Self::Preference, Self::Project, Self::Fact]
    }

    /// How much this kind is worth before age is taken off it.
    pub fn weight(self) -> f32 {
        match self {
            Self::Profile => 1.0,
            Self::Preference => 0.9,
            Self::Project => 0.7,
            Self::Fact => 0.5,
        }
    }

    /// Days after which an unused observation of this kind is worth half what
    /// it was. Profile is effectively exempt: a decade is longer than this
    /// application will be running, which is the honest way to say "never".
    pub fn half_life_days(self) -> f32 {
        match self {
            Self::Profile => 3650.0,
            Self::Preference => 365.0,
            Self::Project => 90.0,
            Self::Fact => 45.0,
        }
    }

    /// Whether this kind belongs in the prompt whether or not it was asked for.
    ///
    /// The line between core and archival memory. A preference the assistant has
    /// to `recall` before honouring is a preference it will not honour, because
    /// nothing in the turn tells it to go looking. A fact about the roof is
    /// different: the question that needs it says "roof" in it.
    pub fn is_core(self) -> bool {
        matches!(self, Self::Profile | Self::Preference)
    }
}

/// One saved line, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// The note it lives in, vault-relative.
    pub note: String,
    /// The note's title, which is the subject as the user would say it.
    pub subject: String,
    /// The sentence, without the mark, the tag or the comment.
    pub text: String,
    pub kind: Kind,
    /// The day it was written, if the line still says.
    pub saved: Option<NaiveDate>,
}

impl Observation {
    /// A stable name for this line, so a usage ledger kept outside the vault can
    /// find it again after the note has been edited around it.
    ///
    /// The note path and the text, normalised. Not a hash: a ledger a person can
    /// open and read is worth more than a few bytes, and this is the only place
    /// the two halves are tied together.
    pub fn key(&self) -> String {
        format!("{}\u{1f}{}", self.note, normalise(&self.text))
    }

    /// How old it is in days, or `None` when the line carries no date.
    pub fn age_days(&self, now: DateTime<Utc>) -> Option<f32> {
        let saved = self.saved?;
        let days = (now.date_naive() - saved).num_days();
        Some(days.max(0) as f32)
    }

    /// What this is worth today, given how often it has been reached for.
    ///
    /// Three terms, and each is doing separate work:
    ///
    /// * the **kind's weight**, because a standing preference is worth more per
    ///   character of prompt than a fact about a roof;
    /// * **decay**, halving every [`Kind::half_life_days`], because the thing
    ///   that makes a memory unwieldy is not what was saved this week;
    /// * **use**, damped by a logarithm, because something reached for twice is
    ///   clearly worth keeping and something reached for forty times is not
    ///   twenty times more so.
    ///
    /// An undated line does not decay. It is old enough that the format
    /// predates dates, and guessing an age for it would silently delete the
    /// oldest things in the vault — which are the ones most likely to be
    /// profile facts.
    pub fn score(&self, uses: u32, now: DateTime<Utc>) -> f32 {
        let decay = match self.age_days(now) {
            Some(age) => 0.5f32.powf(age / self.kind.half_life_days()),
            None => 1.0,
        };
        self.kind.weight() * decay * (1.0 + (uses as f32).ln_1p())
    }

    /// How it reads in the ambient block: the subject, then the sentence.
    pub fn line(&self) -> String {
        format!("{}: {}", self.subject, self.text)
    }
}

/// Text as two observations that say the same thing would both write it.
///
/// Case, punctuation and runs of whitespace go; word order does not. Enough to
/// catch the same sentence saved twice — which is what happens when a fact comes
/// up again three weeks later — without pretending to catch a paraphrase, which
/// is a judgement only the model can make and [`super::dream`] asks it to.
pub fn normalise(text: &str) -> String {
    let folded: String = text
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .to_lowercase();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

// -- the line format ----------------------------------------------------------

/// Every line the assistant writes starts with this, so `forget` can tell its
/// own work from yours without keeping a second record of what it did.
pub const MARK: &str = "- ";

/// And carries this, so the same is true when you are reading the file.
pub const TAG: &str = "#familiar";

/// What opens the trailing comment in the current format.
const STAMP: &str = "familiar";

/// Render one line, ready to append under the heading.
pub fn render(text: &str, kind: Kind, related: Option<&str>, now: DateTime<Utc>) -> String {
    let mut line = format!("{MARK}{}", text.trim());
    if let Some(related) = related.map(str::trim).filter(|r| !r.is_empty()) {
        // A wikilink to a note that does not exist yet is still a relation:
        // Brain reports it as unresolved rather than losing it.
        line.push_str(&format!(" — see [[{related}]]"));
    }
    line.push_str(&format!(
        " {TAG} <!-- {STAMP} {} {} -->",
        kind.label(),
        now.format("%Y-%m-%d")
    ));
    line
}

/// Whether this line is one Familiar wrote.
pub fn is_familiars(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with(MARK) && trimmed.contains(TAG)
}

/// The observation without its marking.
pub fn text_of(line: &str) -> String {
    let line = line.trim_start().trim_start_matches(MARK);
    let line = match line.find("<!--") {
        Some(at) => &line[..at],
        None => line,
    };
    line.replace(TAG, "").trim().to_string()
}

/// What the trailing comment says: the kind, and the day it was written.
///
/// Three shapes have to read, and only one of them was ever written by a version
/// that knew about kinds:
///
/// * `<!-- familiar preference 2026-08-02 -->` — current.
/// * `<!-- 2026-08-01 -->` — what shipped first. A [`Kind::Fact`], dated.
/// * anything else — neither, which is what a line a person hand-edited looks
///   like. Undated and unclassified rather than discarded.
pub fn stamp_of(line: &str) -> (Kind, Option<NaiveDate>) {
    let Some(comment) = comment_of(line) else {
        return (Kind::Fact, None);
    };
    let mut words = comment.split_whitespace().peekable();
    if words.peek() == Some(&STAMP) {
        words.next();
    }
    let mut kind = Kind::Fact;
    let mut date = None;
    for word in words {
        if let Some(parsed) = Kind::parse(word) {
            kind = parsed;
        } else if let Ok(parsed) = NaiveDate::parse_from_str(word, "%Y-%m-%d") {
            date = Some(parsed);
        }
    }
    (kind, date)
}

/// The same line, filed under a different kind.
///
/// Text and date are kept exactly. Refiling is not saving again, and resetting
/// the date would make every reclassified observation look brand new and
/// therefore immortal — which would turn the safest operation the dream has into
/// a way of quietly exempting things from ever expiring.
pub fn restamp(line: &str, kind: Kind) -> String {
    let (_, saved) = stamp_of(line);
    let body = match line.find("<!--") {
        Some(at) => line[..at].trim_end(),
        None => line.trim_end(),
    };
    match saved {
        Some(saved) => format!("{body} <!-- {STAMP} {} {saved} -->", kind.label()),
        None => format!("{body} <!-- {STAMP} {} -->", kind.label()),
    }
}

fn comment_of(line: &str) -> Option<&str> {
    let start = line.find("<!--")? + 4;
    let end = line[start..].find("-->")? + start;
    Some(line[start..end].trim())
}

/// Parse a line into an observation, given the note it was found in.
pub fn parse(note: &str, subject: &str, line: &str) -> Option<Observation> {
    if !is_familiars(line) {
        return None;
    }
    let text = text_of(line);
    if text.is_empty() {
        return None;
    }
    let (kind, saved) = stamp_of(line);
    Some(Observation {
        note: note.to_string(),
        subject: subject.to_string(),
        text,
        kind,
        saved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-08-02T09:00:00Z".parse().expect("date")
    }

    fn observation(kind: Kind, saved: &str) -> Observation {
        Observation {
            note: "Familiar/Matthew.md".into(),
            subject: "Matthew".into(),
            text: "prefers small commits".into(),
            kind,
            saved: Some(saved.parse().expect("date")),
        }
    }

    #[test]
    fn a_rendered_line_round_trips_through_the_parser() {
        let line = render("prefers small commits", Kind::Preference, None, now());
        let parsed = parse("Familiar/Matthew.md", "Matthew", &line).expect("an observation");
        assert_eq!(parsed.text, "prefers small commits");
        assert_eq!(parsed.kind, Kind::Preference);
        assert_eq!(parsed.saved, Some("2026-08-02".parse().expect("date")));
    }

    #[test]
    fn a_relation_survives_the_round_trip_as_a_wikilink() {
        let line = render("is built on GTK", Kind::Fact, Some("Brain"), now());
        assert!(line.contains("[[Brain]]"), "{line}");
        let parsed = parse("Familiar/Familiar.md", "Familiar", &line).expect("an observation");
        assert!(parsed.text.contains("[[Brain]]"), "{parsed:?}");
    }

    #[test]
    fn a_line_written_before_kinds_existed_still_reads() {
        // The format that shipped first. No migration runs over the vault, so
        // this has to keep parsing for as long as anyone's notes hold one.
        let old = "- writes Rust for GNOME #familiar <!-- 2026-07-14 -->";
        let parsed = parse("Familiar/Matthew.md", "Matthew", old).expect("an observation");
        assert_eq!(parsed.text, "writes Rust for GNOME");
        assert_eq!(parsed.kind, Kind::Fact);
        assert_eq!(parsed.saved, Some("2026-07-14".parse().expect("date")));
    }

    #[test]
    fn a_line_a_person_edited_the_comment_off_is_kept_rather_than_dropped() {
        let edited = "- writes Rust for GNOME #familiar";
        let parsed = parse("Familiar/Matthew.md", "Matthew", edited).expect("an observation");
        assert_eq!(parsed.kind, Kind::Fact);
        assert_eq!(parsed.saved, None);
    }

    #[test]
    fn a_line_familiar_did_not_write_is_not_one_of_ours() {
        assert!(parse("Rust.md", "Rust", "- moves are destructive").is_none());
        assert!(parse("Rust.md", "Rust", "Some prose #familiar").is_none());
    }

    #[test]
    fn an_older_observation_is_worth_less_than_a_new_one_of_the_same_kind() {
        let fresh = observation(Kind::Fact, "2026-08-01");
        let stale = observation(Kind::Fact, "2026-05-01");
        assert!(fresh.score(0, now()) > stale.score(0, now()));
        // And 93 days is about two half-lives, so it should be down near a
        // quarter rather than nudged.
        assert!(stale.score(0, now()) < fresh.score(0, now()) / 3.0);
    }

    #[test]
    fn a_profile_fact_barely_decays_and_a_loose_fact_does() {
        let old = "2025-08-02";
        let profile = Observation {
            kind: Kind::Profile,
            ..observation(Kind::Profile, old)
        };
        let fact = observation(Kind::Fact, old);
        // A year on, the profile fact is still nearly its full weight and the
        // loose fact is nearly gone.
        assert!(profile.score(0, now()) > 0.9);
        assert!(fact.score(0, now()) < 0.05);
    }

    #[test]
    fn being_reached_for_keeps_something_alive_without_letting_it_run_away() {
        let stale = observation(Kind::Fact, "2026-05-01");
        let unused = stale.score(0, now());
        let used_twice = stale.score(2, now());
        let used_forty = stale.score(40, now());
        assert!(used_twice > unused * 1.5, "use has to matter");
        // Damped: forty uses is not twenty times two uses.
        assert!(
            used_forty < used_twice * 3.0,
            "{used_forty} vs {used_twice}"
        );
    }

    #[test]
    fn an_undated_line_does_not_decay_to_nothing() {
        // The oldest lines in a vault are the ones with no date, and they are
        // disproportionately profile facts. Guessing an age for them would
        // quietly delete exactly the wrong things.
        let undated = Observation {
            saved: None,
            ..observation(Kind::Fact, "2026-01-01")
        };
        assert_eq!(undated.score(0, now()), Kind::Fact.weight());
    }

    #[test]
    fn a_core_kind_is_one_that_has_to_be_in_front_of_the_model() {
        // A preference it would have to look up is a preference it will not
        // honour: nothing in the turn tells it to go looking.
        assert!(Kind::Profile.is_core());
        assert!(Kind::Preference.is_core());
        assert!(!Kind::Project.is_core());
        assert!(!Kind::Fact.is_core());
    }

    #[test]
    fn the_same_sentence_saved_twice_has_the_same_key() {
        let one = Observation {
            text: "Prefers small, single-purpose commits.".into(),
            ..observation(Kind::Preference, "2026-08-01")
        };
        let two = Observation {
            text: "prefers small single purpose commits".into(),
            ..observation(Kind::Preference, "2026-06-01")
        };
        assert_eq!(one.key(), two.key());
    }

    #[test]
    fn two_observations_in_different_notes_are_different_keys() {
        let one = observation(Kind::Fact, "2026-08-01");
        let two = Observation {
            note: "Familiar/Rust.md".into(),
            ..one.clone()
        };
        assert_ne!(one.key(), two.key());
    }

    #[test]
    fn refiling_a_line_keeps_its_text_and_the_day_it_was_saved() {
        // Resetting the date would make every reclassified observation look
        // brand new and therefore immortal, which would turn the safest
        // operation the dream has into a way of exempting things from expiry.
        let line = render("prefers metric", Kind::Fact, None, now());
        let refiled = restamp(&line, Kind::Preference);
        let parsed = parse("Familiar/Matthew.md", "Matthew", &refiled).expect("an observation");
        assert_eq!(parsed.kind, Kind::Preference);
        assert_eq!(parsed.text, "prefers metric");
        assert_eq!(parsed.saved, Some("2026-08-02".parse().expect("date")));
    }

    #[test]
    fn refiling_a_line_written_before_kinds_existed_keeps_its_date_too() {
        let old = "- writes Rust for GNOME #familiar <!-- 2026-07-14 -->";
        let parsed = parse("n.md", "n", &restamp(old, Kind::Profile)).expect("an observation");
        assert_eq!(parsed.kind, Kind::Profile);
        assert_eq!(parsed.saved, Some("2026-07-14".parse().expect("date")));
    }

    #[test]
    fn refiling_an_undated_line_leaves_it_undated() {
        let bare = "- writes Rust for GNOME #familiar";
        let parsed = parse("n.md", "n", &restamp(bare, Kind::Profile)).expect("an observation");
        assert_eq!(parsed.kind, Kind::Profile);
        assert_eq!(parsed.saved, None);
    }

    #[test]
    fn every_kind_reads_back_from_what_it_writes() {
        for kind in Kind::all() {
            assert_eq!(Kind::parse(kind.label()), Some(kind));
        }
        assert_eq!(Kind::parse("nonsense"), None);
    }
}
