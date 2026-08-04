//! Memory is Brain's vault.
//!
//! Familiar does not build a knowledge graph. Brain already owns one — notes
//! are entities, `[[wikilinks]]` are relations, `#tags` are types — and it
//! stores it as Markdown you can read, edit and delete without either app. So
//! `remember` appends a line to a note, `recall` is Brain's hybrid search, and
//! salience is what the vault and the ledger between them actually know rather
//! than a score invented to approximate it.
//!
//! Three rules hold everywhere in this file, and they are why the assistant is
//! safe to let near your notes:
//!
//! 1. **It only ever appends.** A note it did not create keeps everything that
//!    was already in it.
//! 2. **It only removes what it wrote.** Every line it adds is marked, and
//!    `forget` — and the dream, which is `forget` running at three in the
//!    morning — will not touch an unmarked one.
//! 3. **A note is never deleted.** Emptying the observations leaves the note.
//!
//! # Where things go
//!
//! The protocol matters because a vault is not only Familiar's. Brain writes
//! notes, and so do you, and the assistant has to be able to say which lines are
//! its own without keeping a shadow copy of the vault to compare against.
//!
//! | What | Where | Whose |
//! |---|---|---|
//! | An observation about a subject you already have a note for | that note, under [`HEADING`] | yours; Familiar's lines are marked inside it |
//! | An observation about a subject with no note anywhere | `Familiar/<Subject>.md` | Familiar's, until you edit it |
//! | What has been reached for, and when | `~/.local/share/familiar/memory-use.json` | Familiar's, and not a note |
//! | Vectors | Brain's cache, shared | derived, rebuildable |
//!
//! Writes are scoped and reads are not. `recall` runs over the whole vault and
//! always has, so the folder changes where new files land and nothing about what
//! can be found. The assistant joins your notes; it only keeps its own
//! foundlings together.

pub mod ambient;
pub mod dream;
pub mod harvest;
pub mod ledger;
pub mod observation;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use brain::model::bm25::Bm25;
use brain::model::index::Index;
use brain::model::note::{Note, NoteId};
use brain::model::search;
use brain::model::semantic::Store;
use brain::model::vault::{Vault, VaultError};
use chrono::{DateTime, Utc};

use ambient::Ranked;
use ledger::Ledger;
use observation::{Kind, Observation};

pub use observation::{is_familiars, MARK, TAG};

/// The heading the observations live under.
pub const HEADING: &str = "## Noted by Familiar";

/// Where a note the assistant had to *create* goes.
pub const FOLDER: &str = "Familiar";

/// How many of the vault's own notes the ambient block may name.
const BACKGROUND: usize = ambient::BACKGROUND_LINES;

/// One note, as something that turns text into vectors needs to see it.
///
/// Here rather than in `ui::embedder` because it crosses the line between them
/// and this half is the one that owns the vault. It is deliberately owned data:
/// it goes to a worker thread, and nothing borrowed can.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passage {
    pub id: NoteId,
    pub title: String,
    pub text: String,
}

/// What `recall` found.
#[derive(Debug, Clone, PartialEq)]
pub struct Recalled {
    pub title: String,
    /// The vault-relative path, so a follow-up can name it exactly.
    pub path: String,
    /// The note in the user's own words.
    pub excerpt: String,
    /// What Familiar has saved there, which is usually the answer to the
    /// question that prompted the recall.
    pub observations: Vec<String>,
    /// Whether the words were there, or only the meaning. Carried through
    /// because "both halves agreed" and "only the vectors liked this" are
    /// different degrees of confidence, and a caller that cannot tell them apart
    /// has to treat every hit the same.
    pub lexical: bool,
    pub semantic: bool,
}

/// The vault, as the assistant sees it.
pub struct Memory {
    vault: Vault,
    index: Index,
    /// The lexical half of `recall`, rebuilt with the index.
    lexical: Bm25,
    /// The semantic half. `None` until a store has been loaded, and absent
    /// stays absent — searching degrades to BM25 rather than failing, which is
    /// the normal state on a machine with no embedding server.
    semantic: Option<Store>,
    store_path: PathBuf,
    /// Everything Familiar has written into this vault, parsed once per scan.
    observations: Vec<Observation>,
    /// Every note's title and body, kept from the scan that read them.
    ///
    /// Brain's `Index` holds this too and keeps it to itself, and re-reading
    /// the vault to hand it to the embedder would be a second full pass over
    /// every file for text that was in hand a moment ago.
    passages: Vec<Passage>,
    ledger: Ledger,
    ledger_path: PathBuf,
}

#[derive(Debug)]
pub enum MemoryError {
    Vault(VaultError),
    /// The note exists but is not one Familiar may edit.
    Refused(String),
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Vault(source) => write!(f, "{source}"),
            Self::Refused(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for MemoryError {}

impl Memory {
    /// Open a vault and index it. Brain measured this: a thousand notes index
    /// in about 29 ms, so it is done up front rather than cached.
    pub fn open(root: &Path) -> Self {
        Self::open_with(root, Ledger::default_path())
    }

    /// The same, with the usage ledger somewhere else. The tests use this so a
    /// run never touches the real one.
    pub fn open_with(root: &Path, ledger_path: PathBuf) -> Self {
        let vault = Vault::new(root);
        let store_path = brain::model::semantic::default_store_path(root);
        let semantic = Some(Store::load(&store_path)).filter(|store| !store.is_empty());
        let empty = Index::build(&[]);
        let mut memory = Self {
            vault,
            lexical: Bm25::build(&empty),
            index: empty,
            semantic,
            store_path,
            observations: Vec::new(),
            passages: Vec::new(),
            ledger: Ledger::load(&ledger_path),
            ledger_path,
        };
        memory.rescan();
        memory
    }

    /// Re-read the vault. Called when the file monitor says something changed,
    /// including changes Brain made.
    pub fn rescan(&mut self) {
        let (notes, _) = self.vault.scan();
        self.index = Index::build(&notes);
        self.lexical = Bm25::build(&self.index);
        self.passages = notes
            .iter()
            .map(|note| Passage {
                id: note.id.clone(),
                title: note.id.title().to_string(),
                text: note.to_text(),
            })
            .collect();
        self.observations = notes
            .iter()
            .flat_map(|note| {
                let path = note.id.as_str().to_string();
                let title = note.id.title().to_string();
                note.to_text()
                    .lines()
                    .filter_map(|line| observation::parse(&path, &title, line))
                    .collect::<Vec<_>>()
            })
            .collect();
        // Bookkeeping about lines that no longer exist is bookkeeping about
        // nothing. Left in, the file grows for ever with keys naming sentences
        // a person deleted by hand months ago.
        let present: BTreeSet<String> = self.observations.iter().map(Observation::key).collect();
        self.ledger.retain(&present);
    }

    pub fn root(&self) -> &Path {
        self.vault.root()
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    /// Everything Familiar has written into this vault.
    pub fn observations(&self) -> &[Observation] {
        &self.observations
    }

    /// Every note, as the embedder needs to see it.
    pub fn passages(&self) -> &[Passage] {
        &self.passages
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn ledger_mut(&mut self) -> &mut Ledger {
        &mut self.ledger
    }

    /// Write the ledger out if anything changed. Cheap and idempotent, so the
    /// caller can do it at any convenient boundary.
    pub fn flush_ledger(&mut self) {
        if !self.ledger.is_dirty() {
            return;
        }
        if self.ledger.save(&self.ledger_path).is_ok() {
            self.ledger.settled();
        }
    }

    pub fn store_path(&self) -> &Path {
        &self.store_path
    }

    pub fn semantic(&self) -> Option<&Store> {
        self.semantic.as_ref()
    }

    /// Take the vectors on, after a catch-up pass on a worker thread brought
    /// them level with the vault.
    pub fn set_semantic(&mut self, store: Store) {
        self.semantic = (!store.is_empty()).then_some(store);
    }

    /// Append an observation to the note for `subject`, creating the note if it
    /// does not exist.
    ///
    /// `related_to` is folded in here rather than being a tool of its own: a
    /// relation is a `[[wikilink]]` in the sentence, which is how Brain already
    /// models one, so there is no second edge format to keep consistent.
    ///
    /// The same sentence saved twice is not saved twice. A fact that comes up
    /// again three weeks later is the commonest way a memory silently doubles,
    /// and the duplicate is indistinguishable from corroboration by the time the
    /// model reads it back.
    pub fn remember(
        &mut self,
        subject: &str,
        text: &str,
        kind: Kind,
        related_to: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Saved, MemoryError> {
        let subject = subject.trim();
        let text = text.trim();
        if subject.is_empty() || text.is_empty() {
            return Err(MemoryError::Refused(
                "a subject and something to say about it are both needed".into(),
            ));
        }

        let id = self.resolve_or_new(subject);
        let path = id.as_str().to_string();
        let normalised = observation::normalise(text);
        if let Some(already) = self
            .observations
            .iter()
            .find(|held| held.note == path && observation::normalise(&held.text) == normalised)
        {
            return Ok(Saved {
                note: path,
                kind: already.kind,
                already_there: true,
            });
        }

        let line = observation::render(text, kind, related_to, now);
        let existing = self
            .vault
            .read(&id)
            .map(|note| note.to_text())
            .unwrap_or_else(|_| format!("# {subject}\n"));

        // Through `from_text` rather than by editing the body: that is the
        // round-trip Brain guarantees, so frontmatter it does not understand
        // comes back out byte for byte.
        let note = Note::from_text(id.clone(), &append_under_heading(&existing, &line));
        self.vault.write(&note).map_err(MemoryError::Vault)?;
        self.rescan();
        Ok(Saved {
            note: path,
            kind,
            already_there: false,
        })
    }

    /// Search the vault: the words and the meaning, fused.
    ///
    /// `vector` is the query already embedded, which only the caller can do —
    /// it needs a model server, and this half of the application has no socket.
    /// `None` degrades to BM25 alone rather than failing, and that degradation
    /// is the normal state on a machine with no embedding server, so it is a
    /// first-class path and not an error case.
    ///
    /// Takes `&mut self` because finding something *is* the use of it: the
    /// ledger records what came back, and the dream reads that months later to
    /// decide what earned its keep.
    pub fn recall(
        &mut self,
        query: &str,
        limit: usize,
        vector: Option<&[f32]>,
        now: DateTime<Utc>,
    ) -> Vec<Recalled> {
        let query = query.trim();
        if query.is_empty() {
            return Vec::new();
        }

        let semantic = match (self.semantic.as_ref(), vector) {
            (Some(store), Some(vector)) => Some((store, vector)),
            _ => None,
        };
        let hits = search::hybrid(&self.index, &self.lexical, semantic, query, limit);

        let found: Vec<Recalled> = hits
            .iter()
            .map(|hit| Recalled {
                lexical: hit.lexical.is_some(),
                semantic: hit.semantic.is_some(),
                ..self.recalled(&hit.id)
            })
            .collect();

        let touched: Vec<String> = self
            .observations
            .iter()
            .filter(|held| found.iter().any(|hit| hit.path == held.note))
            .map(Observation::key)
            .collect();
        self.ledger.used(touched, now);

        found
    }

    /// Remove an observation.
    ///
    /// Only lines the assistant wrote are candidates, and the note itself is
    /// never deleted — an empty section is left behind rather than a missing
    /// file. Returns what was removed.
    pub fn forget(&mut self, subject: &str, matching: &str) -> Result<String, MemoryError> {
        let Some(id) = self.resolve(subject) else {
            return Err(MemoryError::Refused(format!(
                "there is no note for {subject}"
            )));
        };
        match self.remove_from(&id, matching)? {
            Some(removed) => Ok(removed),
            None => Err(MemoryError::Refused(
                "nothing Familiar wrote there matches that".into(),
            )),
        }
    }

    /// Drop the first line of `id` that Familiar wrote and that matches, if any.
    ///
    /// The primitive both `forget` and the dream act through, so there is one
    /// place that knows an unmarked line is untouchable.
    fn remove_from(&mut self, id: &NoteId, matching: &str) -> Result<Option<String>, MemoryError> {
        let matching = observation::normalise(matching);
        let text = self.vault.read(id).map_err(MemoryError::Vault)?.to_text();

        let mut removed = None;
        let kept: Vec<&str> = text
            .lines()
            .filter(|line| {
                if removed.is_some() || !observation::is_familiars(line) {
                    return true;
                }
                let held = observation::normalise(&observation::text_of(line));
                if matching.is_empty() || held.contains(&matching) {
                    removed = Some(observation::text_of(line));
                    return false;
                }
                true
            })
            .collect();

        let Some(removed) = removed else {
            return Ok(None);
        };

        let note = Note::from_text(id.clone(), &format!("{}\n", kept.join("\n")));
        self.vault.write(&note).map_err(MemoryError::Vault)?;
        self.rescan();
        Ok(Some(removed))
    }

    /// Everything held, as the dream sees it: the observation plus what the
    /// ledger and the recent threads know about it.
    ///
    /// `mentions` is keyed by [`Observation::key`] and comes from
    /// [`dream::mentions`], which reads transcripts — slow, and therefore the
    /// caller's business rather than this one's.
    pub fn held(&self, mentions: &std::collections::BTreeMap<String, u32>) -> Vec<dream::Held> {
        self.observations
            .iter()
            .map(|observation| {
                let key = observation.key();
                dream::Held {
                    uses: self.ledger.uses(&key),
                    last_used: self.ledger.last_used(&key),
                    mentions: mentions.get(&key).copied().unwrap_or(0),
                    observation: observation.clone(),
                }
            })
            .collect()
    }

    /// Carry out a night's plan.
    ///
    /// Operations are independent: one that cannot be carried out — because the
    /// note changed under the plan, most often — is counted and stepped over
    /// rather than aborting the rest. The vault is the truth and the next night
    /// sees it as it then is.
    ///
    /// Each operation is a single write of one note, so there is no moment at
    /// which a note is half-changed.
    /// A duplicate is visible, recoverable, and something the next night will
    /// tidy on its own.
    pub fn dream(&mut self, plan: &dream::Plan, now: DateTime<Utc>) -> dream::Applied {
        let mut applied = dream::Applied::default();
        for operation in &plan.operations {
            let done = match operation {
                dream::Operation::Reclassify { note, text, to, .. } => {
                    self.restamp(note, text, *to)
                }
                dream::Operation::Merge {
                    note,
                    subject,
                    kind,
                    into,
                    keys,
                    texts,
                } => self.merge(note, subject, *kind, into, keys, texts, now),
                dream::Operation::Drop {
                    note,
                    subject,
                    text,
                    why,
                    ..
                } => match self.remove_exact(note, text) {
                    true => {
                        applied.dropped.push(dream::Removed {
                            note: note.clone(),
                            subject: subject.clone(),
                            text: text.clone(),
                            why: *why,
                            on: now.date_naive(),
                        });
                        true
                    }
                    false => false,
                },
            };
            match (done, operation) {
                (false, _) => applied.failed += 1,
                (true, dream::Operation::Merge { .. }) => applied.merged += 1,
                (true, dream::Operation::Reclassify { .. }) => applied.reclassified += 1,
                (true, dream::Operation::Drop { .. }) => {}
            }
        }
        if !applied.is_quiet() {
            self.rescan();
            self.flush_ledger();
        }
        applied
    }

    /// Replace several lines of a note with one sentence, in a single write.
    ///
    /// One write rather than "append the replacement, then remove the
    /// originals", and the reason is a bug that pass had: removing an original
    /// meant finding the first line matching its text, and when the replacement
    /// *was* one of the originals word for word — which is what collapsing a
    /// duplicate looks like — the two were indistinguishable. Guarding against
    /// removing the replacement then meant removing nothing, and the note came
    /// out with three lines where it had started with two.
    ///
    /// Rewriting the whole note at once has no such ambiguity: the originals
    /// are struck from the text that exists, and the replacement is added
    /// afterwards to text that no longer contains them. It is also atomic, so
    /// there is no moment where the note is half-merged.
    #[allow(clippy::too_many_arguments)]
    fn merge(
        &mut self,
        note: &str,
        subject: &str,
        kind: Kind,
        into: &str,
        keys: &[String],
        texts: &[String],
        now: DateTime<Utc>,
    ) -> bool {
        let id = NoteId::from_relative(note);
        let Ok(existing) = self.vault.read(&id) else {
            return false;
        };

        // A multiset: the same sentence twice in one note has to lose both
        // copies, and `contains` alone would lose every line matching either.
        let mut wanted: Vec<String> = texts
            .iter()
            .map(|text| observation::normalise(text))
            .collect();
        let held = existing.to_text();
        let kept: Vec<&str> = held
            .lines()
            .filter(|line| {
                if !observation::is_familiars(line) {
                    return true;
                }
                let text = observation::normalise(&observation::text_of(line));
                match wanted.iter().position(|target| *target == text) {
                    Some(at) => {
                        wanted.remove(at);
                        false
                    }
                    None => true,
                }
            })
            .collect();
        // Every original has to have been there. One that was not means the
        // note changed under the plan, and half a merge is worse than none.
        if !wanted.is_empty() {
            return false;
        }

        let replacement = observation::render(into, kind, None, now);
        let merged = Note::from_text(
            id,
            &append_under_heading(&format!("{}\n", kept.join("\n")), &replacement),
        );
        if self.vault.write(&merged).is_err() {
            return false;
        }

        // Before the rescan, and that order is the whole of it: a rescan drops
        // ledger entries for lines that are no longer there, so carried
        // afterwards the counts would be gone by the time anything asked for
        // them — and a merged memory that looks brand new and unwanted is one
        // the next night drops.
        let key = Observation {
            note: note.to_string(),
            subject: subject.to_string(),
            text: into.to_string(),
            kind,
            saved: Some(now.date_naive()),
        }
        .key();
        self.ledger.merge_into(keys, &key);
        self.rescan();
        true
    }

    /// Rewrite one line's kind, keeping its text and the day it was saved.
    ///
    /// The date has to survive: refiling something is not saving it again, and
    /// resetting the clock would make every reclassified observation look new
    /// and immortal.
    fn restamp(&mut self, note: &str, text: &str, to: Kind) -> bool {
        let id = NoteId::from_relative(note);
        let Ok(existing) = self.vault.read(&id) else {
            return false;
        };
        let wanted = observation::normalise(text);
        let text = existing.to_text();
        let mut changed = false;
        let rewritten: Vec<String> = text
            .lines()
            .map(|line| {
                if changed || !observation::is_familiars(line) {
                    return line.to_string();
                }
                if observation::normalise(&observation::text_of(line)) != wanted {
                    return line.to_string();
                }
                changed = true;
                observation::restamp(line, to)
            })
            .collect();
        if !changed {
            return false;
        }
        let note = Note::from_text(id, &format!("{}\n", rewritten.join("\n")));
        if self.vault.write(&note).is_err() {
            return false;
        }
        self.rescan();
        true
    }

    /// Remove the one line of `note` whose text is exactly this, normalised.
    ///
    /// Distinct from `forget`, which takes a fragment the user typed and removes
    /// the first thing that contains it. The dream names a sentence it read out
    /// of the vault, and matching loosely there would remove a neighbour.
    fn remove_exact(&mut self, note: &str, text: &str) -> bool {
        let id = NoteId::from_relative(note);
        let Ok(existing) = self.vault.read(&id) else {
            return false;
        };
        let wanted = observation::normalise(text);
        let held = existing.to_text();
        let mut removed = false;
        let kept: Vec<&str> = held
            .lines()
            .filter(|line| {
                if removed || !observation::is_familiars(line) {
                    return true;
                }
                if observation::normalise(&observation::text_of(line)) == wanted {
                    removed = true;
                    return false;
                }
                true
            })
            .collect();
        if !removed {
            return false;
        }
        let note = Note::from_text(id, &format!("{}\n", kept.join("\n")));
        if self.vault.write(&note).is_err() {
            return false;
        }
        self.rescan();
        true
    }

    /// What every observation is worth today, split at the line between what has
    /// to be in front of the model and what it can go and look up.
    pub fn ranked(&self, now: DateTime<Utc>) -> (Vec<Ranked>, Vec<Ranked>) {
        let mut core = Vec::new();
        let mut rest = Vec::new();
        for held in &self.observations {
            let ranked = Ranked {
                score: held.score(self.ledger.uses(&held.key()), now),
                observation: held.clone(),
            };
            if held.kind.is_core() {
                core.push(ranked);
            } else {
                rest.push(ranked);
            }
        }
        (core, rest)
    }

    /// The ambient block: what the assistant carries into every conversation.
    ///
    /// The policy is in [`ambient`]; this is the part that knows where the
    /// material comes from.
    pub fn ambient(&self, now: DateTime<Utc>) -> Option<String> {
        let (core, recent) = self.ranked(now);
        ambient::compose(&core, &recent, &self.salient(BACKGROUND))
    }

    /// The notes most worth knowing about: inbound links first, then title.
    fn salient(&self, limit: usize) -> Vec<(String, String)> {
        let mut ranked: Vec<(usize, String, String)> = self
            .index
            .ids()
            .map(|id| {
                let links = self.index.backlinks(id).len();
                (links, id.title().to_string(), id.as_str().to_string())
            })
            .collect();
        ranked.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));

        ranked
            .into_iter()
            .take(limit)
            .map(|(_, title, path)| {
                let id = NoteId::from_relative(&path);
                (title, first_sentence(self.index.excerpt(&id)))
            })
            .filter(|(_, excerpt)| !excerpt.is_empty())
            .collect()
    }

    fn recalled(&self, id: &NoteId) -> Recalled {
        let text = self
            .vault
            .read(id)
            .map(|note| note.to_text())
            .unwrap_or_default();
        Recalled {
            title: id.title().to_string(),
            path: id.as_str().to_string(),
            excerpt: first_sentence(&prose(&text)),
            observations: text
                .lines()
                .filter(|line| observation::is_familiars(line))
                .map(observation::text_of)
                .collect(),
            lexical: false,
            semantic: false,
        }
    }

    /// An existing note for this subject, by title or alias.
    fn resolve(&self, subject: &str) -> Option<NoteId> {
        match self.index.resolve(subject.trim(), None) {
            brain::model::index::Resolution::Note(id) => Some(id),
            // Two notes of the same name is an ambiguity Brain reports rather
            // than guesses at, and so does this.
            _ => None,
        }
    }

    /// The note for a subject: the one that already exists anywhere in the
    /// vault, or a new one under [`FOLDER`].
    ///
    /// The order is the whole point. A vault already holding `People/Matthew.md`
    /// gets its observation appended *there* — the assistant joins the note you
    /// wrote rather than starting a rival copy somewhere else. Only when
    /// nothing matches does it create one, and then it creates it in its own
    /// folder, so a vault is never littered at the root with notes only the
    /// assistant has ever written.
    fn resolve_or_new(&self, subject: &str) -> NoteId {
        self.resolve(subject)
            .unwrap_or_else(|| NoteId::from_relative(format!("{FOLDER}/{}.md", sanitize(subject))))
    }
}

/// What `remember` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    /// The note it went into, vault-relative.
    pub note: String,
    pub kind: Kind,
    /// The vault already said this, so nothing was written. Not a failure: the
    /// caller wanted the fact to be in there and it is.
    pub already_there: bool,
}

/// Put the line under the heading, adding the heading if it is not there.
///
/// Appending at the end of the file would be simpler and wrong: it would land
/// inside whatever section the note happens to end with.
fn append_under_heading(text: &str, line: &str) -> String {
    let mut out = String::with_capacity(text.len() + line.len() + HEADING.len() + 4);

    if let Some(at) = text.find(HEADING) {
        let (before, rest) = text.split_at(at);
        let mut lines = rest.lines();
        let heading = lines.next().unwrap_or(HEADING);

        // Walk to the end of the section: the next heading, or the end.
        let mut section: Vec<&str> = Vec::new();
        let mut after: Vec<&str> = Vec::new();
        let mut ended = false;
        for line in lines {
            if !ended && line.starts_with("## ") {
                ended = true;
            }
            if ended {
                after.push(line);
            } else {
                section.push(line);
            }
        }
        while section.last().is_some_and(|line| line.trim().is_empty()) {
            section.pop();
        }

        out.push_str(before);
        out.push_str(heading);
        out.push('\n');
        for line in section {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(line);
        out.push('\n');
        if !after.is_empty() {
            out.push('\n');
            out.push_str(&after.join("\n"));
            out.push('\n');
        }
        return out;
    }

    out.push_str(text.trim_end());
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(HEADING);
    out.push('\n');
    out.push_str(line);
    out.push('\n');
    out
}

/// A note with Familiar's own section taken out.
///
/// Without this, `recall` hands back its own heading as if it were something
/// the user wrote — and the model dutifully reports "a note that he was noted
/// by Familiar", which is true of the file and nonsense as a fact.
fn prose(text: &str) -> String {
    let mut kept = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if line.trim_start().starts_with(HEADING) {
            inside = true;
            continue;
        }
        if inside {
            // Familiar's section runs until the next heading.
            if line.starts_with("## ") {
                inside = false;
            } else {
                continue;
            }
        }
        kept.push(line);
    }
    kept.join("\n")
}

/// The first sentence of a note, for a one-line summary.
fn first_sentence(text: &str) -> String {
    const LIMIT: usize = 160;
    let text = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or("");
    let cut = text
        .find(". ")
        .map(|at| at + 1)
        .unwrap_or_else(|| text.len().min(LIMIT));
    let mut out: String = text.chars().take(cut.min(LIMIT)).collect();
    if out.len() < text.len() {
        out.push('…');
    }
    out.trim().to_string()
}

/// A subject as a filename. Notes are files, so a subject with a slash in it
/// would otherwise become a folder nobody asked for.
fn sanitize(subject: &str) -> String {
    let cleaned: String = subject
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            other => other,
        })
        .collect();
    cleaned.trim().trim_matches('.').to_string()
}

/// Where the vault is, by default: the same place Brain keeps it.
pub fn brain_vault() -> Option<PathBuf> {
    let (config, _) = brain::model::config::Config::load(&brain::model::config::default_path());
    config.vault
}

/// Where the embedding server is, if nothing says otherwise.
///
/// Port 8081 rather than 8080: 8080 is where the chat model already lives on
/// this kind of setup, and an embedding model is a second, much smaller server
/// beside it rather than a replacement for it. The same default Brain uses,
/// which matters — see [`brain_embedding_url`].
pub const DEFAULT_EMBEDDING_URL: &str = "http://127.0.0.1:8081";

/// Where the embedding server is, by default: the one Brain was pointed at.
///
/// Read from Brain's config rather than kept in Familiar's, because there is one
/// vault and there had better be one set of vectors over it. Two applications
/// embedding the same notes with two different models would each invalidate the
/// other's cache on every launch, and the only symptom would be that search got
/// slow and then bad.
pub fn brain_embedding_url() -> String {
    let (config, _) = brain::model::config::Config::load(&brain::model::config::default_path());
    config
        .embedding_url
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_EMBEDDING_URL.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault() -> (tempfile::TempDir, Memory) {
        let directory = tempfile::tempdir().expect("temp dir");
        let ledger = directory.path().join(".ledger.json");
        let memory = Memory::open_with(directory.path(), ledger);
        (directory, memory)
    }

    /// A note at the path the test put it — one *you* wrote.
    fn read(root: &Path, name: &str) -> String {
        std::fs::read_to_string(root.join(name)).expect("the note")
    }

    /// A note the assistant had to create, which lands under its own folder.
    fn read_own(root: &Path, name: &str) -> String {
        read(root, &format!("{FOLDER}/{name}"))
    }

    fn now() -> DateTime<Utc> {
        "2026-08-01T09:00:00Z".parse().expect("date")
    }

    fn note(memory: &mut Memory, subject: &str, text: &str) -> String {
        memory
            .remember(subject, text, Kind::Fact, None, now())
            .expect("remember")
            .note
    }

    fn recall(memory: &mut Memory, query: &str) -> Vec<Recalled> {
        memory.recall(query, 5, None, now())
    }

    #[test]
    fn remembering_writes_a_note_a_person_can_read() {
        let (directory, mut memory) = vault();
        note(&mut memory, "Matthew", "writes Rust for GNOME apps");

        let note = read_own(directory.path(), "Matthew.md");
        assert!(note.contains("# Matthew"), "{note}");
        assert!(note.contains(HEADING), "{note}");
        assert!(note.contains("- writes Rust for GNOME apps"), "{note}");
        assert!(note.contains(TAG), "{note}");
    }

    #[test]
    fn a_relation_is_a_wikilink_not_a_second_format() {
        let (directory, mut memory) = vault();
        memory
            .remember(
                "Familiar",
                "is built on GTK",
                Kind::Fact,
                Some("Brain"),
                now(),
            )
            .expect("remember");

        let note = read_own(directory.path(), "Familiar.md");
        assert!(note.contains("[[Brain]]"), "{note}");
        // And Brain's own scanner sees it as a link, which is the whole point.
        let links = brain::model::markdown::extract(&note);
        assert!(
            links.links.iter().any(|link| link.target == "Brain"),
            "{note}"
        );
    }

    #[test]
    fn a_note_you_wrote_keeps_everything_that_was_in_it() {
        let (directory, mut memory) = vault();
        std::fs::write(
            directory.path().join("Rust.md"),
            "---\ntags: [learning]\n---\n\n# Rust\n\nMoves are destructive.\n",
        )
        .expect("write");
        memory.rescan();

        note(
            &mut memory,
            "Rust",
            "Matthew is learning the borrow checker",
        );

        let note = read(directory.path(), "Rust.md");
        assert!(note.contains("tags: [learning]"), "{note}");
        assert!(note.contains("Moves are destructive."), "{note}");
        assert!(note.contains("- Matthew is learning"), "{note}");
    }

    #[test]
    fn a_note_you_already_have_is_joined_not_duplicated() {
        // The order in `resolve_or_new` is the whole point: a vault holding
        // People/Matthew.md must get the observation *there*, not in a rival
        // copy under Familiar/ that neither of you would think to reconcile.
        let (directory, mut memory) = vault();
        std::fs::create_dir_all(directory.path().join("People")).expect("dir");
        std::fs::write(
            directory.path().join("People/Matthew.md"),
            "# Matthew\n\nMy own notes about him.\n",
        )
        .expect("write");
        memory.rescan();

        let path = note(&mut memory, "Matthew", "writes Rust for GNOME");

        assert_eq!(path, "People/Matthew.md", "it started a second note");
        assert!(!directory.path().join(FOLDER).join("Matthew.md").exists());
        let note = read(directory.path(), "People/Matthew.md");
        assert!(note.contains("My own notes about him."), "{note}");
        assert!(note.contains("writes Rust for GNOME"), "{note}");
    }

    #[test]
    fn a_subject_with_no_note_anywhere_gets_one_in_familiars_folder() {
        // And the root stays yours: a few weeks of use should not leave it
        // littered with files only the assistant has ever touched.
        let (directory, mut memory) = vault();
        let path = note(&mut memory, "Kubernetes", "is not used here");

        assert_eq!(path, "Familiar/Kubernetes.md");
        assert!(!directory.path().join("Kubernetes.md").exists());
    }

    #[test]
    fn the_same_fact_saved_twice_is_saved_once() {
        // A fact that comes up again three weeks later is the commonest way a
        // memory silently doubles, and by the time the model reads it back the
        // duplicate is indistinguishable from corroboration.
        let (directory, mut memory) = vault();
        note(&mut memory, "Matthew", "Prefers small commits.");
        let again = memory
            .remember(
                "Matthew",
                "prefers small commits",
                Kind::Preference,
                None,
                now(),
            )
            .expect("remember");

        assert!(again.already_there);
        let note = read_own(directory.path(), "Matthew.md");
        assert_eq!(note.matches("refers small commits").count(), 1, "{note}");
    }

    #[test]
    fn recall_still_reaches_outside_the_folder() {
        // Writes are scoped; reads never were. A note the user filed anywhere
        // is still findable.
        let (directory, mut memory) = vault();
        std::fs::create_dir_all(directory.path().join("Projects")).expect("dir");
        std::fs::write(
            directory.path().join("Projects/Cogsworth.md"),
            "# Cogsworth\n\nThe WebSocket transport lives here.\n",
        )
        .expect("write");
        memory.rescan();

        let found = recall(&mut memory, "Cogsworth");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path, "Projects/Cogsworth.md");
    }

    #[test]
    fn observations_gather_under_one_heading() {
        let (directory, mut memory) = vault();
        note(&mut memory, "Matthew", "writes Rust");
        note(&mut memory, "Matthew", "prefers GNOME");

        let note = read_own(directory.path(), "Matthew.md");
        assert_eq!(note.matches(HEADING).count(), 1, "{note}");
        let rust = note.find("writes Rust").expect("one");
        let gnome = note.find("prefers GNOME").expect("two");
        assert!(rust < gnome, "newer observations go last: {note}");
    }

    #[test]
    fn an_observation_lands_under_the_heading_not_at_the_end_of_the_file() {
        let (directory, mut memory) = vault();
        std::fs::write(
            directory.path().join("Rust.md"),
            format!("# Rust\n\n{HEADING}\n- old thing {TAG}\n\n## Reading list\n\n- the book\n"),
        )
        .expect("write");
        memory.rescan();

        note(&mut memory, "Rust", "a new thing");

        let note = read(directory.path(), "Rust.md");
        let new = note.find("a new thing").expect("the observation");
        let reading = note.find("## Reading list").expect("the other section");
        assert!(new < reading, "it landed in the wrong section:\n{note}");
        assert!(note.contains("- the book"), "{note}");
    }

    #[test]
    fn forget_removes_only_what_familiar_wrote() {
        let (directory, mut memory) = vault();
        std::fs::write(
            directory.path().join("Rust.md"),
            "# Rust\n\n- a note you wrote yourself\n",
        )
        .expect("write");
        memory.rescan();
        note(&mut memory, "Rust", "something to drop");

        memory.forget("Rust", "something to drop").expect("forget");

        let note = read(directory.path(), "Rust.md");
        assert!(note.contains("- a note you wrote yourself"), "{note}");
        assert!(!note.contains("something to drop"), "{note}");
    }

    #[test]
    fn forget_will_not_touch_a_line_it_did_not_write() {
        let (directory, mut memory) = vault();
        std::fs::write(
            directory.path().join("Rust.md"),
            "# Rust\n\n- moves are destructive\n",
        )
        .expect("write");
        memory.rescan();

        let refused = memory.forget("Rust", "moves are destructive");
        assert!(matches!(refused, Err(MemoryError::Refused(_))));
        assert!(read(directory.path(), "Rust.md").contains("- moves are destructive"));
    }

    #[test]
    fn forgetting_everything_leaves_the_note_behind() {
        let (directory, mut memory) = vault();
        note(&mut memory, "Rust", "the only thing");
        memory.forget("Rust", "the only thing").expect("forget");

        // Your file, not Familiar's, even when Familiar made it.
        assert!(directory.path().join(FOLDER).join("Rust.md").exists());
    }

    #[test]
    fn recall_finds_a_note_by_title_and_by_text() {
        let (directory, mut memory) = vault();
        std::fs::write(
            directory.path().join("Borrow checker.md"),
            "# Borrow checker\n\nIt tracks lifetimes across scopes.\n",
        )
        .expect("write");
        memory.rescan();
        let _ = directory;

        let by_title = recall(&mut memory, "borrow checker");
        assert_eq!(
            by_title.first().map(|hit| hit.title.as_str()),
            Some("Borrow checker")
        );

        let by_text = recall(&mut memory, "lifetimes");
        assert_eq!(
            by_text.first().map(|hit| hit.title.as_str()),
            Some("Borrow checker")
        );
    }

    #[test]
    fn a_hit_says_whether_the_words_were_there_or_only_the_meaning() {
        // A caller that cannot tell the two apart has to treat every hit the
        // same, which is exactly what makes a purely semantic near-miss read as
        // a confident answer.
        let (_directory, mut memory) = vault();
        note(
            &mut memory,
            "Cogsworth",
            "The WebSocket transport lives here.",
        );

        let found = recall(&mut memory, "Cogsworth");
        let hit = found.first().expect("a hit");
        assert!(hit.lexical, "{hit:?}");
        // No vectors were supplied, so nothing can have come from that half.
        assert!(!hit.semantic, "{hit:?}");
    }

    #[test]
    fn recall_reports_observations_rather_than_familiars_own_heading() {
        // The model reads whatever comes back as fact. Handing it the section
        // heading produced "a note that he was noted by Familiar".
        let (_directory, mut memory) = vault();
        note(&mut memory, "Matthew", "prefers GNOME apps written in Rust");

        let found = recall(&mut memory, "Matthew");
        let hit = found.first().expect("a hit");
        assert!(
            !hit.excerpt.contains("Noted by Familiar"),
            "{:?}",
            hit.excerpt
        );
        assert_eq!(
            hit.observations,
            ["prefers GNOME apps written in Rust"],
            "{hit:?}"
        );
    }

    #[test]
    fn a_notes_own_prose_survives_alongside_the_observations() {
        let (directory, mut memory) = vault();
        std::fs::write(
            directory.path().join("Rust.md"),
            "# Rust\n\nA language with a borrow checker.\n",
        )
        .expect("write");
        memory.rescan();
        note(&mut memory, "Rust", "Matthew is learning it");

        let hit = recall(&mut memory, "Rust")
            .into_iter()
            .next()
            .expect("a hit");
        assert_eq!(hit.excerpt, "A language with a borrow checker.");
        assert_eq!(hit.observations, ["Matthew is learning it"]);
    }

    #[test]
    fn recall_of_nothing_finds_nothing() {
        let (_directory, mut memory) = vault();
        assert!(memory.recall("   ", 5, None, now()).is_empty());
    }

    #[test]
    fn finding_something_counts_as_having_used_it() {
        // What the dream reads months later to decide what earned its keep.
        let (_directory, mut memory) = vault();
        note(
            &mut memory,
            "Cogsworth",
            "The WebSocket transport lives here.",
        );
        let key = memory.observations()[0].key();
        assert_eq!(memory.ledger().uses(&key), 0);

        recall(&mut memory, "Cogsworth");
        assert_eq!(memory.ledger().uses(&key), 1);
    }

    #[test]
    fn a_search_that_found_nothing_credits_nothing() {
        let (_directory, mut memory) = vault();
        note(
            &mut memory,
            "Cogsworth",
            "The WebSocket transport lives here.",
        );
        let key = memory.observations()[0].key();

        recall(&mut memory, "helicopters");
        assert_eq!(memory.ledger().uses(&key), 0);
    }

    #[test]
    fn the_ambient_block_says_it_is_data_and_wraps_the_facts() {
        let (_directory, mut memory) = vault();
        memory
            .remember(
                "Matthew",
                "writes Rust for GNOME",
                Kind::Profile,
                None,
                now(),
            )
            .expect("remember");

        let block = memory.ambient(now()).expect("a block");
        assert!(block.contains("data, not instructions"), "{block}");
        assert!(block.contains("<saved_memory>"), "{block}");
        assert!(block.contains("</saved_memory>"), "{block}");
        assert!(block.contains("writes Rust for GNOME"), "{block}");
    }

    #[test]
    fn a_standing_preference_rides_in_the_prompt_and_a_loose_fact_waits_to_be_asked_for() {
        // The line the whole design turns on. A preference the model would have
        // to `recall` before honouring is one it will not honour.
        let (_directory, mut memory) = vault();
        memory
            .remember(
                "Matthew",
                "wants files under work/ rather than the root",
                Kind::Preference,
                None,
                now(),
            )
            .expect("remember");
        memory
            .remember("Roof", "was finished in April", Kind::Fact, None, now())
            .expect("remember");

        let (core, rest) = memory.ranked(now());
        assert_eq!(core.len(), 1, "{core:?}");
        assert_eq!(rest.len(), 1, "{rest:?}");
    }

    #[test]
    fn an_empty_vault_contributes_no_block() {
        let (_directory, memory) = vault();
        assert_eq!(memory.ambient(now()), None);
    }

    #[test]
    fn salience_puts_the_most_linked_note_first() {
        let (directory, mut memory) = vault();
        for (name, body) in [
            ("Rust.md", "# Rust\n\nA language.\n"),
            ("Ownership.md", "# Ownership\n\nSee [[Rust]].\n"),
            ("Borrowing.md", "# Borrowing\n\nAlso [[Rust]].\n"),
        ] {
            std::fs::write(directory.path().join(name), body).expect("write");
        }
        memory.rescan();

        let salient = memory.salient(3);
        assert_eq!(
            salient.first().map(|(title, _)| title.as_str()),
            Some("Rust")
        );
    }

    #[test]
    fn a_subject_with_a_slash_does_not_become_a_folder() {
        let (directory, mut memory) = vault();
        note(&mut memory, "GTK/GNOME", "is a stack");
        // Under the folder, and still one file rather than a nested directory.
        assert!(directory.path().join(FOLDER).join("GTK-GNOME.md").exists());
    }

    #[test]
    fn an_observation_needs_both_a_subject_and_something_to_say() {
        let (_directory, mut memory) = vault();
        assert!(memory
            .remember("", "something", Kind::Fact, None, now())
            .is_err());
        assert!(memory
            .remember("Something", "  ", Kind::Fact, None, now())
            .is_err());
    }

    #[test]
    fn a_line_a_person_deleted_by_hand_stops_being_counted() {
        // The ledger keys into the vault; the vault is the truth. Otherwise the
        // file grows for ever with counts for sentences nobody can see.
        let (directory, mut memory) = vault();
        note(
            &mut memory,
            "Cogsworth",
            "The WebSocket transport lives here.",
        );
        recall(&mut memory, "Cogsworth");
        assert_eq!(memory.ledger().len(), 1);

        std::fs::write(
            directory.path().join(FOLDER).join("Cogsworth.md"),
            "# Cogsworth\n",
        )
        .expect("write");
        memory.rescan();
        assert_eq!(memory.ledger().len(), 0);
    }

    // -- what a night actually does to the files -----------------------------

    fn tonight() -> DateTime<Utc> {
        "2026-08-02T03:00:00Z".parse().expect("date")
    }

    /// Everything held, with nothing having been used or mentioned — the state
    /// the dream is most willing to act on.
    fn held(memory: &Memory) -> Vec<dream::Held> {
        memory.held(&std::collections::BTreeMap::new())
    }

    #[test]
    fn a_dream_drops_the_line_it_named_and_leaves_its_neighbours() {
        let (directory, mut memory) = vault();
        note(&mut memory, "Roof", "was finished in April");
        note(&mut memory, "Roof", "cost 13,850 in the end");

        let corpus = held(&memory);
        let plan = dream::Plan {
            operations: vec![dream::Operation::Drop {
                key: corpus[0].key(),
                note: "Familiar/Roof.md".into(),
                subject: "Roof".into(),
                text: "was finished in April".into(),
                why: dream::Why::Stale,
            }],
        };
        let applied = memory.dream(&plan, tonight());

        assert_eq!(applied.dropped.len(), 1);
        assert_eq!(applied.failed, 0);
        let note = read_own(directory.path(), "Roof.md");
        assert!(!note.contains("was finished in April"), "{note}");
        assert!(note.contains("cost 13,850 in the end"), "{note}");
    }

    #[test]
    fn a_dream_cannot_touch_a_line_it_did_not_write() {
        // The rule the whole subsystem rests on, at the one place where text is
        // removed unsupervised. A plan naming a sentence of the user's own
        // fails rather than acting.
        let (directory, mut memory) = vault();
        std::fs::write(
            directory.path().join("Rust.md"),
            "# Rust\n\n- moves are destructive\n",
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
        let applied = memory.dream(&plan, tonight());

        assert_eq!(applied.failed, 1);
        assert!(applied.dropped.is_empty());
        assert!(read(directory.path(), "Rust.md").contains("- moves are destructive"));
    }

    #[test]
    fn a_merge_writes_the_replacement_before_taking_the_originals_out() {
        let (directory, mut memory) = vault();
        note(&mut memory, "Roof", "was finished in April");
        note(&mut memory, "Roof", "cost 13,850 in the end");

        let corpus = held(&memory);
        let plan = dream::Plan {
            operations: vec![dream::Operation::Merge {
                note: "Familiar/Roof.md".into(),
                subject: "Roof".into(),
                kind: Kind::Fact,
                into: "The roof was finished in April 2026 and cost 13,850.".into(),
                keys: corpus.iter().map(dream::Held::key).collect(),
                texts: corpus
                    .iter()
                    .map(|item| item.observation.text.clone())
                    .collect(),
            }],
        };
        let applied = memory.dream(&plan, tonight());

        assert_eq!(applied.merged, 1);
        let note = read_own(directory.path(), "Roof.md");
        assert!(
            note.contains("finished in April 2026 and cost 13,850"),
            "{note}"
        );
        assert!(!note.contains("- was finished in April "), "{note}");
        assert_eq!(
            memory.observations().len(),
            1,
            "{:?}",
            memory.observations()
        );
    }

    #[test]
    fn a_merge_carries_the_uses_of_what_it_replaced() {
        // Throwing the counts away would make a merged memory look brand new
        // and unwanted, which is how the next night comes to drop it.
        let (_directory, mut memory) = vault();
        note(&mut memory, "Roof", "was finished in April");
        note(&mut memory, "Roof", "cost 13,850 in the end");
        recall(&mut memory, "Roof");

        let corpus = held(&memory);
        let plan = dream::Plan {
            operations: vec![dream::Operation::Merge {
                note: "Familiar/Roof.md".into(),
                subject: "Roof".into(),
                kind: Kind::Fact,
                into: "The roof was finished in April 2026 and cost 13,850.".into(),
                keys: corpus.iter().map(dream::Held::key).collect(),
                texts: corpus
                    .iter()
                    .map(|item| item.observation.text.clone())
                    .collect(),
            }],
        };
        memory.dream(&plan, tonight());

        let survivor = &memory.observations()[0];
        assert!(memory.ledger().uses(&survivor.key()) >= 2);
    }

    #[test]
    fn refiling_a_line_changes_its_kind_and_nothing_else() {
        let (directory, mut memory) = vault();
        note(&mut memory, "Matthew", "wants metric only, never imperial");
        let corpus = held(&memory);

        let plan = dream::Plan {
            operations: vec![dream::Operation::Reclassify {
                key: corpus[0].key(),
                note: "Familiar/Matthew.md".into(),
                subject: "Matthew".into(),
                text: "wants metric only, never imperial".into(),
                from: Kind::Fact,
                to: Kind::Preference,
            }],
        };
        let applied = memory.dream(&plan, tonight());

        assert_eq!(applied.reclassified, 1);
        assert!(applied.dropped.is_empty());
        let refiled = &memory.observations()[0];
        assert_eq!(refiled.kind, Kind::Preference);
        assert_eq!(refiled.text, "wants metric only, never imperial");
        // And it is now in the half of the block that rides in every prompt.
        assert_eq!(memory.ranked(tonight()).0.len(), 1);
        assert!(read_own(directory.path(), "Matthew.md").contains("preference"));
    }

    #[test]
    fn an_operation_over_a_note_that_has_changed_is_counted_and_stepped_over() {
        // The vault is the truth. A plan computed an hour ago against a note
        // since edited must not take the rest of the night down with it.
        let (_directory, mut memory) = vault();
        note(&mut memory, "Roof", "was finished in April");
        let plan = dream::Plan {
            operations: vec![
                dream::Operation::Drop {
                    key: "Familiar/Gone.md\u{1f}nothing".into(),
                    note: "Familiar/Gone.md".into(),
                    subject: "Gone".into(),
                    text: "nothing".into(),
                    why: dream::Why::Stale,
                },
                dream::Operation::Drop {
                    key: memory.observations()[0].key(),
                    note: "Familiar/Roof.md".into(),
                    subject: "Roof".into(),
                    text: "was finished in April".into(),
                    why: dream::Why::Stale,
                },
            ],
        };
        let applied = memory.dream(&plan, tonight());
        assert_eq!(applied.failed, 1);
        assert_eq!(applied.dropped.len(), 1);
    }

    #[test]
    fn a_dream_that_did_nothing_reports_nothing() {
        let (_directory, mut memory) = vault();
        note(&mut memory, "Roof", "was finished in April");
        assert!(memory.dream(&dream::Plan::default(), tonight()).is_quiet());
    }

    #[test]
    fn every_observation_in_the_vault_is_parsed_once_per_scan() {
        let (_directory, mut memory) = vault();
        note(&mut memory, "Matthew", "writes Rust");
        note(&mut memory, "Rust", "has a borrow checker");
        let held: Vec<&str> = memory
            .observations()
            .iter()
            .map(|held| held.text.as_str())
            .collect();
        assert_eq!(held.len(), 2, "{held:?}");
    }
}
