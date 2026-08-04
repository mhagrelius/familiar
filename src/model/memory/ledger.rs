//! What has actually been reached for, and when.
//!
//! The vault holds the memories; this holds the evidence about them. Two
//! separate things, and the reason they are separate files is that one of them
//! is yours: an observation that reads `- prefers small commits #familiar` is a
//! sentence in your notes, and `uses=7 last=2026-07-30` written beside it would
//! be telemetry in your notes. So the counting lives under Familiar's own data
//! directory and keys back into the vault by [`Observation::key`].
//!
//! [`super::dream`] is the consumer. Deciding what to pare needs to know what
//! survived contact with a real conversation, and "saved three weeks ago and
//! never looked at since" is the single most useful fact about a memory that is
//! not in the memory itself.
//!
//! **Only retrieval counts.** A `recall` that returned a note is an unambiguous
//! signal that something was wanted. Riding along in the ambient block is not —
//! it happens to everything in the block on every turn, so counting it would
//! make the number a clock. The fuzzier signal, whether a subject came up in
//! conversation at all, is left to the dream, which has the time to read
//! transcripts and the licence to be approximate.
//!
//! [`Observation::key`]: super::observation::Observation::key

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

/// How often one observation has been reached for, and when it last was.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Use {
    #[serde(default)]
    pub uses: u32,
    #[serde(default)]
    pub last: Option<NaiveDate>,
}

/// The ledger, as it sits on disk and in memory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    entries: BTreeMap<String, Use>,
    /// Set when something changed and not yet written. Not persisted — a loaded
    /// ledger is by definition level with its file.
    #[serde(skip)]
    dirty: bool,
}

impl Ledger {
    /// `$XDG_DATA_HOME/familiar/memory-use.json`, beside the contexts.
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("familiar/memory-use.json")
    }

    /// Read it, or start an empty one.
    ///
    /// A file that will not parse is an empty ledger rather than an error. The
    /// worst case is that everything looks unused for a while and the dream is
    /// conservative about it — which is the direction to fail in.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, text)?;
        std::fs::rename(&temporary, path)
    }

    /// Whether anything has changed since it was loaded or last written.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many times this has been reached for.
    pub fn uses(&self, key: &str) -> u32 {
        self.entries.get(key).map_or(0, |used| used.uses)
    }

    pub fn last_used(&self, key: &str) -> Option<NaiveDate> {
        self.entries.get(key).and_then(|used| used.last)
    }

    /// Record that these were retrieved. One call per `recall`, so a search that
    /// returned five notes credits all five once rather than one of them five
    /// times.
    pub fn used(&mut self, keys: impl IntoIterator<Item = String>, now: DateTime<Utc>) {
        let day = now.date_naive();
        for key in keys {
            let entry = self.entries.entry(key).or_default();
            entry.uses = entry.uses.saturating_add(1);
            entry.last = Some(day);
            self.dirty = true;
        }
    }

    /// Forget the bookkeeping for observations that are no longer in the vault.
    ///
    /// Called by the dream after it applies a plan, and after a rescan. Without
    /// it the file grows for ever with keys naming lines a person deleted by
    /// hand months ago.
    pub fn retain(&mut self, present: &std::collections::BTreeSet<String>) -> usize {
        let before = self.entries.len();
        self.entries.retain(|key, _| present.contains(key));
        let dropped = before - self.entries.len();
        if dropped > 0 {
            self.dirty = true;
        }
        dropped
    }

    /// Carry the counts across when an observation's text was rewritten — which
    /// is what a merge in [`super::dream`] does. The new line inherits the sum
    /// of what the old ones had earned, because the thing they recorded is that
    /// somebody wanted this, and that is still true of the sentence that
    /// replaced them.
    pub fn merge_into(&mut self, from: &[String], to: &str) {
        let mut carried = Use::default();
        for key in from {
            if let Some(used) = self.entries.remove(key) {
                carried.uses = carried.uses.saturating_add(used.uses);
                carried.last = carried.last.max(used.last);
            }
        }
        if carried == Use::default() {
            return;
        }
        let entry = self.entries.entry(to.to_string()).or_default();
        entry.uses = entry.uses.saturating_add(carried.uses);
        entry.last = entry.last.max(carried.last);
        self.dirty = true;
    }

    /// Mark it written. Called after a successful [`Ledger::save`].
    pub fn settled(&mut self) {
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn now() -> DateTime<Utc> {
        "2026-08-02T09:00:00Z".parse().expect("date")
    }

    #[test]
    fn a_missing_file_is_an_empty_ledger_rather_than_an_error() {
        let directory = tempfile::tempdir().expect("temp dir");
        let ledger = Ledger::load(&directory.path().join("nothing.json"));
        assert!(ledger.is_empty());
        assert!(!ledger.is_dirty());
    }

    #[test]
    fn a_corrupt_file_is_an_empty_ledger_rather_than_an_error() {
        // Failing here would take the whole memory subsystem down over a file
        // whose only content is a count. Everything looks unused for a while
        // and the dream is conservative, which is the direction to fail in.
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("memory-use.json");
        std::fs::write(&path, "{not json").expect("write");
        assert!(Ledger::load(&path).is_empty());
    }

    #[test]
    fn a_recall_credits_every_note_it_returned_once() {
        let mut ledger = Ledger::default();
        ledger.used(["a".to_string(), "b".to_string()], now());
        assert_eq!(ledger.uses("a"), 1);
        assert_eq!(ledger.uses("b"), 1);
        assert_eq!(ledger.uses("c"), 0);
        assert_eq!(ledger.last_used("a"), Some("2026-08-02".parse().unwrap()));
        assert!(ledger.is_dirty());
    }

    #[test]
    fn it_round_trips_through_its_file() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("familiar/memory-use.json");
        let mut ledger = Ledger::default();
        ledger.used(["a".to_string()], now());
        ledger.save(&path).expect("save");

        let read = Ledger::load(&path);
        assert_eq!(read.uses("a"), 1);
        assert!(!read.is_dirty(), "a loaded ledger is level with its file");
    }

    #[test]
    fn keys_for_lines_that_are_gone_are_dropped() {
        let mut ledger = Ledger::default();
        ledger.used(["kept".to_string(), "gone".to_string()], now());
        let present: BTreeSet<String> = ["kept".to_string()].into_iter().collect();
        assert_eq!(ledger.retain(&present), 1);
        assert_eq!(ledger.uses("gone"), 0);
        assert_eq!(ledger.uses("kept"), 1);
    }

    #[test]
    fn a_merge_carries_what_the_old_lines_had_earned() {
        // The dream rewrites three sentences into one. The evidence that
        // somebody wanted this is still true of the sentence that replaced
        // them, so throwing the counts away would make a merged memory look
        // brand new and unwanted.
        let mut ledger = Ledger::default();
        ledger.used(["one".to_string()], now());
        ledger.used(["one".to_string(), "two".to_string()], now());
        ledger.merge_into(&["one".to_string(), "two".to_string()], "merged");

        assert_eq!(ledger.uses("merged"), 3);
        assert_eq!(ledger.uses("one"), 0);
        assert_eq!(
            ledger.last_used("merged"),
            Some("2026-08-02".parse().unwrap())
        );
    }

    #[test]
    fn merging_lines_nobody_ever_used_leaves_no_entry_behind() {
        let mut ledger = Ledger::default();
        ledger.merge_into(&["one".to_string()], "merged");
        assert!(ledger.is_empty());
        assert!(!ledger.is_dirty());
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("memory-use.json");
        let mut ledger = Ledger::default();
        ledger.used(["a".to_string()], now());
        ledger.save(&path).expect("save");

        let leftovers: Vec<String> = std::fs::read_dir(directory.path())
            .expect("read dir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
