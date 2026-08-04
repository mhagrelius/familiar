//! A thread: one continuous conversation, and the file it lives in.
//!
//! One thread is one JSON file, written whole after each turn, tmp → fsync →
//! rename. There is no database. Brain measured this ground: a thousand files
//! read off disk in under two milliseconds, and the SQLite transcript in
//! `llamatui` existed because a terminal had nothing better to offer.
//!
//! [`Thread::messages_for_model`] is the single enforcement point of the
//! invariant that **history never carries reasoning**. llama-server rejects a
//! prior reasoning content part and Qwen3's template drops it regardless, so
//! this is correctness rather than taste, and it is a function rather than a
//! rule because a function can be tested.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, Utc};

use super::heartbeat::Schedule;
use serde::{Deserialize, Serialize};

use super::turn::{Finish, ToolCall, ToolOutcome, TurnMetrics, TurnState};
use super::wire::{Message, ToolInvocation};

/// Bumped only if the shape below changes incompatibly.
pub const SCHEMA_VERSION: u32 = 1;

/// A thread's identity and its filename stem.
///
/// The timestamp is the id: threads are created constantly, never merged, and
/// sort chronologically for free. Colons are legal here but not everywhere a
/// vault might be synced to, so the time is dashed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ThreadId(String);

impl ThreadId {
    pub fn at(when: DateTime<Utc>) -> Self {
        Self(when.format("%Y-%m-%dT%H-%M-%S%.3f").to_string())
    }

    pub fn now() -> Self {
        Self::at(Utc::now())
    }

    /// A stem read back off disk. Rejects anything that could escape the
    /// threads directory, since the id is used to build a path.
    pub fn from_stem(stem: &str) -> Option<Self> {
        let sane = !stem.is_empty()
            && stem
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && !stem.starts_with('.');
        sane.then(|| Self(stem.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One tool call, as it is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
    #[serde(default)]
    pub outcome: Option<ToolOutcome>,
}

impl From<&ToolCall> for StoredToolCall {
    fn from(call: &ToolCall) -> Self {
        Self {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            outcome: call.outcome.clone(),
        }
    }
}

/// The way back, for redrawing a thread that was reopened.
///
/// Everything a chip needs survives the round trip; `complete` does not need
/// to, because a call that reached the disk finished arriving by definition.
impl From<&StoredToolCall> for ToolCall {
    fn from(stored: &StoredToolCall) -> Self {
        Self {
            id: stored.id.clone(),
            name: stored.name.clone(),
            arguments: stored.arguments.clone(),
            complete: true,
            outcome: stored.outcome.clone(),
        }
    }
}

/// One user submission and the assistant's response to it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StoredTurn {
    pub at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub user: String,
    /// Images attached to the question, by content-addressed name. They live
    /// under the context rather than in this file: a thread should stay
    /// readable, and base64 in a transcript is neither readable nor small.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    /// Shown when the thread is reopened, and never sent back.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thinking: String,
    #[serde(default)]
    pub answer: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<StoredToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish: Option<Finish>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<TurnMetrics>,
}

impl StoredTurn {
    /// Fold a finished [`TurnState`] and the message that provoked it into the
    /// record kept on disk.
    pub fn new(user: impl Into<String>, state: &TurnState) -> Self {
        Self {
            at: Some(Utc::now()),
            user: user.into(),
            images: Vec::new(),
            thinking: state.thinking.clone(),
            answer: state.answer.clone(),
            tool_calls: state.tool_calls.iter().map(StoredToolCall::from).collect(),
            finish: state.finish.clone(),
            metrics: Some(state.metrics()),
        }
    }

    /// The answer was cut short by `max_tokens`, which the UI says out loud
    /// rather than leaving you to wonder.
    pub fn was_truncated(&self) -> bool {
        self.finish == Some(Finish::Length)
    }
}

/// Something Familiar itself put in the transcript — what compaction dropped
/// from the model's view, most often. Addressed to the reader, not the model:
/// [`Thread::messages_for_model`] does not send these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub at: Option<DateTime<Utc>>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    /// Boxed because a turn is much larger than a note, and a `Vec<Entry>` of
    /// mostly turns would otherwise pay the difference on every note too.
    Turn(Box<StoredTurn>),
    Note(Note),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Thread {
    #[serde(default = "default_version")]
    pub version: u32,
    pub id: ThreadId,
    /// Renamed by hand, or `None` and derived from the first thing you asked.
    #[serde(default)]
    pub title: Option<String>,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    /// Instructions for this chat alone, added to the project's.
    ///
    /// Nothing writes it yet — the settings that exist are the project's — and
    /// it is kept because a file written with one must still open. When there
    /// is a way to set it, it goes into [`crate::model::instructions::Prompt`]
    /// under the project's own.
    #[serde(default, alias = "persona", skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    /// A standing prompt and a schedule, if this thread wakes on its own.
    ///
    /// On the thread rather than in a job list beside it, because a heartbeat
    /// *is* a conversation that continues without being asked — the standing
    /// prompt goes down the ordinary turn pipeline and the answer lands here,
    /// where you can open it and read back a week of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<Heartbeat>,
    /// The rolling summary that stands in for this thread's earliest exchanges,
    /// once it has grown enough to need one.
    ///
    /// State rather than a derivation, and persisted with the thread, because
    /// computing it is a call to the model: a thread reopened tomorrow must not
    /// have to summarise itself again to be answerable. `entries` still holds
    /// every turn — this only shortens what is *sent*.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fold: Option<crate::model::compaction::Fold>,
    /// The several-step job this chat is working through, if it is.
    ///
    /// On the thread beside `heartbeat` and `fold`, and for the same reason as
    /// `fold`: it is state rather than a derivation. The steps are what the
    /// model proposed and what the *user* then edited, and neither survives
    /// being recomputed — reopening a chat tomorrow has to show the plan you
    /// annotated, not a fresh one.
    ///
    /// One at a time. A chat works through one job; a second would raise the
    /// question of which one `advance` meant, and there is no good answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<crate::model::workflow::Workflow>,
    #[serde(default)]
    pub entries: Vec<Entry>,
}

/// What a thread does when it wakes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub schedule: Schedule,
    /// What to ask. Submitted as an ordinary user turn.
    pub prompt: String,
    /// Off without being forgotten. A schedule you have paused is one you
    /// intend to resume, and deleting it to stop it for a week is how people
    /// lose the prompt they spent time on.
    #[serde(default = "crate::model::thread::yes")]
    pub enabled: bool,
    /// When it last ran, which is what `Schedule::due` measures from. `None`
    /// until it has, and the first run is scheduled from when it was set up
    /// rather than from the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<DateTime<Utc>>,
    /// What happened last time, for the window that lists these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<String>,
}

pub(crate) fn yes() -> bool {
    true
}

impl Heartbeat {
    pub fn new(schedule: Schedule, prompt: &str) -> Self {
        Self {
            schedule,
            prompt: prompt.trim().to_string(),
            enabled: true,
            last_run: None,
            last_outcome: None,
        }
    }

    /// Whether to run now.
    ///
    /// A thread that has never run gets its clock started here rather than
    /// firing immediately: `Schedule::due` answers `None` for a `None` last
    /// run, and the application records `last_run` when it sets one up.
    pub fn due(&self, now: DateTime<Local>) -> Option<crate::model::heartbeat::Due> {
        if !self.enabled {
            return None;
        }
        self.schedule
            .due(self.last_run.map(|last| last.with_timezone(&Local)), now)
    }

    /// The next time this is expected to run, for the window that lists them.
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

fn default_version() -> u32 {
    SCHEMA_VERSION
}

impl Thread {
    pub fn new() -> Self {
        let now = Utc::now();
        Self {
            version: SCHEMA_VERSION,
            id: ThreadId::at(now),
            title: None,
            created: now,
            updated: now,
            instructions: None,
            workspace: None,
            heartbeat: None,
            fold: None,
            workflow: None,
            entries: Vec::new(),
        }
    }

    /// Nothing has been said yet. An empty thread is never written to disk —
    /// opening the app and closing it should not leave a file behind.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn turns(&self) -> impl Iterator<Item = &StoredTurn> {
        self.entries.iter().filter_map(|entry| match entry {
            Entry::Turn(turn) => Some(turn.as_ref()),
            Entry::Note(_) => None,
        })
    }

    pub fn push_turn(&mut self, turn: StoredTurn) {
        self.updated = turn.at.unwrap_or_else(Utc::now);
        self.entries.push(Entry::Turn(Box::new(turn)));
    }

    pub fn push_note(&mut self, text: impl Into<String>) {
        let now = Utc::now();
        self.updated = now;
        self.entries.push(Entry::Note(Note {
            at: Some(now),
            text: text.into(),
        }));
    }

    /// The sidebar label: what it was renamed to, or the first line of the
    /// first thing you asked.
    pub fn display_title(&self) -> String {
        if let Some(title) = self.title.as_ref().filter(|t| !t.trim().is_empty()) {
            return title.clone();
        }
        match self.turns().next() {
            Some(first) => summarize(&first.user),
            None => "New Chat".to_string(),
        }
    }

    /// The model's view of this conversation, without any prior reasoning.
    pub fn messages_for_model(&self) -> Vec<Message> {
        self.messages_with_reasoning(false)
    }

    /// The model's view, optionally carrying the model's own thinking back.
    ///
    /// Whether this helps is a property of the template, not a universal rule.
    /// llama-server accepts `reasoning_content` on a history message and the
    /// froggeric template re-emits it inside `<think>` tags — but the stock
    /// Qwen3 template drops it, and a structured `text_reasoning` content part
    /// is rejected outright, which is what the original "never carry it" rule
    /// was about.
    ///
    /// **All of it or none of it, never a sliding window.** Measured over a
    /// six-turn thread: carrying everything roughly halves how much the model
    /// re-derives each turn (42k characters of thinking down to 24k) *and*
    /// keeps the cached prefix intact, because reasoning is only ever appended.
    /// Carrying the most recent two turns instead rewrites the middle of the
    /// prompt as old reasoning falls out, which threw the KV cache away and
    /// cost a 4,000-token re-prefill on every turn. Size is bounded by
    /// compaction, which folds at turn boundaries where a rewrite is expected.
    ///
    /// Two things are still always absent: **notes**, which are addressed to
    /// you, and an **unanswered turn**, which is the one currently streaming.
    pub fn messages_with_reasoning(&self, carry_reasoning: bool) -> Vec<Message> {
        let mut messages = Vec::new();
        for turn in self.turns() {
            if !turn.user.is_empty() {
                messages.push(Message::user(turn.user.clone()));
            }

            let ran: Vec<_> = turn
                .tool_calls
                .iter()
                .filter(|call| call.outcome.is_some())
                .collect();

            if !ran.is_empty() {
                let invocations = ran
                    .iter()
                    .map(|call| {
                        ToolInvocation::new(
                            call.id.clone(),
                            call.name.clone(),
                            call.arguments.clone(),
                        )
                    })
                    .collect();
                // The assistant message that asked for the tools carries no
                // text of its own: whatever it said afterwards belongs to the
                // message after the results.
                messages.push(Message {
                    role: super::wire::Role::Assistant,
                    content: None,
                    reasoning_content: None,
                    tool_calls: invocations,
                    tool_call_id: None,
                });
                for call in ran {
                    let text = match call.outcome.as_ref() {
                        Some(ToolOutcome::Ok(result)) => result.clone(),
                        Some(ToolOutcome::Failed(error)) => format!("Error: {error}"),
                        Some(ToolOutcome::Denied) => {
                            "The user declined to run this tool.".to_string()
                        }
                        None => unreachable!("filtered above"),
                    };
                    messages.push(Message::tool_result(call.id.clone(), text));
                }
            }

            if !turn.answer.is_empty() {
                let mut answer = Message::assistant(turn.answer.clone());
                if carry_reasoning && !turn.thinking.is_empty() {
                    answer = answer.with_reasoning(turn.thinking.clone());
                }
                messages.push(answer);
            }
        }
        messages
    }

    pub fn load(path: &Path) -> Result<Self, ThreadError> {
        let text = fs::read_to_string(path).map_err(|source| ThreadError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_str(&text).map_err(|source| ThreadError::Unreadable {
            path: path.to_path_buf(),
            detail: source.to_string(),
        })
    }

    /// Write atomically: tmp, flush, fsync, rename.
    pub fn save(&self, path: &Path) -> Result<(), ThreadError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| ThreadError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let text =
            serde_json::to_string_pretty(self).map_err(|source| ThreadError::Unreadable {
                path: path.to_path_buf(),
                detail: source.to_string(),
            })?;

        let temporary = path.with_extension("json.tmp");
        let io = |source| ThreadError::Io {
            path: temporary.clone(),
            source,
        };
        let mut file = fs::File::create(&temporary).map_err(io)?;
        file.write_all(text.as_bytes()).map_err(io)?;
        file.flush().map_err(io)?;
        file.sync_all().map_err(io)?;
        drop(file);

        fs::rename(&temporary, path).map_err(|source| ThreadError::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}

impl Default for Thread {
    fn default() -> Self {
        Self::new()
    }
}

/// The first line of a message, shortened to fit a sidebar.
fn summarize(text: &str) -> String {
    const LIMIT: usize = 48;
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let line = line.trim();
    if line.chars().count() <= LIMIT {
        return line.to_string();
    }
    // Break on a word if there is one near the limit, so a title does not end
    // mid-word for the sake of four characters.
    let truncated: String = line.chars().take(LIMIT).collect();
    let cut = truncated.rfind(' ').filter(|space| *space > LIMIT / 2);
    let kept = match cut {
        Some(space) => &truncated[..space],
        None => truncated.as_str(),
    };
    format!("{}…", kept.trim_end())
}

/// A title the model wrote, made fit to be one — or `None` if there is nothing
/// usable in it.
///
/// Everything here is defence against a model that answers a request for a
/// two-word heading with a sentence. The first line only, no surrounding
/// quotes, no trailing punctuation, and the same length ceiling a derived title
/// gets — a name that does not fit the sidebar is not better than the first
/// line of the conversation, it is the same problem with extra steps.
pub fn tidy_title(title: &str) -> Option<String> {
    let line = title
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .trim_matches(['"', '\'', '“', '”', '‘', '’'])
        .trim_end_matches(['.', ',', ';', ':'])
        .trim();
    if line.is_empty() {
        return None;
    }
    Some(summarize(line))
}

#[derive(Debug)]
pub enum ThreadError {
    Io { path: PathBuf, source: io::Error },
    Unreadable { path: PathBuf, detail: String },
}

impl std::fmt::Display for ThreadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Unreadable { path, detail } => write!(f, "{}: {detail}", path.display()),
        }
    }
}

impl std::error::Error for ThreadError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::wire::Role;

    fn answered(user: &str, answer: &str) -> StoredTurn {
        StoredTurn {
            at: Some(Utc::now()),
            user: user.into(),
            thinking: "the user wants a thing".into(),
            answer: answer.into(),
            ..Default::default()
        }
    }

    #[test]
    fn reasoning_is_left_out_unless_it_is_asked_for() {
        let mut thread = Thread::new();
        thread.push_turn(answered("why?", "because."));

        let messages = thread.messages_for_model();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].text_of(), "because.");
        let json = serde_json::to_string(&messages).expect("serialize");
        assert!(!json.contains("the user wants a thing"), "{json}");
    }

    #[test]
    fn reasoning_is_carried_for_every_turn_or_for_none() {
        // Never a window. Dropping old reasoning out of the middle of the
        // prompt rewrites the cached prefix on every turn; appending does not.
        let mut thread = Thread::new();
        for turn in 1..=4 {
            let mut stored = answered(&format!("q{turn}"), &format!("a{turn}"));
            stored.thinking = format!("thinking{turn}");
            thread.push_turn(stored);
        }

        let carried: Vec<&str> = thread
            .messages_with_reasoning(true)
            .iter()
            .filter_map(|m| m.reasoning_content.as_deref())
            .map(|r| Box::leak(r.to_string().into_boxed_str()) as &str)
            .collect();
        assert_eq!(
            carried,
            ["thinking1", "thinking2", "thinking3", "thinking4"]
        );

        assert!(thread
            .messages_with_reasoning(false)
            .iter()
            .all(|m| m.reasoning_content.is_none()));
    }

    #[test]
    fn a_note_is_for_the_reader_not_the_model() {
        let mut thread = Thread::new();
        thread.push_turn(answered("hello", "hi"));
        thread.push_note("Earlier turns were summarized to fit the context window.");

        let messages = thread.messages_for_model();
        assert_eq!(messages.len(), 2);
        assert!(thread
            .entries
            .iter()
            .any(|entry| matches!(entry, Entry::Note(_))));
    }

    #[test]
    fn a_tool_call_becomes_a_request_and_a_result() {
        let mut thread = Thread::new();
        thread.push_turn(StoredTurn {
            user: "what do I know about scanners?".into(),
            tool_calls: vec![StoredToolCall {
                id: "call_1".into(),
                name: "recall".into(),
                arguments: r#"{"query":"scanner"}"#.into(),
                outcome: Some(ToolOutcome::Ok("3 notes".into())),
            }],
            answer: "Three notes mention it.".into(),
            ..Default::default()
        });

        let messages = thread.messages_for_model();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[1].content.as_ref().map(|c| c.text()), None);
        assert_eq!(messages[1].tool_calls[0].function.name, "recall");
        assert_eq!(messages[2].role, Role::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(messages[3].text_of(), "Three notes mention it.");
    }

    #[test]
    fn a_tool_call_that_never_ran_is_not_replayed() {
        // Cancelled mid-approval: sending the request with no result would
        // leave the model waiting for an answer that is never coming.
        let mut thread = Thread::new();
        thread.push_turn(StoredTurn {
            user: "delete everything".into(),
            tool_calls: vec![StoredToolCall {
                id: "call_1".into(),
                name: "delete".into(),
                arguments: "{}".into(),
                outcome: None,
            }],
            ..Default::default()
        });
        assert_eq!(thread.messages_for_model().len(), 1);
    }

    #[test]
    fn a_denied_tool_tells_the_model_it_was_denied() {
        let mut thread = Thread::new();
        thread.push_turn(StoredTurn {
            user: "delete everything".into(),
            tool_calls: vec![StoredToolCall {
                id: "call_1".into(),
                name: "delete".into(),
                arguments: "{}".into(),
                outcome: Some(ToolOutcome::Denied),
            }],
            answer: "Understood, I have left it alone.".into(),
            ..Default::default()
        });
        let messages = thread.messages_for_model();
        assert!(messages[2].text_of().contains("declined"));
    }

    #[test]
    fn a_thread_round_trips_through_a_file() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("threads/2026-07-31T14-02-11.json");

        let mut thread = Thread::new();
        thread.push_turn(answered("why?", "because."));
        thread.push_note("compacted");
        thread.save(&path).expect("save");

        let read = Thread::load(&path).expect("load");
        assert_eq!(read, thread);
        assert!(read.turns().next().is_some());
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("t.json");
        Thread::new().save(&path).expect("save");

        let leftovers: Vec<_> = fs::read_dir(directory.path())
            .expect("read dir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_thread_written_by_an_earlier_build_still_opens() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("t.json");
        fs::write(
            &path,
            r#"{"id":"2026-01-01T00-00-00","created":"2026-01-01T00:00:00Z",
                "updated":"2026-01-01T00:00:00Z","entries":[]}"#,
        )
        .expect("write");

        let thread = Thread::load(&path).expect("load");
        assert_eq!(thread.version, SCHEMA_VERSION);
        assert!(thread.is_empty());
    }

    #[test]
    fn an_unreadable_thread_says_which_file() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("t.json");
        fs::write(&path, "{not json").expect("write");

        let error = Thread::load(&path).expect_err("error");
        assert!(error.to_string().contains("t.json"), "{error}");
    }

    #[test]
    fn the_title_falls_back_to_the_first_thing_you_asked() {
        let mut thread = Thread::new();
        assert_eq!(thread.display_title(), "New Chat");

        thread.push_turn(answered("How does the markdown scanner work?", "Like so."));
        assert_eq!(
            thread.display_title(),
            "How does the markdown scanner work?"
        );

        thread.title = Some("Scanner".into());
        assert_eq!(thread.display_title(), "Scanner");
    }

    #[test]
    fn a_name_the_model_wrote_is_made_fit_to_be_one() {
        // What the `schedule` tool hands over. A model asked for a two-word
        // heading mostly gives one, and when it does not, the failure has to be
        // a usable title rather than a paragraph in the sidebar.
        assert_eq!(
            tidy_title("Morning Briefing").as_deref(),
            Some("Morning Briefing")
        );
        assert_eq!(
            tidy_title("  \"Morning Briefing.\"  ").as_deref(),
            Some("Morning Briefing")
        );
        assert_eq!(
            tidy_title("Morning Briefing\nAI news and the weather").as_deref(),
            Some("Morning Briefing")
        );
        // Nothing usable in it is `None`, and the thread keeps deriving its
        // title from the first thing that was asked.
        assert_eq!(tidy_title("   "), None);
        assert_eq!(tidy_title(""), None);

        // And a model that answers with a sentence gets the same ceiling a
        // derived title gets, rather than a sidebar row three lines tall.
        let long = tidy_title(
            "A daily morning briefing covering artificial intelligence news and the local \
             forecast",
        )
        .expect("a title");
        assert!(long.chars().count() <= 49, "{long}");
        assert!(long.ends_with('…'), "{long}");
    }

    #[test]
    fn a_long_first_message_is_cut_on_a_word() {
        let mut thread = Thread::new();
        thread.push_turn(answered(
            "Explain how the block incremental rescanning interacts with marker hiding",
            "…",
        ));
        let title = thread.display_title();
        assert!(title.ends_with('…'), "{title}");
        assert!(title.chars().count() <= 49, "{title}");
        assert!(!title.contains("interacts"), "{title}");
    }

    #[test]
    fn a_thread_id_cannot_escape_its_directory() {
        assert!(ThreadId::from_stem("../../etc/passwd").is_none());
        assert!(ThreadId::from_stem(".hidden").is_none());
        assert!(ThreadId::from_stem("").is_none());
        assert!(ThreadId::from_stem("2026-07-31T14-02-11.123").is_some());
    }

    #[test]
    fn ids_sort_chronologically() {
        let early = ThreadId::at("2026-07-31T09:00:00Z".parse().expect("date"));
        let late = ThreadId::at("2026-07-31T14:02:11Z".parse().expect("date"));
        assert!(early < late);
    }
}
