//! Projects, and the directory they and their chats live in.
//!
//! A **Project** is what a person would call a project: a name, instructions
//! that are added to the assistant's built-in ones, the tools that are on, a
//! folder on disk, and the chats that belong to it. [`DEFAULT_PROJECT`] is the
//! one that always exists — the chats that belong to no project — and it is a
//! project in every way except that it has no name to change and cannot be
//! deleted. Its instructions are how somebody customises the assistant's
//! ordinary behaviour.
//!
//! # "Project" is a word in the window, never in the prompt
//!
//! The model already has two meanings for the word and neither is this one:
//! Planner has real projects (`#Project`, `add-project`) and the memory tool
//! has a `project` kind for what the user is working on. Both are scored by
//! their own eval families. So nothing here is ever composed into the system
//! prompt — not the name, not the slug, not the word. The model is told about a
//! **workspace folder**, which is what it was told before this was renamed, and
//! `the_prompt_never_learns_the_word_project` in `capability.rs` holds that
//! line.
//!
//! ```text
//! ~/.local/share/familiar/
//!   projects/
//!     default/
//!       project.json
//!       threads/2026-07-31T14-02-11.json
//! ```
//!
//! [`Store`] is the only thing that turns a slug into a path, so a slug that
//! could climb out of the data directory is rejected at the one place it could
//! do harm. [`Store::migrate`] renames the layout this one replaced.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::jobs::Jobs;
use super::thread::{Heartbeat, Thread, ThreadError, ThreadId};
use super::workflow::Workflow;

/// The project that always exists: chats that belong to no project.
pub const DEFAULT_PROJECT: &str = "default";

/// What the default project is called in the window. It has no name of its own
/// to edit, because "no project" is not a thing you name.
pub const DEFAULT_NAME: &str = "Chats";

pub const SCHEMA_VERSION: u32 = 1;

/// Which tools a project offers the model. Coarse on purpose: the model is
/// local and small, and a long tool list is the fastest way to make it call
/// the wrong one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSet {
    /// `remember` / `recall` / `forget`, over Brain's vault.
    #[serde(default = "yes")]
    pub memory: bool,
    /// Web search and page fetch. The only thing here that leaves the machine.
    #[serde(default = "yes")]
    pub web: bool,
    /// Reading and, behind approval, writing files under the workspace root.
    #[serde(default)]
    pub workspace: bool,
    /// Current conditions, the forecast and any active alerts.
    ///
    /// On by default with `memory` and `web`: it needs no key, no workspace and
    /// no approval, it is a read, and "what is the weather" is the most common
    /// thing anyone asks an assistant. Its egress is one government API.
    #[serde(default = "yes")]
    pub weather: bool,
    /// The `gh` CLI, which is already signed in as the user.
    ///
    /// Its own switch rather than part of the workspace, because the authority
    /// is different in kind: the workspace tools reach files on this machine,
    /// and this one acts as the user on repositories other people share. It
    /// still needs a workspace, which is the repository it runs in.
    #[serde(default)]
    pub github: bool,
    /// Making Word documents, workbooks, decks and PDFs, and handling PDFs the
    /// user already has.
    ///
    /// Off by default and useless without `workspace`, which is where the
    /// files go. It is a separate switch because it is eight more tools and a
    /// paragraph of prompt, and a small local model with a long tool list
    /// reaches for the wrong one — the same reason this whole struct is coarse.
    #[serde(default)]
    pub documents: bool,
    /// Planner's task list, through its `planner agent` CLI.
    ///
    /// Off by default: it is only useful on a machine that has Planner, and a
    /// tool that always fails is worse than one that is not offered. Needs no
    /// folder — Planner keeps its own store and the CLI talks to the running
    /// app.
    #[serde(default)]
    pub planner: bool,
    /// Magpie's transcripts, through its `magpie agent` CLI.
    ///
    /// Off by default for the same reason, and one more: its one useful verb
    /// spends minutes of CPU and hundreds of megabytes of disk, which is not
    /// something to have switched on in a project that will never ask for it.
    #[serde(default)]
    pub magpie: bool,
    /// Writing and running Python in a container, through
    /// [`crate::model::sandbox`].
    ///
    /// Off by default because it needs podman and a 600 MB image built once on
    /// the machine, and a tool that always fails is worse than one that is not
    /// offered. Needs no workspace: the sandbox keeps its own directory, and a
    /// workspace only adds something to read.
    #[serde(default)]
    pub python: bool,
    /// Asking a stronger model through `claude -p` or `codex exec`.
    ///
    /// Off by default and the most consequential switch here: it is the only
    /// capability that sends the user's words to a company's servers as a
    /// matter of course. Gated per call on top of that.
    #[serde(default)]
    pub escalate: bool,
    /// Reading and organising the user's email, through `model::email`.
    ///
    /// Off by default, and needs an account in Preferences. More of somebody's
    /// life passes through this than through anything else here.
    #[serde(default)]
    pub mail: bool,
    /// Letting this conversation set its own schedule, through
    /// [`crate::model::heartbeat`].
    ///
    /// Off by default like the rest, and the omission it fixes was worse than
    /// the ones above: the capability *shipped* — the menu has always listed
    /// scheduled chats — and the model was never told, so asked for a morning
    /// briefing it made a task reminding the user to ask for one, and then
    /// explained that it had no scheduler at all. It needs nothing installed
    /// and nothing configured, so the catalogue can always offer it.
    #[serde(default)]
    pub scheduling: bool,
    /// Planning a several-step job and working through it, through
    /// [`crate::model::workflow`].
    ///
    /// **On by default**, with `memory`, `web` and `weather`. It needs nothing
    /// installed and nothing configured — the steps live on the thread and the
    /// saved ones are files beside it — and a capability that only appears once
    /// you have gone looking for it in a menu is one most people never find.
    ///
    /// The worry that kept it off was that a model with a planning tool in
    /// front of it plans the weather. That is measured and it does not: every
    /// scenario in the `workflow` family's not-planning half scores 100%, and
    /// the cost of carrying its guidance in every prompt was measured against
    /// the two families that have historically been the canaries for prompt
    /// length. See `DESIGN.md`.
    #[serde(default = "yes")]
    pub workflow: bool,
}

fn yes() -> bool {
    true
}

impl Default for ToolSet {
    fn default() -> Self {
        Self {
            memory: true,
            web: true,
            weather: true,
            workspace: false,
            github: false,
            documents: false,
            planner: false,
            magpie: false,
            python: false,
            escalate: false,
            mail: false,
            scheduling: false,
            workflow: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    #[serde(default = "default_version")]
    pub version: u32,
    pub slug: String,
    pub name: String,
    /// Added to the assistant's built-in instructions for every chat here, and
    /// never in place of them.
    ///
    /// It replaced them until 2026-08-03, which meant the only way to add "call
    /// me Matt" was to rewrite the whole persona — including the paragraph
    /// about Markdown rendering that makes answers display properly — and lose
    /// whatever that paragraph gains whenever it is next improved. `persona` is
    /// still read so a file written before the change still opens.
    #[serde(default, alias = "persona", skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default)]
    pub tools: ToolSet,
    /// The folder on disk. Called the *workspace* everywhere the model can see,
    /// because that is the word its tools use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    /// Named only when something in front of llama-server routes by model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

fn default_version() -> u32 {
    SCHEMA_VERSION
}

impl Project {
    /// Chats that belong to no project.
    pub fn default_project() -> Self {
        Self {
            version: SCHEMA_VERSION,
            slug: DEFAULT_PROJECT.to_string(),
            name: DEFAULT_NAME.to_string(),
            instructions: None,
            tools: ToolSet::default(),
            workspace: None,
            model: None,
        }
    }

    pub fn named(name: &str) -> Self {
        Self {
            version: SCHEMA_VERSION,
            slug: slugify(name),
            name: name.trim().to_string(),
            instructions: None,
            tools: ToolSet::default(),
            workspace: None,
            model: None,
        }
    }

    pub fn is_default(&self) -> bool {
        self.slug == DEFAULT_PROJECT
    }
}

/// Enough of a chat to draw a sidebar row without opening it twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    pub id: ThreadId,
    pub title: String,
    pub updated: DateTime<Utc>,
    pub turns: usize,
}

/// Projects and their chats on disk. Nothing else builds a path under the data
/// directory.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `$XDG_DATA_HOME/familiar`, falling back to `~/.local/share`.
    pub fn default_root() -> PathBuf {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("familiar")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `jobs.json` beside the projects.
    ///
    /// One file for the whole machine rather than one per project, because
    /// "what is running?" is the question people ask and answering it should
    /// not mean walking every project directory.
    pub fn jobs_path(&self) -> PathBuf {
        self.root.join("jobs.json")
    }

    /// Every job, or an empty list when there is no file yet.
    ///
    /// A file that will not parse comes back empty rather than as an error, on
    /// the same principle the rest of the store follows: a machine with one bad
    /// file should still start. It is not overwritten until something is saved,
    /// so a hand edit with a typo in it can still be fixed by hand.
    pub fn load_jobs(&self) -> Jobs {
        let path = self.jobs_path();
        let Ok(text) = fs::read_to_string(&path) else {
            return Jobs::default();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    pub fn save_jobs(&self, jobs: &Jobs) -> Result<(), StoreError> {
        let text = serde_json::to_string_pretty(jobs).map_err(|error| StoreError::Io {
            path: self.jobs_path(),
            source: std::io::Error::other(error),
        })?;
        write_atomically(&self.jobs_path(), &text)
    }

    /// Whether the machine has a jobs file at all.
    ///
    /// The migration runs only when it does not — otherwise every start would
    /// re-import heartbeats the user has since deleted.
    pub fn has_jobs_file(&self) -> bool {
        self.jobs_path().exists()
    }

    /// Every schedule still living on a thread, for the one-time migration.
    ///
    /// Reads every thread of every project, which is why it happens once. The
    /// heartbeat is left on the thread afterwards: a file an older build can
    /// still open is worth more than a tidy one.
    pub fn heartbeats(&self, slugs: &[String]) -> Vec<(String, String, Heartbeat)> {
        let mut found = Vec::new();
        for slug in slugs {
            let Ok(summaries) = self.threads(slug) else {
                continue;
            };
            for summary in summaries {
                let Ok(thread) = self.load_thread(slug, &summary.id) else {
                    continue;
                };
                if let Some(beat) = thread.heartbeat.clone() {
                    found.push((slug.clone(), thread.id.to_string(), beat));
                }
            }
        }
        found
    }

    /// Rename the layout this one replaced.
    ///
    /// `contexts/` became `projects/`, `context.json` became `project.json`,
    /// and `main-line` — a name only ever visible to whoever chose it — became
    /// the default project. Each step is a rename that only happens when the
    /// old name exists and the new one does not, so running it twice does
    /// nothing and running it on a fresh machine does nothing.
    ///
    /// Explicit rather than folded into [`Store::new`]: it writes, and the rest
    /// of this type does not write until it is asked to.
    pub fn migrate(&self) -> Result<(), StoreError> {
        let rename = |from: PathBuf, to: PathBuf| -> Result<(), StoreError> {
            if from.exists() && !to.exists() {
                fs::rename(&from, &to).map_err(|source| StoreError::Io { path: from, source })?;
            }
            Ok(())
        };

        rename(self.root.join("contexts"), self.root.join("projects"))?;
        let directory = self.root.join("projects");
        rename(directory.join("main-line"), directory.join(DEFAULT_PROJECT))?;

        let Ok(entries) = fs::read_dir(&directory) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                rename(path.join("context.json"), path.join("project.json"))?;
            }
        }
        Ok(())
    }

    /// Every project, the default one first and the rest by name.
    ///
    /// A first run has no directory at all, and that is not an error: it
    /// returns the default project nobody has written yet.
    pub fn projects(&self) -> Result<Vec<Project>, StoreError> {
        let directory = self.root.join("projects");
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(vec![Project::default_project()])
            }
            Err(source) => {
                return Err(StoreError::Io {
                    path: directory,
                    source,
                })
            }
        };

        let mut projects = Vec::new();
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let Some(slug) = entry.file_name().to_str().and_then(safe_slug) else {
                continue;
            };
            projects.push(self.load_project(&slug)?);
        }

        if !projects.iter().any(Project::is_default) {
            projects.push(Project::default_project());
        }
        projects.sort_by(
            |left, right| match (left.is_default(), right.is_default()) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => left.name.to_lowercase().cmp(&right.name.to_lowercase()),
            },
        );
        Ok(projects)
    }

    /// A project directory with no readable `project.json` is a project whose
    /// settings were lost, not a project that does not exist — its chats are
    /// still there, so it opens with defaults.
    pub fn load_project(&self, slug: &str) -> Result<Project, StoreError> {
        let slug = self.checked(slug)?;
        let path = self.project_directory(&slug).join("project.json");
        let fallback = || {
            if slug == DEFAULT_PROJECT {
                Project::default_project()
            } else {
                let mut project = Project::named(&slug);
                project.slug = slug.clone();
                project
            }
        };
        match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Project>(&text) {
                // The directory name is the identity; a `slug` inside the file
                // disagreeing with it would make the project unreachable.
                Ok(mut project) => {
                    let default = slug == DEFAULT_PROJECT;
                    project.slug = slug;
                    // The default project has no name of its own — there is no
                    // row to edit one in — so the file does not get to name it.
                    // A file migrated from the old layout says `main-line`,
                    // which is what the sidebar showed until this line existed.
                    if default {
                        project.name = DEFAULT_NAME.to_string();
                    }
                    Ok(project)
                }
                Err(_) => Ok(fallback()),
            },
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(fallback()),
            Err(source) => Err(StoreError::Io { path, source }),
        }
    }

    pub fn save_project(&self, project: &Project) -> Result<(), StoreError> {
        let slug = self.checked(&project.slug)?;
        let directory = self.project_directory(&slug);
        fs::create_dir_all(directory.join("threads")).map_err(|source| StoreError::Io {
            path: directory.clone(),
            source,
        })?;
        let text =
            serde_json::to_string_pretty(project).map_err(|source| StoreError::Unreadable {
                path: directory.join("project.json"),
                detail: source.to_string(),
            })?;
        write_atomically(&directory.join("project.json"), &text)
    }

    /// A project under a name nobody has used. Two "Planning"s would otherwise
    /// be one directory.
    pub fn create_project(&self, name: &str) -> Result<Project, StoreError> {
        let mut project = Project::named(name);
        if project.slug.is_empty() {
            return Err(StoreError::BadName(name.to_string()));
        }
        let taken: Vec<String> = self.projects()?.into_iter().map(|p| p.slug).collect();
        if taken.contains(&project.slug) {
            let base = project.slug.clone();
            let mut suffix = 2;
            while taken.contains(&format!("{base}-{suffix}")) {
                suffix += 1;
            }
            project.slug = format!("{base}-{suffix}");
        }
        self.save_project(&project)?;
        Ok(project)
    }

    /// Deleting a project deletes its chats. The default project is not
    /// deletable — there has to be somewhere to talk.
    pub fn delete_project(&self, slug: &str) -> Result<(), StoreError> {
        let slug = self.checked(slug)?;
        if slug == DEFAULT_PROJECT {
            return Err(StoreError::Protected(slug));
        }
        let directory = self.project_directory(&slug);
        match fs::remove_dir_all(&directory) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::Io {
                path: directory,
                source,
            }),
        }
    }

    /// Every chat in a project, newest first.
    ///
    /// This opens each file. Brain measured the same shape — a thousand notes
    /// read in under two milliseconds — and a summary index would be a second
    /// copy of the truth that can disagree with the first.
    pub fn threads(&self, slug: &str) -> Result<Vec<ThreadSummary>, StoreError> {
        let slug = self.checked(slug)?;
        let directory = self.project_directory(&slug).join("threads");
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(StoreError::Io {
                    path: directory,
                    source,
                })
            }
        };

        let mut summaries = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // One unreadable thread hides itself, not the whole sidebar.
            let Ok(thread) = Thread::load(&path) else {
                continue;
            };
            summaries.push(ThreadSummary {
                title: thread.display_title(),
                updated: thread.updated,
                turns: thread.turns().count(),
                id: thread.id,
            });
        }
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.updated));
        Ok(summaries)
    }

    /// What has been talked about lately, across every project.
    ///
    /// One string per chat — the questions and the answers, not the thinking
    /// — for [`crate::model::memory::dream::mentions`] to count subjects in.
    /// A memory whose subject has been in the room all month is alive whether
    /// or not anyone searched for it by name, and that is a signal nothing else
    /// in the application can see.
    ///
    /// Reading every thread file is slow and that is fine: the only caller runs
    /// at three in the morning. `since` keeps it from being slow *and*
    /// unbounded — a year of conversation is not evidence about this month.
    /// Unreadable threads are skipped, as everywhere else here.
    pub fn recent_conversations(&self, since: DateTime<Utc>, cap: usize) -> Vec<String> {
        let mut found = Vec::new();
        let Ok(projects) = self.projects() else {
            return found;
        };
        for project in projects {
            let Ok(summaries) = self.threads(&project.slug) else {
                continue;
            };
            for summary in summaries {
                if found.len() >= cap {
                    return found;
                }
                if summary.updated < since {
                    // Summaries come back newest first, so the rest of this
                    // project is older still.
                    break;
                }
                let Ok(thread) = self.load_thread(&project.slug, &summary.id) else {
                    continue;
                };
                let said: String = thread
                    .turns()
                    .map(|turn| format!("{}\n{}", turn.user, turn.answer))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !said.trim().is_empty() {
                    found.push(said);
                }
            }
        }
        found
    }

    /// A thread with an id no file in this project is already using.
    ///
    /// Ids are timestamps to the millisecond, which is unique enough for a
    /// person clicking "New Chat" and not unique enough for anything faster.
    /// Rather than reach for more decimal places — which only moves the
    /// collision — the free id is the one nothing is holding.
    pub fn new_thread(&self, slug: &str) -> Result<Thread, StoreError> {
        let slug = self.checked(slug)?;
        let mut thread = Thread::new();
        let base = thread.id.to_string();
        let mut suffix = 2;
        while self.thread_path(&slug, &thread.id)?.exists() {
            thread.id = ThreadId::from_stem(&format!("{base}-{suffix}"))
                .ok_or_else(|| StoreError::BadName(base.clone()))?;
            suffix += 1;
        }
        Ok(thread)
    }

    pub fn load_thread(&self, slug: &str, id: &ThreadId) -> Result<Thread, StoreError> {
        let path = self.thread_path(slug, id)?;
        Thread::load(&path).map_err(StoreError::Thread)
    }

    /// An empty thread is not written: opening the app and closing it should
    /// leave nothing behind.
    pub fn save_thread(&self, slug: &str, thread: &Thread) -> Result<(), StoreError> {
        if thread.is_empty() {
            return Ok(());
        }
        let path = self.thread_path(slug, &thread.id)?;
        thread.save(&path).map_err(StoreError::Thread)
    }

    pub fn delete_thread(&self, slug: &str, id: &ThreadId) -> Result<(), StoreError> {
        let path = self.thread_path(slug, id)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::Io { path, source }),
        }
    }

    /// Every workflow this project has saved, by name.
    ///
    /// Markdown files rather than rows in `project.json`, because a workflow is
    /// a thing the user writes and edits and the vault makes the same argument
    /// for notes. A file that is not a workflow is skipped rather than reported:
    /// this directory is one the user can open, so something else being in it is
    /// ordinary rather than an error.
    pub fn workflows(&self, slug: &str) -> Result<Vec<Workflow>, StoreError> {
        let slug = self.checked(slug)?;
        let directory = self.workflow_directory(&slug);
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(StoreError::Io {
                    path: directory,
                    source,
                })
            }
        };

        let mut found = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(name) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Some(mut workflow) = Workflow::from_markdown(&text) else {
                continue;
            };
            workflow.saved_as = Some(name);
            found.push(workflow);
        }
        found.sort_by_key(|workflow| workflow.goal.to_lowercase());
        Ok(found)
    }

    /// One saved workflow, ready to be started. The name is slugified on the way
    /// in, so "Quarterly comparison" finds `quarterly-comparison.md` — the model
    /// will ask for it by the goal it saw, not by the filename it never did.
    pub fn load_workflow(&self, slug: &str, name: &str) -> Result<Option<Workflow>, StoreError> {
        let wanted = slugify(name);
        Ok(self
            .workflows(slug)?
            .into_iter()
            .find(|workflow| workflow.saved_as.as_deref() == Some(wanted.as_str())))
    }

    /// Save a workflow's shape, returning the name it was filed under.
    ///
    /// The outcomes are dropped here rather than at the call site — a saved
    /// workflow is a shape, and the one place that can guarantee it is the one
    /// place that writes the file.
    pub fn save_workflow(&self, slug: &str, workflow: &Workflow) -> Result<String, StoreError> {
        let slug = self.checked(slug)?;
        let name = slugify(&workflow.goal);
        if name.is_empty() || name.len() > 64 {
            return Err(StoreError::BadName(workflow.goal.clone()));
        }
        let directory = self.workflow_directory(&slug);
        fs::create_dir_all(&directory).map_err(|source| StoreError::Io {
            path: directory.clone(),
            source,
        })?;
        write_atomically(
            &directory.join(format!("{name}.md")),
            &workflow.fresh().to_markdown(),
        )?;
        Ok(name)
    }

    pub fn delete_workflow(&self, slug: &str, name: &str) -> Result<(), StoreError> {
        let slug = self.checked(slug)?;
        let name = slugify(name);
        if name.is_empty() {
            return Err(StoreError::BadName(name));
        }
        let path = self.workflow_directory(&slug).join(format!("{name}.md"));
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::Io { path, source }),
        }
    }

    fn project_directory(&self, slug: &str) -> PathBuf {
        self.root.join("projects").join(slug)
    }

    fn workflow_directory(&self, slug: &str) -> PathBuf {
        self.project_directory(slug).join("workflows")
    }

    fn thread_path(&self, slug: &str, id: &ThreadId) -> Result<PathBuf, StoreError> {
        let slug = self.checked(slug)?;
        Ok(self
            .project_directory(&slug)
            .join("threads")
            .join(format!("{id}.json")))
    }

    fn checked(&self, slug: &str) -> Result<String, StoreError> {
        safe_slug(slug).ok_or_else(|| StoreError::BadName(slug.to_string()))
    }
}

/// Lowercase, words joined by dashes, and nothing that means anything to a
/// filesystem.
pub fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    for character in name.trim().to_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

/// A slug that can only ever name a directory directly under `projects/`.
fn safe_slug(slug: &str) -> Option<String> {
    let sane = !slug.is_empty()
        && slug.len() <= 64
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !slug.starts_with('-')
        && !slug.ends_with('-');
    sane.then(|| slug.to_string())
}

fn write_atomically(path: &Path, text: &str) -> Result<(), StoreError> {
    use std::io::Write;

    // Appended rather than `with_extension`, which would file `notes.md`'s
    // temporary under `notes.json.tmp` and make a stray one impossible to place.
    let temporary = PathBuf::from(format!("{}.tmp", path.display()));
    let io = |source| StoreError::Io {
        path: temporary.clone(),
        source,
    };
    let mut file = fs::File::create(&temporary).map_err(io)?;
    file.write_all(text.as_bytes()).map_err(io)?;
    file.flush().map_err(io)?;
    file.sync_all().map_err(io)?;
    drop(file);

    fs::rename(&temporary, path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[derive(Debug)]
pub enum StoreError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Unreadable {
        path: PathBuf,
        detail: String,
    },
    Thread(ThreadError),
    /// A name that slugified to nothing, or a slug that could climb out of the
    /// data directory.
    BadName(String),
    /// The default project, which is not deletable.
    Protected(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Unreadable { path, detail } => write!(f, "{}: {detail}", path.display()),
            Self::Thread(source) => write!(f, "{source}"),
            Self::BadName(name) => write!(f, "{name:?} is not a usable name"),
            Self::Protected(slug) => write!(f, "{slug} cannot be deleted"),
        }
    }
}

impl std::error::Error for StoreError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::thread::StoredTurn;

    fn store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = Store::new(directory.path());
        (directory, store)
    }

    /// Threads are made through the store, as the application makes them, so
    /// two in the same millisecond do not become one file.
    fn thread_saying(store: &Store, slug: &str, text: &str) -> Thread {
        let mut thread = store.new_thread(slug).expect("new thread");
        thread.push_turn(StoredTurn {
            user: text.into(),
            answer: "…".into(),
            ..Default::default()
        });
        thread
    }

    #[test]
    fn a_first_run_has_a_default_project_and_no_directory() {
        let (directory, store) = store();
        let projects = store.projects().expect("projects");
        assert_eq!(projects.len(), 1);
        assert!(projects[0].is_default());
        assert!(!directory.path().join("projects").exists());
    }

    #[test]
    fn the_default_project_sorts_first_and_the_rest_by_name() {
        let (_directory, store) = store();
        store.create_project("Zettelkasten").expect("create");
        store.create_project("Planning").expect("create");
        store
            .save_project(&Project::default_project())
            .expect("save");

        let names: Vec<_> = store
            .projects()
            .expect("projects")
            .into_iter()
            .map(|project| project.name)
            .collect();
        assert_eq!(names, [DEFAULT_NAME, "Planning", "Zettelkasten"]);
    }

    #[test]
    fn a_project_round_trips_with_its_tools_and_instructions() {
        let (_directory, store) = store();
        let mut project = store.create_project("Planning").expect("create");
        project.instructions = Some("You help plan the week.".into());
        project.tools = ToolSet {
            memory: true,
            web: false,
            weather: false,
            workspace: true,
            github: false,
            documents: false,
            planner: false,
            magpie: false,
            python: false,
            escalate: false,
            mail: false,
            scheduling: false,
            workflow: false,
        };
        project.workspace = Some(PathBuf::from("/home/someone/Notes"));
        store.save_project(&project).expect("save");

        assert_eq!(store.load_project("planning").expect("load"), project);
    }

    /// The field was called `persona` and replaced the built-in instructions.
    /// A file written then still opens, and what it says is now added to them.
    #[test]
    fn a_project_written_before_the_rename_still_opens() {
        let (directory, store) = store();
        store.create_project("Planning").expect("create");
        fs::write(
            directory.path().join("projects/planning/project.json"),
            r#"{"slug":"planning","name":"Planning","persona":"You help plan the week."}"#,
        )
        .expect("write");

        let project = store.load_project("planning").expect("load");
        assert_eq!(
            project.instructions.as_deref(),
            Some("You help plan the week.")
        );
    }

    #[test]
    fn two_projects_of_the_same_name_get_their_own_directories() {
        let (_directory, store) = store();
        let first = store.create_project("Planning").expect("create");
        let second = store.create_project("Planning").expect("create");
        assert_eq!(first.slug, "planning");
        assert_eq!(second.slug, "planning-2");
        assert_eq!(second.name, "Planning");
    }

    #[test]
    fn a_project_whose_settings_were_lost_still_opens_with_its_chats() {
        let (directory, store) = store();
        store.create_project("Planning").expect("create");
        store
            .save_thread(
                "planning",
                &thread_saying(&store, "planning", "what is due?"),
            )
            .expect("save");
        fs::write(
            directory.path().join("projects/planning/project.json"),
            "{not json",
        )
        .expect("write");

        let project = store.load_project("planning").expect("load");
        assert_eq!(project.slug, "planning");
        assert_eq!(store.threads("planning").expect("threads").len(), 1);
    }

    #[test]
    fn a_slug_cannot_climb_out_of_the_data_directory() {
        let (_directory, store) = store();
        assert!(matches!(
            store.load_project("../../etc"),
            Err(StoreError::BadName(_))
        ));
        assert!(matches!(
            store.threads("../secrets"),
            Err(StoreError::BadName(_))
        ));
        assert!(matches!(
            store.delete_project("."),
            Err(StoreError::BadName(_))
        ));
    }

    #[test]
    fn the_default_project_cannot_be_deleted() {
        let (_directory, store) = store();
        store
            .save_project(&Project::default_project())
            .expect("save");
        assert!(matches!(
            store.delete_project(DEFAULT_PROJECT),
            Err(StoreError::Protected(_))
        ));
        assert!(store.load_project(DEFAULT_PROJECT).is_ok());
    }

    /// The layout before 2026-08-03: `contexts/main-line/context.json`. The
    /// chats under it are the point — losing them to a rename would be losing
    /// the conversation.
    #[test]
    fn the_old_layout_is_renamed_and_keeps_its_chats() {
        let (directory, store) = store();
        let old = directory.path().join("contexts/main-line");
        fs::create_dir_all(old.join("threads")).expect("dirs");
        fs::write(
            old.join("context.json"),
            r#"{"slug":"main-line","name":"main-line","persona":"Call me Matt."}"#,
        )
        .expect("write");
        // Written through the real type, so this is the file the old layout
        // actually held rather than one hand-rolled to match it.
        let mut thread = Thread::new();
        thread.push_turn(StoredTurn {
            user: "hello".into(),
            answer: "hi".into(),
            ..Default::default()
        });
        thread
            .save(&old.join("threads").join(format!("{}.json", thread.id)))
            .expect("write");

        store.migrate().expect("migrate");

        assert!(!directory.path().join("contexts").exists());
        let projects = store.projects().expect("projects");
        assert_eq!(projects.len(), 1);
        assert!(projects[0].is_default());
        // What it was called then is not a name anybody chose, and the window
        // showed it until this was asserted.
        assert_eq!(projects[0].name, DEFAULT_NAME);
        assert_eq!(projects[0].instructions.as_deref(), Some("Call me Matt."));
        assert_eq!(store.threads(DEFAULT_PROJECT).expect("threads").len(), 1);
    }

    #[test]
    fn migrating_twice_or_on_a_fresh_machine_does_nothing() {
        let (directory, store) = store();
        store.migrate().expect("first");
        store.migrate().expect("second");
        assert!(!directory.path().join("projects").exists());

        store.create_project("Planning").expect("create");
        store.migrate().expect("again");
        assert_eq!(store.projects().expect("projects").len(), 2);
    }

    #[test]
    fn deleting_a_project_takes_its_chats_with_it() {
        let (_directory, store) = store();
        store.create_project("Planning").expect("create");
        store
            .save_thread(
                "planning",
                &thread_saying(&store, "planning", "what is due?"),
            )
            .expect("save");

        store.delete_project("planning").expect("delete");
        assert_eq!(store.threads("planning").expect("threads").len(), 0);
        assert_eq!(store.projects().expect("projects").len(), 1);
    }

    #[test]
    fn threads_are_listed_newest_first() {
        let (_directory, store) = store();
        let mut early = thread_saying(&store, DEFAULT_PROJECT, "first question");
        early.updated = "2026-07-30T09:00:00Z".parse().expect("date");
        store.save_thread(DEFAULT_PROJECT, &early).expect("save");

        let mut late = thread_saying(&store, DEFAULT_PROJECT, "second question");
        late.updated = "2026-07-31T09:00:00Z".parse().expect("date");
        store.save_thread(DEFAULT_PROJECT, &late).expect("save");

        let titles: Vec<_> = store
            .threads(DEFAULT_PROJECT)
            .expect("threads")
            .into_iter()
            .map(|summary| summary.title)
            .collect();
        assert_eq!(titles, ["second question", "first question"]);
    }

    #[test]
    fn an_empty_thread_is_never_written() {
        let (_directory, store) = store();
        store
            .save_thread(DEFAULT_PROJECT, &Thread::new())
            .expect("save");
        assert!(store.threads(DEFAULT_PROJECT).expect("threads").is_empty());
    }

    #[test]
    fn one_unreadable_thread_does_not_hide_the_others() {
        let (directory, store) = store();
        store
            .save_thread(
                DEFAULT_PROJECT,
                &thread_saying(&store, DEFAULT_PROJECT, "a good one"),
            )
            .expect("save");
        fs::write(
            directory
                .path()
                .join("projects/default/threads/broken.json"),
            "{not json",
        )
        .expect("write");

        let summaries = store.threads(DEFAULT_PROJECT).expect("threads");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].title, "a good one");
    }

    #[test]
    fn a_workflow_is_saved_by_its_goal_and_found_by_it() {
        // The model asks for it by the goal it saw in the conversation; it never
        // saw a filename.
        let (_directory, store) = store();
        let mut flow = crate::model::workflow::Workflow::proposed(
            "Quarterly comparison",
            vec!["read the figures".into(), "write it up".into()],
        )
        .expect("a workflow");
        flow.steps[1].note = Some("use Q2, not Q1".into());
        flow.advance(crate::model::workflow::State::Done {
            outcome: "4 sheets".into(),
        })
        .expect("advance");

        let name = store
            .save_workflow(DEFAULT_PROJECT, &flow)
            .expect("save workflow");
        assert_eq!(name, "quarterly-comparison");

        let read = store
            .load_workflow(DEFAULT_PROJECT, "Quarterly comparison")
            .expect("load")
            .expect("found");
        assert_eq!(read.goal, "Quarterly comparison");
        // The shape survives; the one run it happened to have does not.
        assert_eq!(read.progress(), (0, 2));
        assert_eq!(read.steps[1].note.as_deref(), Some("use Q2, not Q1"));
        assert_eq!(read.saved_as.as_deref(), Some("quarterly-comparison"));

        assert_eq!(store.workflows(DEFAULT_PROJECT).expect("list").len(), 1);
        store
            .delete_workflow(DEFAULT_PROJECT, "Quarterly comparison")
            .expect("delete");
        assert!(store.workflows(DEFAULT_PROJECT).expect("list").is_empty());
    }

    #[test]
    fn something_else_in_the_workflows_folder_is_skipped_rather_than_reported() {
        // It is a directory the user can open, so a stray file in it is
        // ordinary. A project whose workflow list fails to load because of one
        // is worse than one that quietly shows the rest.
        let (directory, store) = store();
        let folder = directory
            .path()
            .join("projects")
            .join(DEFAULT_PROJECT)
            .join("workflows");
        fs::create_dir_all(&folder).expect("folder");
        fs::write(folder.join("notes.md"), "# Just prose\n\nnothing here.").expect("write");
        fs::write(folder.join("not-markdown.txt"), "1. a\n2. b").expect("write");

        assert!(store.workflows(DEFAULT_PROJECT).expect("list").is_empty());
    }

    #[test]
    fn a_workflow_with_no_usable_name_is_refused_rather_than_written_somewhere_odd() {
        let (_directory, store) = store();
        let flow =
            crate::model::workflow::Workflow::proposed("!!!", vec!["one".into(), "two".into()])
                .expect("a workflow");
        assert!(store.save_workflow(DEFAULT_PROJECT, &flow).is_err());
    }

    #[test]
    fn a_thread_round_trips_through_the_store() {
        let (_directory, store) = store();
        let thread = thread_saying(&store, DEFAULT_PROJECT, "what is due?");
        store.save_thread(DEFAULT_PROJECT, &thread).expect("save");

        let read = store
            .load_thread(DEFAULT_PROJECT, &thread.id)
            .expect("load");
        assert_eq!(read, thread);

        store
            .delete_thread(DEFAULT_PROJECT, &thread.id)
            .expect("delete");
        assert!(store.threads(DEFAULT_PROJECT).expect("threads").is_empty());
    }

    #[test]
    fn two_new_threads_never_become_one_file() {
        // Ids are timestamps, and two "New Chat" clicks can land in the same
        // millisecond. Before the store allocated the id, the second thread
        // silently overwrote the first.
        let (_directory, store) = store();
        let first = thread_saying(&store, DEFAULT_PROJECT, "one");
        store.save_thread(DEFAULT_PROJECT, &first).expect("save");
        let second = thread_saying(&store, DEFAULT_PROJECT, "two");
        store.save_thread(DEFAULT_PROJECT, &second).expect("save");

        assert_ne!(first.id, second.id);
        assert_eq!(store.threads(DEFAULT_PROJECT).expect("threads").len(), 2);
    }

    #[test]
    fn deleting_a_thread_that_is_already_gone_is_not_an_error() {
        let (_directory, store) = store();
        let id = ThreadId::now();
        assert!(store.delete_thread(DEFAULT_PROJECT, &id).is_ok());
    }

    #[test]
    fn recent_conversations_are_what_was_said_and_not_what_was_thought() {
        // The thinking is behind a disclosure and is not a record of what came
        // up; counting subject mentions in it would credit a memory for the
        // model having considered it.
        let (_directory, store) = store();
        let mut thread = store.new_thread(DEFAULT_PROJECT).expect("a thread");
        thread.push_turn(StoredTurn {
            at: Some(Utc::now()),
            user: "How is the Kubernetes cluster doing?".into(),
            thinking: "The user has never mentioned Cogsworth.".into(),
            answer: "It looks healthy.".into(),
            ..Default::default()
        });
        store.save_thread(DEFAULT_PROJECT, &thread).expect("save");

        let said = store.recent_conversations(Utc::now() - chrono::Duration::days(30), 50);
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("Kubernetes"), "{said:?}");
        assert!(said[0].contains("looks healthy"), "{said:?}");
        assert!(!said[0].contains("Cogsworth"), "{said:?}");
    }

    #[test]
    fn a_conversation_older_than_the_window_is_not_evidence_about_this_month() {
        let (_directory, store) = store();
        let mut thread = store.new_thread(DEFAULT_PROJECT).expect("a thread");
        thread.push_turn(StoredTurn {
            at: Some(Utc::now()),
            user: "How is the Kubernetes cluster doing?".into(),
            answer: "It looks healthy.".into(),
            ..Default::default()
        });
        store.save_thread(DEFAULT_PROJECT, &thread).expect("save");

        assert!(store
            .recent_conversations(Utc::now() + chrono::Duration::days(1), 50)
            .is_empty());
    }

    #[test]
    fn names_slugify_to_something_a_filesystem_can_hold() {
        assert_eq!(slugify("Planning"), "planning");
        assert_eq!(slugify("  Q3 / roadmap!  "), "q3-roadmap");
        assert_eq!(slugify("Ideas — 2026"), "ideas-2026");
        assert_eq!(slugify("…"), "");
    }

    #[test]
    fn a_name_that_slugifies_to_nothing_is_refused() {
        let (_directory, store) = store();
        assert!(matches!(
            store.create_project("…"),
            Err(StoreError::BadName(_))
        ));
    }
}
