//! Running the tools the model asked for.
//!
//! Every tool answers through a callback, because some of them cannot answer
//! immediately. The vault tools are file reads and appends and call back before
//! `run` returns; a web search calls back when Exa does. One shape for both
//! means the application has one path through a round of tools rather than a
//! synchronous one and an asynchronous one that have to agree.

use std::cell::RefCell;
use std::rc::Rc;

use chrono::Utc;

use crate::model::documents;
use crate::model::dynamo;
use crate::model::escalate;
use crate::model::github;
use crate::model::magpie;
use crate::model::memory::{observation::Kind, Memory};
use crate::model::news;
use crate::model::office;
use crate::model::planner;
use crate::model::sandbox::{self, Sandbox};
use crate::model::tools;
use crate::model::turn::{ToolCall, ToolOutcome};
use crate::model::weather::{self, Point};
use crate::model::web::{self, ContentsRequest, SearchRequest, SearchResponse};
use crate::model::workspace::{self, Workspace};
use crate::ui::client;
use crate::ui::embedder::Embeddings;

/// How many notes `recall` brings back. Enough to be useful, few enough that
/// the results do not crowd out the conversation in the model's context.
const RECALL_LIMIT: usize = 5;

/// Where the Exa key comes from, in order.
///
/// The environment first, so a key can be supplied for one run without being
/// written anywhere; then Preferences, which is where most people will put it.
pub fn exa_key(configured: Option<&str>) -> Option<String> {
    std::env::var("EXA_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
        .or_else(|| {
            configured
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(str::to_string)
        })
}

/// Where a slow tool's progress lines go while it is still running.
pub type Progress = Rc<dyn Fn(&str)>;

/// The one-shot completion of a slow run, held where two async reads can reach
/// it so that whichever finishes last is the one that answers.
type Settle = Rc<RefCell<Option<Box<dyn FnOnce(Option<String>)>>>>;

/// Runs a tool call and says what happened.
pub struct Runner {
    memory: Rc<RefCell<Option<Memory>>>,
    web: soup::Session,
    exa_key: Option<String>,
    /// The project's folder, if it has one. Absent means the tools are not
    /// offered, so this being `None` here is a mistake rather than a state.
    workspace: Option<Workspace>,
    /// Where the weather is, from Preferences. `None` until it is set, and the
    /// tool says so rather than picking somewhere.
    weather_at: Option<Point>,
    /// Somewhere to put a line about what a slow tool is doing while it does
    /// it. Only `magpie transcribe` produces any; everything else here answers
    /// in under a second and has nothing to report.
    progress: Option<Progress>,
    /// The embedding thread, when there is one. Absent means `recall` searches
    /// lexically, which is what it did before there were vectors and is still a
    /// perfectly good answer.
    embeddings: Option<Rc<Embeddings>>,
    /// Where Python runs, when the context has it switched on. Absent means the
    /// tools are not offered, so `None` here is a mistake rather than a state —
    /// the same contract the workspace keeps.
    sandbox: Option<Sandbox>,
    /// Where and how to ask a stronger model, when the context allows it.
    escalation: Option<Escalation>,
    /// The mail account, when the context has mail switched on.
    mail: Option<crate::ui::mail::Account>,
    /// How to switch a capability on, which only the application can do — it
    /// owns the context and writes it to disk. A closure rather than a handle
    /// to the application, so this module stays ignorant of it, in step with
    /// `progress`.
    switch_on: Option<SwitchOn>,
    /// How to set this chat's schedule, which only the application can do — it
    /// owns the open thread and writes it to disk. A closure for the same
    /// reason `switch_on` is one.
    scheduler: Option<Scheduler>,
    /// How to act on this chat's workflow, which only the application can do —
    /// it owns the thread the steps live on and the folder the saved ones live
    /// in. Same shape as `scheduler`, same reason.
    workflows: Option<Keeper>,
}

/// Given the names the model asked for, switch them on and say what happened.
pub type SwitchOn = Rc<dyn Fn(&[String]) -> String>;

/// Given an action, a phrasing of when, a standing prompt and a name for the
/// chat, do it and say what happened.
pub type Scheduler = Rc<dyn Fn(&str, &str, &str, &str) -> Result<String, String>>;

/// Given a `workflow` call's arguments, act on this chat's workflow and say
/// what happened — or why it could not.
pub type Keeper = Rc<dyn Fn(&str) -> Result<String, String>>;

/// What `escalate` needs: which CLI, which model, and somewhere empty to run.
#[derive(Debug, Clone)]
pub struct Escalation {
    pub backend: escalate::Backend,
    pub model: Option<String>,
    /// The data directory. The consultation runs in a subdirectory of it, not
    /// in the user's workspace.
    pub root: std::path::PathBuf,
}

impl Runner {
    pub fn new(memory: Rc<RefCell<Option<Memory>>>, exa_key: Option<String>) -> Self {
        Self {
            memory,
            web: client::web_session(),
            exa_key,
            workspace: None,
            weather_at: None,
            progress: None,
            embeddings: None,
            sandbox: None,
            escalation: None,
            mail: None,
            switch_on: None,
            scheduler: None,
            workflows: None,
        }
    }

    pub fn with_workflows(mut self, workflows: Keeper) -> Self {
        self.workflows = Some(workflows);
        self
    }

    pub fn with_switch(mut self, switch_on: SwitchOn) -> Self {
        self.switch_on = Some(switch_on);
        self
    }

    pub fn with_scheduler(mut self, scheduler: Scheduler) -> Self {
        self.scheduler = Some(scheduler);
        self
    }

    pub fn with_mail(mut self, mail: Option<crate::ui::mail::Account>) -> Self {
        self.mail = mail;
        self
    }

    pub fn with_escalation(mut self, escalation: Option<Escalation>) -> Self {
        self.escalation = escalation;
        self
    }

    pub fn with_sandbox(mut self, sandbox: Option<Sandbox>) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub fn with_embeddings(mut self, embeddings: Option<Rc<Embeddings>>) -> Self {
        self.embeddings = embeddings;
        self
    }

    pub fn with_weather(mut self, at: Option<Point>) -> Self {
        self.weather_at = at;
        self
    }

    pub fn with_progress(mut self, progress: Progress) -> Self {
        self.progress = Some(progress);
        self
    }

    pub fn with_workspace(mut self, workspace: Option<Workspace>) -> Self {
        self.workspace = workspace;
        self
    }

    /// Run `call` and report the outcome exactly once.
    ///
    /// Never panics on bad arguments: a small local model writes malformed JSON
    /// often enough that treating it as a crash would make the app unusable.
    /// The model is told what was wrong so it can try again.
    pub fn run<F>(&self, call: &ToolCall, done: F)
    where
        F: FnOnce(ToolOutcome) + 'static,
    {
        let arguments: serde_json::Value = match serde_json::from_str(&call.arguments) {
            Ok(arguments) => arguments,
            Err(_) if call.arguments.trim().is_empty() => serde_json::json!({}),
            Err(error) => {
                done(ToolOutcome::Failed(format!(
                    "those arguments are not valid JSON ({error}). Try again with a plain \
                     object."
                )));
                return;
            }
        };
        let text = |key: &str| {
            arguments
                .get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string()
        };

        match call.name.as_str() {
            "recall" => self.recall(text("query"), done),
            "remember" => done(self.remember(
                &text("subject"),
                &text("observation"),
                Some(text("kind")).filter(|kind| !kind.is_empty()),
                Some(text("related_to")).filter(|related| !related.is_empty()),
            )),
            "forget" => done(self.forget(&text("subject"), &text("matching"))),
            "web_search" => {
                let results = arguments
                    .get("numResults")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(5) as usize;
                self.search(&text("query"), results, done);
            }
            "news" => {
                let days = arguments
                    .get("days")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(news::Window::DEFAULT_DAYS);
                self.news(
                    Some(text("topic")).filter(|topic| !topic.trim().is_empty()),
                    days,
                    done,
                );
            }
            "fetch_url" => self.fetch(&text("url"), done),
            "list_dir" => done(self.in_workspace(|space| space.list(&text("path")))),
            "read_file" => done(self.in_workspace(|space| space.read(&text("path")))),
            "search_files" => {
                let where_to = text("path");
                done(self.in_workspace(|space| {
                    space.search(
                        &text("query"),
                        Some(where_to.as_str()).filter(|p| !p.is_empty()),
                    )
                }))
            }
            "write_file" => {
                done(self.in_workspace(|space| space.write(&text("path"), &text("contents"))))
            }
            "move_file" => {
                done(self.in_workspace(|space| space.move_to(&text("from"), &text("to"))))
            }
            "delete_file" => done(self.delete(&text("path"))),

            // -- documents --------------------------------------------------
            "read_skill" => done(read_skill(&text("name"))),
            "create_document" => done(self.create_document(
                &text("path"),
                Some(text("title")).filter(|t| !t.is_empty()),
                &text("markdown"),
            )),
            "create_pdf" => done(self.create_pdf(
                &text("path"),
                Some(text("title")).filter(|t| !t.is_empty()),
                &text("markdown"),
            )),
            "create_spreadsheet" => {
                done(self.create_spreadsheet(&text("path"), arguments.get("sheets")))
            }
            "create_presentation" => {
                done(self.create_presentation(&text("path"), arguments.get("slides")))
            }
            "run_python" => self.run_python(&text("code"), done),
            "escalate" => self.escalate(&text("question"), &text("tried"), done),
            "mail" => self.mail(&tools::argv_of(&call.arguments), done),
            "copy_to_workspace" => done(self.copy_to_workspace(&text("from"), &text("to"))),
            "use_tools" => done(self.use_tools(arguments.get("names"))),
            "schedule" => done(self.schedule(
                &text("action"),
                &text("when"),
                &text("prompt"),
                &text("title"),
            )),
            // Handed the raw arguments rather than unpacked fields: the action
            // decides which of them mean anything, and `workflow::Action::parse`
            // is the one place that knows. Unpacking here would be a second
            // parser to keep in step with it.
            "workflow" => done(self.workflow(&call.arguments)),
            "gh" => self.gh(&tools::argv_of(&call.arguments), done),
            "dynamo" => self.dynamo(&tools::argv_of(&call.arguments), done),
            "planner" => self.planner(&tools::argv_of(&call.arguments), done),
            "magpie" => self.magpie(&tools::argv_of(&call.arguments), done),
            "weather" => {
                // A named place, or the configured one. Both halves or neither:
                // a latitude alone is a half-written call, not a location.
                let asked = match (
                    arguments
                        .get("latitude")
                        .and_then(serde_json::Value::as_f64),
                    arguments
                        .get("longitude")
                        .and_then(serde_json::Value::as_f64),
                ) {
                    (Some(latitude), Some(longitude)) => Some(Point {
                        latitude,
                        longitude,
                    }),
                    _ => self.weather_at,
                };
                self.weather(asked.filter(Point::is_plausible), done)
            }
            "read_pdf" => self.read_pdf(&text("path"), &text("pages"), done),
            "merge_pdfs" => self.merge_pdfs(arguments.get("from"), &text("to"), done),
            "extract_pages" => self.extract_pages(&text("path"), &text("pages"), &text("to"), done),

            other => done(ToolOutcome::Failed(format!(
                "there is no tool called {other}"
            ))),
        }
    }

    // -- the vault ------------------------------------------------------------

    /// Search the notes, semantically where that is possible.
    ///
    /// Two steps rather than one because the first needs a socket: the query is
    /// embedded on the worker thread, and the lookup happens when the vector
    /// comes back — or immediately, with no vector, when there is no embedding
    /// server. Both paths end in the same `answer`, so the only difference a
    /// missing server makes is which notes come back.
    fn recall<F>(&self, query: String, done: F)
    where
        F: FnOnce(ToolOutcome) + 'static,
    {
        let memory = self.memory.clone();
        let asked = query.clone();
        let answer = move |vector: Option<Vec<f32>>| {
            // The vault borrow ends before `done` runs, and that is not
            // tidiness. `done` continues the turn: it builds the next request,
            // which reads the vault to decide which tools to offer. Holding
            // the borrow across it makes that read a panic — and because this
            // closure is called from a GLib callback, which cannot unwind, the
            // panic aborts the process rather than failing the tool.
            //
            // `remember` and `forget` are safe from this by shape: they return
            // their outcome and the borrow ends with them. Only this one has a
            // callback, because only this one waits for a socket.
            let outcome = {
                let mut borrowed = memory.borrow_mut();
                match borrowed.as_mut() {
                    None => ToolOutcome::Failed("no notes are configured".into()),
                    Some(memory) => {
                        let found =
                            memory.recall(&asked, RECALL_LIMIT, vector.as_deref(), Utc::now());
                        memory.flush_ledger();
                        Self::recalled(&asked, &found)
                    }
                }
            };
            done(outcome);
        };
        match &self.embeddings {
            Some(embeddings) => embeddings.query(&query, answer),
            None => answer(None),
        }
    }

    /// What `recall` tells the model it found.
    fn recalled(query: &str, found: &[crate::model::memory::Recalled]) -> ToolOutcome {
        if found.is_empty() {
            // Not a failure: "nothing" is a real answer and the model should
            // say so rather than treat it as an error to work around.
            return ToolOutcome::Ok(format!("No notes mention {query}."));
        }
        let lines: Vec<String> = found
            .iter()
            .map(|hit| {
                let mut entry = format!("- {}", hit.title);
                // A hit only the vectors liked is a different degree of
                // confidence from one whose words were there, and a model that
                // cannot tell them apart reports a near-miss as an answer.
                if hit.semantic && !hit.lexical {
                    entry.push_str(" (related, not an exact match)");
                }
                if !hit.excerpt.is_empty() {
                    entry.push_str(&format!(" — {}", hit.excerpt));
                }
                for observation in &hit.observations {
                    entry.push_str(&format!("\n    · {observation}"));
                }
                entry
            })
            .collect();
        ToolOutcome::Ok(format!(
            "{} note(s) mention {query}:\n{}",
            found.len(),
            lines.join("\n")
        ))
    }

    fn remember(
        &self,
        subject: &str,
        observation: &str,
        kind: Option<String>,
        related: Option<String>,
    ) -> ToolOutcome {
        let mut borrowed = self.memory.borrow_mut();
        let Some(memory) = borrowed.as_mut() else {
            return ToolOutcome::Failed("no notes are configured".into());
        };
        // An unrecognised or absent kind is a `fact`: the least privileged one,
        // which decays fastest and does not ride in every prompt. Guessing
        // upward would be the wrong way to be wrong.
        let kind = kind.as_deref().and_then(Kind::parse).unwrap_or(Kind::Fact);
        match memory.remember(subject, observation, kind, related.as_deref(), Utc::now()) {
            Ok(saved) if saved.already_there => ToolOutcome::Ok(format!(
                "Already saved in {}. Nothing was written again.",
                saved.note
            )),
            Ok(saved) => ToolOutcome::Ok(format!("Saved to {} as a {}.", saved.note, kind.label())),
            Err(error) => ToolOutcome::Failed(error.to_string()),
        }
    }

    fn forget(&self, subject: &str, matching: &str) -> ToolOutcome {
        let mut borrowed = self.memory.borrow_mut();
        let Some(memory) = borrowed.as_mut() else {
            return ToolOutcome::Failed("no notes are configured".into());
        };
        match memory.forget(subject, matching) {
            Ok(removed) => ToolOutcome::Ok(format!("Removed “{removed}” from {subject}.")),
            Err(error) => ToolOutcome::Failed(error.to_string()),
        }
    }

    // -- the workspace --------------------------------------------------------

    /// Run something against the workspace, or say there is not one.
    fn in_workspace<F>(&self, act: F) -> ToolOutcome
    where
        F: FnOnce(&Workspace) -> Result<String, workspace::Refusal>,
    {
        let Some(space) = self.workspace.as_ref() else {
            return ToolOutcome::Failed("this context has no workspace".into());
        };
        match act(space) {
            Ok(said) => ToolOutcome::Ok(said),
            Err(refusal) => ToolOutcome::Failed(refusal.to_string()),
        }
    }

    /// Deleting goes to the desktop trash, so what the model removes is
    /// somewhere a person would think to look for it.
    fn delete(&self, asked: &str) -> ToolOutcome {
        let Some(space) = self.workspace.as_ref() else {
            return ToolOutcome::Failed("this context has no workspace".into());
        };
        let path = match space.resolve(asked) {
            Ok(path) => path,
            Err(refusal) => return ToolOutcome::Failed(refusal.to_string()),
        };
        match workspace::trash(&path) {
            Ok(()) => ToolOutcome::Ok(format!("Moved {asked} to the trash.")),
            Err(error) => ToolOutcome::Failed(format!("{asked} could not be deleted: {error}")),
        }
    }

    // -- documents ------------------------------------------------------------

    /// Everything a document tool needs: a workspace, and a path that ends in
    /// the right thing.
    ///
    /// The extension is checked rather than corrected. A `.docx` written to
    /// `report.txt` opens as a wall of gibberish and neither the model nor the
    /// user can tell the tool was at fault, while silently renaming what was
    /// asked for hides the mistake from both.
    fn destination<'a>(
        &'a self,
        asked: &str,
        extension: &str,
    ) -> Result<&'a Workspace, ToolOutcome> {
        let space = self
            .workspace
            .as_ref()
            .ok_or_else(|| ToolOutcome::Failed("this context has no workspace".into()))?;
        office::check_extension(asked, extension).map_err(ToolOutcome::Failed)?;
        Ok(space)
    }

    fn create_document(&self, path: &str, title: Option<String>, markdown: &str) -> ToolOutcome {
        let space = match self.destination(path, "docx") {
            Ok(space) => space,
            Err(outcome) => return outcome,
        };
        let blocks = office::markup::parse(markdown);
        if blocks.is_empty() {
            return ToolOutcome::Failed(EMPTY.into());
        }
        let bytes = office::docx::write(title.as_deref(), &blocks);
        report(space.write_bytes(path, &bytes, "a Word document"), &blocks)
    }

    fn create_pdf(&self, path: &str, title: Option<String>, markdown: &str) -> ToolOutcome {
        let space = match self.destination(path, "pdf") {
            Ok(space) => space,
            Err(outcome) => return outcome,
        };
        let blocks = office::markup::parse(markdown);
        if blocks.is_empty() {
            return ToolOutcome::Failed(EMPTY.into());
        }
        let rendered = match office::pdf::write(title.as_deref(), &blocks) {
            Ok(rendered) => rendered,
            Err(error) => return ToolOutcome::Failed(error.to_string()),
        };
        match space.write_bytes(path, &rendered.bytes, "a PDF") {
            Ok(said) => ToolOutcome::Ok(format!("{said} {} page(s).", rendered.pages)),
            Err(refusal) => ToolOutcome::Failed(refusal.to_string()),
        }
    }

    fn create_spreadsheet(&self, path: &str, sheets: Option<&serde_json::Value>) -> ToolOutcome {
        let space = match self.destination(path, "xlsx") {
            Ok(space) => space,
            Err(outcome) => return outcome,
        };
        let Some(listed) = sheets.and_then(serde_json::Value::as_array) else {
            return ToolOutcome::Failed("`sheets` has to be a list of {name, rows} objects".into());
        };

        let mut built = Vec::new();
        for entry in listed {
            let name = entry
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Sheet1");
            let mut sheet = office::xlsx::Sheet::new(name);
            sheet.header = entry
                .get("header")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            let Some(rows) = entry.get("rows").and_then(serde_json::Value::as_array) else {
                return ToolOutcome::Failed(format!(
                    "the sheet {name:?} has no `rows` — it should be a list of lists of strings"
                ));
            };
            for row in rows {
                let Some(cells) = row.as_array() else {
                    return ToolOutcome::Failed("every row has to be a list of cell values".into());
                };
                sheet.rows.push(cells.iter().map(cell).collect());
            }
            built.push(sheet);
        }

        if built.iter().all(|sheet| sheet.rows.is_empty()) {
            return ToolOutcome::Failed("there are no rows to write".into());
        }
        let counted: usize = built.iter().map(|sheet| sheet.rows.len()).sum();
        let bytes = office::xlsx::write(&built);
        match space.write_bytes(path, &bytes, "a workbook") {
            Ok(said) => ToolOutcome::Ok(format!(
                "{said} {} sheet(s), {counted} row(s).",
                built.len()
            )),
            Err(refusal) => ToolOutcome::Failed(refusal.to_string()),
        }
    }

    fn create_presentation(&self, path: &str, slides: Option<&serde_json::Value>) -> ToolOutcome {
        let space = match self.destination(path, "pptx") {
            Ok(space) => space,
            Err(outcome) => return outcome,
        };
        let Some(listed) = slides.and_then(serde_json::Value::as_array) else {
            return ToolOutcome::Failed(
                "`slides` has to be a list of {title, bullets, notes} objects".into(),
            );
        };
        if listed.is_empty() {
            return ToolOutcome::Failed("there are no slides to write".into());
        }

        let built: Vec<office::pptx::Slide> = listed
            .iter()
            .map(|entry| office::pptx::Slide {
                title: entry
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                bullets: entry
                    .get("bullets")
                    .and_then(serde_json::Value::as_array)
                    .map(|bullets| {
                        bullets
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(bullet)
                            .collect()
                    })
                    .unwrap_or_default(),
                notes: entry
                    .get("notes")
                    .and_then(serde_json::Value::as_str)
                    .filter(|notes| !notes.trim().is_empty())
                    .map(str::to_string),
            })
            .collect();

        let bytes = office::pptx::write(&built);
        match space.write_bytes(path, &bytes, "a presentation") {
            Ok(said) => ToolOutcome::Ok(format!("{said} {} slide(s).", built.len())),
            Err(refusal) => ToolOutcome::Failed(refusal.to_string()),
        }
    }

    /// Read a PDF out of the workspace, the same way a dropped one is read.
    ///
    /// `pdfinfo` then `pdftotext`, both through gio so the window keeps
    /// running — a fifty-page document would otherwise freeze it mid-turn.
    /// The pages with no text layer say so in their place rather than going
    /// missing, and the whole thing is framed as untrusted data.
    fn read_pdf<F>(&self, asked: &str, wanted: &str, done: F)
    where
        F: FnOnce(ToolOutcome) + 'static,
    {
        let Some(space) = self.workspace.as_ref() else {
            done(ToolOutcome::Failed("this context has no workspace".into()));
            return;
        };
        let path = match space.resolve(asked) {
            Ok(path) => path,
            Err(refusal) => {
                done(ToolOutcome::Failed(refusal.to_string()));
                return;
            }
        };
        match space.read_bytes(asked) {
            Ok(bytes) if !documents::is_pdf(&bytes) => {
                done(ToolOutcome::Failed(format!(
                    "{asked} is not a PDF. Use read_file for a text file."
                )));
                return;
            }
            Err(refusal) => {
                done(ToolOutcome::Failed(refusal.to_string()));
                return;
            }
            Ok(_) => {}
        }

        let name = asked.to_string();
        let wanted = wanted.trim().to_string();
        run(documents::info_command(&path), move |counted| {
            let Some(counted) = counted else {
                done(ToolOutcome::Failed(NO_POPPLER.into()));
                return;
            };
            let info = documents::parse_info(&counted);
            // Resolved before extracting, so a nonsense range is a sentence
            // about page numbers rather than an empty document.
            let only = if wanted.is_empty() {
                None
            } else {
                match documents::parse_pages(&wanted, info.pages) {
                    Ok(only) => Some(only),
                    Err(complaint) => {
                        done(ToolOutcome::Failed(complaint));
                        return;
                    }
                }
            };

            run(documents::extract_command(&path), move |extracted| {
                let pages = documents::split_pages(&extracted.unwrap_or_default());
                let mut plan = documents::plan(info.pages, &pages);
                if let Some(only) = only {
                    plan.pages.retain(|page| only.contains(&page.number()));
                    // Nothing is rasterised for a tool call — there is no
                    // question to attach an image to — so a scanned page says
                    // so in its place and is not counted as omitted.
                    plan.to_rasterise.clear();
                    plan.omitted.clear();
                }
                done(ToolOutcome::Ok(documents::frame_within(
                    &name,
                    &info,
                    &plan,
                    documents::TOOL_TEXT_BUDGET,
                )));
            });
        });
    }

    fn merge_pdfs<F>(&self, from: Option<&serde_json::Value>, to: &str, done: F)
    where
        F: FnOnce(ToolOutcome) + 'static,
    {
        let Some(space) = self.workspace.as_ref() else {
            done(ToolOutcome::Failed("this context has no workspace".into()));
            return;
        };
        let sources: Vec<String> = from
            .and_then(serde_json::Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if sources.len() < 2 {
            done(ToolOutcome::Failed(
                "merging needs at least two PDFs in `from`".into(),
            ));
            return;
        }
        if let Err(complaint) = office::check_extension(to, "pdf") {
            done(ToolOutcome::Failed(complaint));
            return;
        }

        let mut resolved = Vec::new();
        for source in &sources {
            match space.resolve(source) {
                Ok(path) if path.is_file() => resolved.push(path),
                Ok(_) => {
                    done(ToolOutcome::Failed(format!("{source} is not there")));
                    return;
                }
                Err(refusal) => {
                    done(ToolOutcome::Failed(refusal.to_string()));
                    return;
                }
            }
        }
        let target = match space.resolve(to) {
            Ok(path) => path,
            Err(refusal) => {
                done(ToolOutcome::Failed(refusal.to_string()));
                return;
            }
        };
        // pdfunite writes the output last and would happily consume a file it
        // is also reading.
        if resolved.contains(&target) {
            done(ToolOutcome::Failed(
                "the merged file cannot be one of the files being merged — pick a new name".into(),
            ));
            return;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        let count = resolved.len();
        let to = to.to_string();
        run(
            documents::unite_command(&resolved, &target),
            move |output| {
                done(match output {
                    Some(_) if target.is_file() => {
                        ToolOutcome::Ok(format!("Merged {count} PDFs into {to}."))
                    }
                    Some(_) => ToolOutcome::Failed(format!(
                        "pdfunite ran but wrote nothing to {to}; one of the inputs may be damaged"
                    )),
                    None => ToolOutcome::Failed(NO_POPPLER.into()),
                });
            },
        );
    }

    /// Pull pages out into a new PDF.
    ///
    /// `pdfinfo` for the page count, `pdfseparate` once per page, then
    /// `pdfunite` to put the chosen ones back together in the order asked for.
    /// Poppler has no single tool that takes a discontinuous range, and doing
    /// it this way is what makes `7,1-3` mean what it says.
    fn extract_pages<F>(&self, asked: &str, pages: &str, to: &str, done: F)
    where
        F: FnOnce(ToolOutcome) + 'static,
    {
        let Some(space) = self.workspace.as_ref() else {
            done(ToolOutcome::Failed("this context has no workspace".into()));
            return;
        };
        if let Err(complaint) = office::check_extension(to, "pdf") {
            done(ToolOutcome::Failed(complaint));
            return;
        }
        let (source, target) = match (space.resolve(asked), space.resolve(to)) {
            (Ok(source), Ok(target)) => (source, target),
            (Err(refusal), _) | (_, Err(refusal)) => {
                done(ToolOutcome::Failed(refusal.to_string()));
                return;
            }
        };
        if source == target {
            done(ToolOutcome::Failed(
                "write the pages to a new file rather than over the original".into(),
            ));
            return;
        }
        if !source.is_file() {
            done(ToolOutcome::Failed(format!("{asked} is not there")));
            return;
        }

        let (asked, pages, to) = (asked.to_string(), pages.to_string(), to.to_string());
        run(documents::info_command(&source), move |counted| {
            let Some(counted) = counted else {
                done(ToolOutcome::Failed(NO_POPPLER.into()));
                return;
            };
            let total = documents::parse_info(&counted).pages;
            let wanted = match documents::parse_pages(&pages, total) {
                Ok(wanted) => wanted,
                Err(complaint) => {
                    done(ToolOutcome::Failed(complaint));
                    return;
                }
            };
            let Ok(scratch) = tempfile::tempdir() else {
                done(ToolOutcome::Failed(
                    "there is nowhere to put the pages while they are separated".into(),
                ));
                return;
            };
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            separate(
                Extraction {
                    scratch,
                    source,
                    wanted: wanted.into_iter().collect(),
                    separated: Vec::new(),
                    target,
                    to,
                    asked,
                },
                done,
            );
        });
    }

    // -- Python ---------------------------------------------------------------

    /// Write the script into the sandbox directory and run a container over it.
    ///
    /// The script travels as a file in the bind mount rather than as an
    /// argument or down a pipe, which is what lets the argv stay a plain argv
    /// with no shell in it — see [`sandbox::Sandbox::command`].
    ///
    /// The directory is listed either side of the run so the result can name
    /// what the script actually produced. Listing twice is two `read_dir`
    /// walks over a directory holding a handful of files, and it is the
    /// difference between "it says it made a chart" and knowing one is there.
    fn run_python<F>(&self, code: &str, done: F)
    where
        F: FnOnce(ToolOutcome) + 'static,
    {
        let Some(sandbox) = self.sandbox.as_ref() else {
            done(ToolOutcome::Failed(
                "this context has no Python sandbox".into(),
            ));
            return;
        };

        let code = code.trim();
        if code.is_empty() {
            done(ToolOutcome::Failed(sandbox::Refusal::Empty.to_string()));
            return;
        }
        if code.chars().count() > sandbox::MAX_CODE {
            done(ToolOutcome::Failed(
                sandbox::Refusal::TooLong(code.chars().count()).to_string(),
            ));
            return;
        }

        if let Err(error) = sandbox.prepare() {
            done(ToolOutcome::Failed(format!(
                "the sandbox directory could not be made: {error}"
            )));
            return;
        }
        // A trailing newline, because a script whose last line has none is a
        // syntax error in some shapes and never helps in any.
        if let Err(error) = std::fs::write(sandbox.script_path(), format!("{code}\n")) {
            done(ToolOutcome::Failed(format!(
                "the script could not be written to the sandbox: {error}"
            )));
            return;
        }

        let before = sandbox.listing();
        let sandbox = sandbox.clone();
        let started = std::time::Instant::now();
        run_capturing(sandbox.command(), move |finished| {
            let Some((stdout, stderr, code)) = finished else {
                done(ToolOutcome::Failed(sandbox::Trouble::NoPodman.to_string()));
                return;
            };
            // Podman failing to start a container at all is not a failing
            // script, and the two want completely different remedies.
            if let Some(trouble) = sandbox::trouble(&stderr) {
                done(ToolOutcome::Failed(trouble.to_string()));
                return;
            }

            let elapsed = started.elapsed().as_secs();
            let created = sandbox
                .listing()
                .difference(&before)
                .cloned()
                .collect::<Vec<_>>();
            let ran = sandbox::Ran {
                stdout,
                stderr,
                timed_out: sandbox::killed_by_clock(code, elapsed),
                code,
                created,
            };
            // A failing script is `Ok` and not `Failed`: it ran, its traceback
            // is the useful part, and the framing already tells the model to
            // send one correction. `Failed` is for a tool that could not run,
            // which is what the "fix the call or say it could not be done"
            // guidance is written about.
            done(ToolOutcome::Ok(sandbox::frame(&ran)))
        });
    }

    /// Switch capabilities on for this context.
    ///
    /// The names arrive as a JSON array, and a model asked for an array sends a
    /// bare string often enough that refusing one would be a tool failing on an
    /// input it plainly understood.
    fn use_tools(&self, names: Option<&serde_json::Value>) -> ToolOutcome {
        let asked: Vec<String> = match names {
            Some(serde_json::Value::Array(values)) => values
                .iter()
                .filter_map(|value| value.as_str())
                .map(str::to_string)
                .collect(),
            Some(serde_json::Value::String(one)) => vec![one.clone()],
            _ => Vec::new(),
        };
        if asked.is_empty() {
            return ToolOutcome::Failed(
                "`use_tools` needs the name of at least one capability to switch on.".into(),
            );
        }
        let Some(switch) = self.switch_on.as_ref() else {
            return ToolOutcome::Failed(
                "nothing can be switched on from here in this conversation.".into(),
            );
        };
        ToolOutcome::Ok(switch(&asked))
    }

    /// Set, show or clear this chat's schedule.
    fn schedule(&self, action: &str, when: &str, prompt: &str, title: &str) -> ToolOutcome {
        let Some(scheduler) = self.scheduler.as_ref() else {
            return ToolOutcome::Failed("this chat cannot schedule itself".into());
        };
        let action = match action.trim().to_lowercase().as_str() {
            "" => "set".to_string(),
            other => other.to_string(),
        };
        match scheduler(&action, when, prompt, title) {
            Ok(said) => ToolOutcome::Ok(said),
            Err(why) => ToolOutcome::Failed(why),
        }
    }

    /// Plan a job of several steps, or move along one that is already planned.
    ///
    /// A closure for the same reason `schedule` is one: the workflow lives on
    /// the open thread and the saved ones are files under the project, and only
    /// the application owns either. This module stays ignorant of both.
    fn workflow(&self, arguments: &str) -> ToolOutcome {
        let Some(keeper) = self.workflows.as_ref() else {
            return ToolOutcome::Failed("this chat cannot plan a workflow".into());
        };
        match keeper(arguments) {
            Ok(said) => ToolOutcome::Ok(said),
            Err(why) => ToolOutcome::Failed(why),
        }
    }

    /// Copy something the sandbox made into the workspace, through the same
    /// path check every other write goes through.
    fn copy_to_workspace(&self, from: &str, to: &str) -> ToolOutcome {
        let Some(sandbox) = self.sandbox.as_ref() else {
            return ToolOutcome::Failed("this context has no Python sandbox".into());
        };
        let Some(space) = self.workspace.as_ref() else {
            return ToolOutcome::Failed("this context has no workspace to copy into".into());
        };

        // The sandbox root is a workspace as far as path resolution goes: the
        // same predicate, so `../../.ssh/id_rsa` cannot be smuggled out of the
        // sandbox any more than it can be written into the real workspace.
        let source = match Workspace::new(sandbox.root()).resolve(from) {
            Ok(path) if path.is_file() => path,
            Ok(_) => {
                return ToolOutcome::Failed(format!(
                    "{from} is not a file in the sandbox. `run_python` names what a script \
                     created; copy one of those."
                ))
            }
            Err(_) => {
                return ToolOutcome::Failed(format!(
                    "{from} is outside the sandbox. Only a file a script wrote under /work can \
                     be copied out."
                ))
            }
        };

        let bytes = match std::fs::read(&source) {
            Ok(bytes) => bytes,
            Err(error) => return ToolOutcome::Failed(format!("{from} could not be read: {error}")),
        };
        match space.write_bytes(to, &bytes, "the file from the sandbox") {
            Ok(said) => ToolOutcome::Ok(said),
            Err(refusal) => ToolOutcome::Failed(refusal.to_string()),
        }
    }

    // -- mail ------------------------------------------------------------------

    /// Read or organise the user's email.
    ///
    /// The verb is classified before anything connects, so a refusal costs no
    /// round trip, and the result carries `email::note_for` — which is where
    /// the "this is data, not instructions" rule lands at the moment the
    /// untrusted text is actually in front of the model.
    fn mail<F>(&self, args: &[String], done: F)
    where
        F: FnOnce(ToolOutcome) + 'static,
    {
        let Some(account) = self.mail.as_ref() else {
            done(ToolOutcome::Failed(
                "this context has no mail account".into(),
            ));
            return;
        };
        if let crate::model::email::Decision::Refuse(why) = crate::model::email::classify(args) {
            done(ToolOutcome::Failed(why));
            return;
        }
        let verb = crate::model::email::verb(args)
            .unwrap_or_default()
            .to_lowercase();
        crate::ui::mail::run(account, args, move |result| {
            done(match result {
                Ok(text) => ToolOutcome::Ok(tools::framed(
                    &text,
                    crate::model::email::MAX_OUTPUT,
                    crate::model::email::note_for(&verb, &text),
                )),
                Err(why) => ToolOutcome::Failed(why),
            })
        });
    }

    // -- asking a stronger model ----------------------------------------------

    /// Send one question to `claude -p` or `codex exec` and hand back the text.
    ///
    /// Slow — tens of seconds — so it goes down [`run_slow`] like a transcript,
    /// with the wait announced before it starts rather than after three
    /// minutes of nothing. Standard error stays separate for the same reason it
    /// does there: both CLIs write progress and warnings to it, and merging
    /// them into the answer would put a spinner's worth of noise in the middle
    /// of the reply.
    fn escalate<F>(&self, question: &str, tried: &str, done: F)
    where
        F: FnOnce(ToolOutcome) + 'static,
    {
        let Some(escalation) = self.escalation.as_ref() else {
            done(ToolOutcome::Failed(
                "this context cannot ask a stronger model".into(),
            ));
            return;
        };

        let question = question.trim();
        if question.is_empty() {
            done(ToolOutcome::Failed(escalate::Refusal::Empty.to_string()));
            return;
        }
        // What is measured is what would be *sent*, which is the question and
        // whatever was already tried.
        let asked = if tried.trim().is_empty() {
            question.to_string()
        } else {
            format!(
                "{question}\n\nWhat has already been tried:\n{}",
                tried.trim()
            )
        };
        if asked.chars().count() > escalate::MAX_QUESTION {
            done(ToolOutcome::Failed(
                escalate::Refusal::TooLong(asked.chars().count()).to_string(),
            ));
            return;
        }

        let directory = escalate::scratch(&escalation.root);
        if let Err(error) = std::fs::create_dir_all(&directory) {
            done(ToolOutcome::Failed(format!(
                "there is nowhere to run the consultation: {error}"
            )));
            return;
        }

        let backend = escalation.backend;
        let command = escalate::command(backend, escalation.model.as_deref());
        let settle = move |finished: Option<(String, String, i32)>| {
            done(match finished {
                Some((out, _, _)) if !out.trim().is_empty() => ToolOutcome::Ok(tools::framed(
                    &out,
                    escalate::MAX_OUTPUT,
                    escalate::note_for(&out),
                )),
                // Standard error is where both CLIs put "not signed in", which
                // is the failure a user actually hits.
                Some((_, complaint, _)) => ToolOutcome::Failed(format!(
                    "`{}` ran and returned nothing. {}",
                    backend.label(),
                    if complaint.trim().is_empty() {
                        "It may not be signed in — tell the user to check it from a terminal."
                            .to_string()
                    } else {
                        complaint.chars().take(400).collect::<String>()
                    }
                )),
                None => ToolOutcome::Failed(format!(
                    "{}. Tell the user, and answer with what you have.",
                    backend.install_hint()
                )),
            });
        };

        // The consultation runs in the scratch directory, not the workspace:
        // both CLIs read the tree they are standing in, and a question should
        // carry what it says rather than what it was next to. The question
        // itself goes down standard input rather than the command line — see
        // `escalate::command` for both reasons.
        if let Some(progress) = self.progress.clone() {
            progress(&escalate::waiting_for(backend));
        }
        run_capturing_in(
            Some(&directory),
            command,
            Some(escalate::prompt(&asked)),
            settle,
        );
    }

    // -- weather --------------------------------------------------------------

    /// Current conditions, the forecast and any alerts.
    ///
    /// Four requests at worst, chained because each depends on the last:
    /// `/points` gives the grid, the grid gives the station list, and the
    /// station list has to be *walked* — the first entry is regularly not
    /// reporting. Each step's completion starts the next, the same shape as a
    /// round of tools, so the main loop keeps running throughout.
    fn weather<F>(&self, at: Option<Point>, done: F)
    where
        F: FnOnce(ToolOutcome) + 'static,
    {
        let Some(at) = at else {
            done(ToolOutcome::Failed(
                "no location is set. Tell the user to add one in Preferences → Assistant → \
                 Weather, as a latitude and longitude."
                    .into(),
            ));
            return;
        };

        let session = self.web.clone();
        get(
            session.clone(),
            &weather::points_url(&at),
            move |body| match body.as_deref().and_then(weather::parse_points) {
                Some(grid) => forecast(session, grid, done),
                None => done(ToolOutcome::Failed(
                    "that location could not be resolved. The National Weather Service covers \
                     the United States only — say so if the user asked about somewhere else. \
                     If it should have worked, the service returns brief errors that clear \
                     within a few seconds."
                        .into(),
                )),
            },
        );
    }

    // -- GitHub ---------------------------------------------------------------

    /// Run `gh`, in the workspace, as an argv.
    ///
    /// The workspace is the repository: `gh pr list` reads the remote from the
    /// git checkout it is standing in, so without a directory it would fail on
    /// every repository-scoped call. Standard error is captured rather than
    /// silenced, because `gh`'s failures — not a repository, not signed in,
    /// rate limited — are exactly what the model needs to read.
    fn gh<F>(&self, args: &[String], done: F)
    where
        F: FnOnce(ToolOutcome) + 'static,
    {
        let Some(space) = self.workspace.as_ref() else {
            done(ToolOutcome::Failed(
                "this context has no workspace, and `gh` needs a repository to run in".into(),
            ));
            return;
        };
        if let github::Decision::Refuse(why) = github::classify(args) {
            done(ToolOutcome::Failed(why));
            return;
        }

        let command = github::command(args);
        let said = command.join(" ");
        run_in(space.root(), command, move |output| {
            done(match output {
                Some(text) if text.trim().is_empty() => {
                    // `gh pr list` with nothing to list says nothing at all,
                    // and an empty string reads to a model as a failure.
                    ToolOutcome::Ok(format!("`{said}` ran and returned nothing."))
                }
                Some(text) => {
                    let kept: String = text.chars().take(github::MAX_OUTPUT).collect();
                    if kept.len() < text.len() {
                        ToolOutcome::Ok(format!(
                            "{kept}\n\n[cut off after {} characters — narrow it with --limit \
                             or --json]",
                            github::MAX_OUTPUT
                        ))
                    } else {
                        ToolOutcome::Ok(kept)
                    }
                }
                None => ToolOutcome::Failed(
                    "the GitHub CLI could not be run. Tell the user to install `gh` and sign \
                     in with `gh auth login`."
                        .into(),
                ),
            });
        });
    }

    // -- the sibling applications ---------------------------------------------

    /// Run `dynamo agent`, which answers a query against Postgres on the NAS.
    ///
    /// Slower than `planner` — a network round trip over the tailnet rather
    /// than D-Bus to a local process — but still well inside a turn, because
    /// every query is bounded by the resolution Dynamo picks for the period.
    fn dynamo<F>(&self, args: &[String], done: F)
    where
        F: FnOnce(ToolOutcome) + 'static,
    {
        if let dynamo::Decision::Refuse(why) = dynamo::classify(args) {
            done(ToolOutcome::Failed(why));
            return;
        }
        let command = dynamo::command(args);
        run(command, move |output| {
            done(match output {
                Some(text) if !text.trim().is_empty() => ToolOutcome::Ok(tools::framed(
                    &text,
                    dynamo::MAX_OUTPUT,
                    dynamo::note_for(&text),
                )),
                Some(_) => ToolOutcome::Failed(
                    "`dynamo` ran and said nothing, which it should never do — it answers \
                     JSON even for a refusal. Try `describe`."
                        .into(),
                ),
                // Two different problems with one symptom, so name both: the
                // binary missing, and the binary present but unable to reach
                // the database it reads.
                None => ToolOutcome::Failed(
                    "Dynamo could not be run. Either it is not installed — its `install.sh` \
                     puts `dynamo` in ~/.local/bin — or it has no database credentials in \
                     ~/.config/dynamo/config.json."
                        .into(),
                ),
            });
        });
    }

    /// Run `planner agent`, which answers in milliseconds and needs no
    /// directory: it talks to the running Planner over D-Bus, and its store is
    /// wherever Planner keeps it.
    fn planner<F>(&self, args: &[String], done: F)
    where
        F: FnOnce(ToolOutcome) + 'static,
    {
        if let planner::Decision::Refuse(why) = planner::classify(args) {
            done(ToolOutcome::Failed(why));
            return;
        }
        let command = planner::command(args);
        run(command, move |output| {
            done(match output {
                Some(text) if !text.trim().is_empty() => ToolOutcome::Ok(tools::framed(
                    &text,
                    planner::MAX_OUTPUT,
                    planner::note_for(&text),
                )),
                Some(_) => ToolOutcome::Failed(
                    "`planner` ran and said nothing, which it should never do. Try `overview` \
                     to check it is answering at all."
                        .into(),
                ),
                None => ToolOutcome::Failed(
                    "Planner could not be run. Tell the user to install it — its `install.sh` \
                     puts `planner` in ~/.local/bin."
                        .into(),
                ),
            });
        });
    }

    /// Run `magpie agent`, in the workspace when there is one.
    ///
    /// The directory matters for exactly one argument: `dir=` is resolved
    /// against the working directory Magpie was invoked from, so running in the
    /// workspace makes `dir=notes` mean what it means everywhere else here.
    ///
    /// `transcribe` goes down [`run_slow`], which keeps standard error separate
    /// — Magpie writes JSON to one and progress to the other, and merging them
    /// makes the JSON unparseable — and reports each progress line as it
    /// arrives rather than after the four minutes are up.
    fn magpie<F>(&self, args: &[String], done: F)
    where
        F: FnOnce(ToolOutcome) + 'static,
    {
        if let magpie::Decision::Refuse(why) = magpie::classify(args) {
            done(ToolOutcome::Failed(why));
            return;
        }
        let command = magpie::command(args);
        let settle = |output: Option<String>| {
            done(match output {
                Some(text) if !text.trim().is_empty() => ToolOutcome::Ok(tools::framed(
                    &text,
                    magpie::MAX_OUTPUT,
                    magpie::note_for(&text),
                )),
                Some(_) => ToolOutcome::Failed(
                    "`magpie` ran and said nothing. Try `tools` to check it is answering.".into(),
                ),
                None => ToolOutcome::Failed(
                    "Magpie could not be run. Tell the user to install it — its `install.sh` \
                     puts `magpie` in ~/.local/bin. It is a desktop application, so it also \
                     needs a running session."
                        .into(),
                ),
            });
        };

        let directory = self
            .workspace
            .as_ref()
            .map(|space| space.root().to_path_buf());
        if !magpie::is_slow(args) {
            spawn(
                directory.as_deref(),
                command,
                gtk::gio::SubprocessFlags::STDERR_SILENCE,
                settle,
            );
            return;
        }

        if let Some(progress) = self.progress.clone() {
            if let Some(waiting) = magpie::waiting_for(args) {
                progress(&waiting);
            }
            run_slow(
                directory.as_deref(),
                command,
                move |line| progress(line),
                settle,
            );
            return;
        }
        run_slow(directory.as_deref(), command, |_| {}, settle);
    }

    // -- the web --------------------------------------------------------------

    fn search<F>(&self, query: &str, results: usize, done: F)
    where
        F: FnOnce(ToolOutcome) + 'static,
    {
        let query = query.trim().to_string();
        if query.is_empty() {
            done(ToolOutcome::Failed("a query is needed".into()));
            return;
        }
        let Some(key) = self.exa_key.clone() else {
            done(ToolOutcome::Failed(no_key()));
            return;
        };

        let request = SearchRequest::new(&query, results);
        let Ok(body) = serde_json::to_vec(&request) else {
            done(ToolOutcome::Failed("could not build that search".into()));
            return;
        };

        client::post_json(&self.web, web::SEARCH_URL, &key, body, move |answer| {
            done(match answer {
                Ok(body) => match serde_json::from_str::<SearchResponse>(&body) {
                    Ok(response) => ToolOutcome::Ok(response.for_model(&query)),
                    Err(error) => {
                        ToolOutcome::Failed(format!("Exa answered something unexpected ({error})"))
                    }
                },
                Err(error) => ToolOutcome::Failed(explain(&error)),
            });
        });
    }

    /// Research a subject's recent history across every lane at once.
    ///
    /// The other tools here are one request, or a chain of them — the weather
    /// resolves a grid and then asks for a forecast. This is the opposite
    /// shape: every lane goes out together and the answer is assembled when the
    /// last one is back. Doing them in sequence would multiply four round trips
    /// by Exa's latency for no gain, since none of them needs another's result.
    ///
    /// The counter is what makes it safe. Each callback pushes what it got and
    /// decrements; only the one that takes it to zero renders the brief, so the
    /// contract every tool here keeps — answer exactly once — holds no matter
    /// what order the lanes come back in, or how many of them fail. A lane that
    /// fails contributes no items and its name to `missing`, because a thin
    /// brief that admits what it is missing is worth more than an error.
    fn news<F>(&self, topic: Option<String>, days: i64, done: F)
    where
        F: FnOnce(ToolOutcome) + 'static,
    {
        let Some(key) = self.exa_key.clone() else {
            done(ToolOutcome::Failed(no_key()));
            return;
        };
        let window = news::Window::of(days, Utc::now());
        // Both derived before anything is spawned, so the topic can be moved
        // into the closure that renders the brief.
        let (angles, hn) = match topic.as_deref() {
            Some(topic) => (news::angles(topic), news::hn_url(topic, &window)),
            None => (news::trending_angles(), news::hn_front_page_url()),
        };

        // Every lane's findings, and the ones that never answered.
        let gathered: Rc<RefCell<Vec<news::Item>>> = Rc::new(RefCell::new(Vec::new()));
        let missing: Rc<RefCell<Vec<news::Lane>>> = Rc::new(RefCell::new(Vec::new()));
        let pending = Rc::new(std::cell::Cell::new(angles.len() + 1));
        let finish = Rc::new(RefCell::new(Some(done)));

        // Called by every lane, and acted on by whichever is last.
        let settle = {
            let gathered = gathered.clone();
            let missing = missing.clone();
            let pending = pending.clone();
            let finish = finish.clone();
            move || {
                pending.set(pending.get().saturating_sub(1));
                if pending.get() > 0 {
                    return;
                }
                let Some(done) = finish.borrow_mut().take() else {
                    return;
                };
                let stories = news::rank(gathered.borrow_mut().split_off(0), &window);
                done(ToolOutcome::Ok(news::brief(
                    topic.as_deref(),
                    &window,
                    &stories,
                    &missing.borrow(),
                )));
            }
        };

        for angle in angles {
            let Ok(body) = serde_json::to_vec(&news::exa_request(&angle, &window)) else {
                missing.borrow_mut().push(angle.lane);
                settle();
                continue;
            };
            let gathered = gathered.clone();
            let missing = missing.clone();
            let settle = settle.clone();
            client::post_json(&self.web, web::SEARCH_URL, &key, body, move |answer| {
                match answer
                    .ok()
                    .and_then(|body| serde_json::from_str::<SearchResponse>(&body).ok())
                {
                    Some(response) => gathered
                        .borrow_mut()
                        .extend(news::from_exa(&response, angle.lane, &window)),
                    None => missing.borrow_mut().push(angle.lane),
                }
                settle();
            });
        }

        // Hacker News, which needs no key and is the only lane that reports
        // what a story actually scored.
        let hits = gathered.clone();
        let absent = missing.clone();
        client::get_with_agent(
            &self.web,
            &hn,
            weather::USER_AGENT,
            "application/json",
            move |body| {
                match body {
                    Some(body) => hits.borrow_mut().extend(news::from_hn(&body)),
                    None => absent.borrow_mut().push(news::Lane::Engagement),
                }
                settle();
            },
        );
    }

    fn fetch<F>(&self, url: &str, done: F)
    where
        F: FnOnce(ToolOutcome) + 'static,
    {
        let url = url.trim().to_string();
        if !url.starts_with("http://") && !url.starts_with("https://") {
            done(ToolOutcome::Failed(
                "that is not an http or https URL".into(),
            ));
            return;
        }
        let Some(key) = self.exa_key.clone() else {
            done(ToolOutcome::Failed(no_key()));
            return;
        };

        let Ok(body) = serde_json::to_vec(&ContentsRequest::new(&url)) else {
            done(ToolOutcome::Failed("could not build that request".into()));
            return;
        };

        client::post_json(&self.web, web::CONTENTS_URL, &key, body, move |answer| {
            done(match answer {
                Ok(body) => match serde_json::from_str::<SearchResponse>(&body) {
                    Ok(response) if !response.results.is_empty() => {
                        ToolOutcome::Ok(response.for_model(&url))
                    }
                    Ok(_) => ToolOutcome::Failed(format!("nothing could be read from {url}")),
                    Err(error) => {
                        ToolOutcome::Failed(format!("Exa answered something unexpected ({error})"))
                    }
                },
                Err(error) => ToolOutcome::Failed(explain(&error)),
            });
        });
    }
}

/// What a document tool says when it was handed nothing to write.
const EMPTY: &str = "there is no content to put in it — pass the document's text as `markdown`";

const NO_POPPLER: &str =
    "working with PDFs needs poppler-utils (pdfinfo, pdftotext, pdfunite, pdfseparate). Tell \
     the user to install it.";

/// Run a command for the lookout, which has no `Runner` and wants only the
/// output. Public because the application gathers its signals directly.
pub fn run_for_signals<F>(command: Vec<String>, done: F)
where
    F: FnOnce(Option<String>) + 'static,
{
    run(command, done);
}

/// Active warnings for a place, for the lookout, as one line each.
///
/// Two requests rather than the six the `weather` tool makes: the proactive
/// check has no use for the forecast or the current conditions, and a
/// background pass that fires every few hours should cost what it needs and
/// nothing more.
///
/// The line carries the event, when it ends and what the service says about it,
/// because "Severe Thunderstorm Warning" alone was not enough for the model to
/// connect a warning to an afternoon spent on a roof — and the sentence that
/// makes the connection obvious is one the National Weather Service has already
/// written.
pub fn alerts_for_signals<F>(at: crate::model::weather::Point, done: F)
where
    F: FnOnce(Vec<String>) + 'static,
{
    let session = client::web_session();
    get(
        session.clone(),
        &weather::points_url(&at),
        move |body| match body.as_deref().and_then(weather::parse_points) {
            Some(grid) => get(session, &weather::alerts_url(&grid), move |body| {
                let found = body
                    .as_deref()
                    .map(weather::parse_alerts)
                    .unwrap_or_default();
                done(found.iter().map(one_line).collect());
            }),
            None => done(Vec::new()),
        },
    );
}

/// One alert, short enough for a signal and specific enough to be acted on.
fn one_line(alert: &crate::model::weather::Alert) -> String {
    let mut line = alert.event.clone();
    if let Some(ends) = &alert.ends {
        line.push_str(&format!(" until {ends}"));
    }
    // The headline is a sentence; the description is several paragraphs of
    // them. The first sentence of whichever there is says what the hazard
    // actually is, which is the part that collides with somebody's day.
    let said = alert.headline.as_deref().or(alert.description.as_deref());
    if let Some(said) = said {
        let first: String = said
            .split_whitespace()
            .take(30)
            .collect::<Vec<_>>()
            .join(" ");
        if !first.is_empty() {
            line.push_str(" — ");
            line.push_str(&first);
        }
    }
    line
}

/// Run a command and hand back its standard output, or `None` if it could not
/// be started. On the GLib main loop, so a slow document does not freeze the
/// window mid-turn — the same shape the composer uses for a dropped PDF.
fn run<F>(command: Vec<String>, done: F)
where
    F: FnOnce(Option<String>) + 'static,
{
    spawn(
        None,
        command,
        gtk::gio::SubprocessFlags::STDERR_SILENCE,
        done,
    );
}

/// The same, in a directory and keeping standard error.
///
/// `gh` needs both: the working directory is which repository it acts on, and
/// its diagnostics — not a repository, not signed in, rate limited — go to
/// standard error, which is exactly what the model has to read to recover.
fn run_in<F>(directory: &std::path::Path, command: Vec<String>, done: F)
where
    F: FnOnce(Option<String>) + 'static,
{
    spawn(
        Some(directory),
        command,
        gtk::gio::SubprocessFlags::STDERR_MERGE,
        done,
    );
}

/// Run a command and hand back standard output, standard error and the exit
/// code, all three.
///
/// The other runners here need one of those. This one needs all of them and
/// cannot merge any: a Python traceback goes to standard error while the answer
/// goes to standard output, merging them would put the traceback in the middle
/// of the result, and the exit code is the only thing that says which of the
/// two actually happened — a script is free to print a warning to stderr and
/// succeed.
///
/// `None` means the process could not be started at all, which for this caller
/// means podman is not installed.
fn run_capturing<F>(command: Vec<String>, done: F)
where
    F: FnOnce(Option<(String, String, i32)>) + 'static,
{
    run_capturing_in(None, command, None, done);
}

/// The same, in a directory and with something to say on standard input.
///
/// `input` is for a program that takes its subject that way rather than as an
/// argument. `escalate` is the one that does, and it does it because an
/// argument would be both eaten by the CLI's variadic options and readable by
/// anyone running `ps` — see `model::escalate::command`.
fn run_capturing_in<F>(
    directory: Option<&std::path::Path>,
    command: Vec<String>,
    input: Option<String>,
    done: F,
) where
    F: FnOnce(Option<(String, String, i32)>) + 'static,
{
    use gtk::gio;

    let arguments: Vec<&std::ffi::OsStr> = command.iter().map(std::ffi::OsStr::new).collect();
    let mut flags = gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE;
    if input.is_some() {
        flags |= gio::SubprocessFlags::STDIN_PIPE;
    }
    let launcher = gio::SubprocessLauncher::new(flags);
    if let Some(directory) = directory {
        launcher.set_cwd(directory);
    }
    let Ok(process) = launcher.spawn(&arguments) else {
        done(None);
        return;
    };

    let waited = process.clone();
    // `communicate_utf8_async` writes the input, closes the pipe, and reads
    // both outputs to the end — which is the whole exchange in one call, and
    // keeps the main loop running throughout.
    process.communicate_utf8_async(input, gio::Cancellable::NONE, move |result| {
        let Ok((out, error)) = result else {
            done(None);
            return;
        };
        done(Some((
            out.map(|text| text.to_string()).unwrap_or_default(),
            error.map(|text| text.to_string()).unwrap_or_default(),
            exit_code(&waited),
        )));
    });
}

/// A finished process's exit code, as a shell would report it.
///
/// `exit_status` is only meaningful for a process that exited normally, and
/// asking for it after a signal is a glib critical rather than an answer. A
/// signalled process is reported the way a shell does — 128 plus the signal —
/// so the caller has one number to reason about either way.
fn exit_code(process: &gtk::gio::Subprocess) -> i32 {
    // `has_exited` is glib's `if_exited`: whether it ended normally rather than
    // whether it ended at all.
    if process.has_exited() {
        return process.exit_status();
    }
    if process.has_signaled() {
        return 128 + process.term_sig();
    }
    // Neither exited nor signalled, which should not happen once `communicate`
    // has completed. Not zero: reporting a script that never finished as a
    // success is the one wrong answer here.
    1
}

/// Run something that takes minutes, reporting progress as it arrives.
///
/// Three things separate this from [`run`], and each of them is a bug if it is
/// got wrong:
///
/// * **Standard error stays separate.** Magpie writes one JSON object to stdout
///   and a progress line per percent to stderr. [`run_in`]'s `STDERR_MERGE`
///   would drop those lines into the middle of the JSON and nothing would parse.
/// * **Progress is read while the process runs**, line by line, rather than
///   collected at the end where it would be a wall of text nobody sees. This is
///   why `communicate_utf8_async` is not used: it answers once, at the end.
/// * **There is no timeout, deliberately.** An hour of conference audio is an
///   hour of conference audio. A timeout here would kill a job that was working
///   and leave a part-finished download behind, and the user already has a
///   cancel button — in Magpie's own window, where the job actually lives.
///
/// `done` fires when standard output reaches end of file, which is when the
/// JSON is complete.
fn run_slow<P, F>(directory: Option<&std::path::Path>, command: Vec<String>, progress: P, done: F)
where
    P: Fn(&str) + 'static,
    F: FnOnce(Option<String>) + 'static,
{
    use gtk::gio;

    let arguments: Vec<&std::ffi::OsStr> = command.iter().map(std::ffi::OsStr::new).collect();
    let launcher = gio::SubprocessLauncher::new(
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE,
    );
    if let Some(directory) = directory {
        launcher.set_cwd(directory);
    }
    let Ok(process) = launcher.spawn(&arguments) else {
        done(None);
        return;
    };
    let process = Rc::new(process);

    if let Some(errors) = process.stderr_pipe() {
        read_progress(
            gio::DataInputStream::new(&errors),
            Rc::new(progress),
            process.clone(),
        );
    }
    let Some(output) = process.stdout_pipe() else {
        done(None);
        return;
    };
    let done: Box<dyn FnOnce(Option<String>)> = Box::new(done);
    gather(
        gio::DataInputStream::new(&output),
        String::new(),
        Rc::new(RefCell::new(Some(done))),
        process,
    );
}

/// Read stderr a line at a time until it ends, reporting each one.
///
/// The process is carried along so it outlives the reads; nothing else holds it
/// once `run_slow` has returned.
#[allow(
    clippy::only_used_in_recursion,
    reason = "`process` is an ownership anchor, not an input: it keeps the child \
              alive while its pipes are still being read, and nothing else holds \
              it once `run_slow` has returned"
)]
fn read_progress(
    stream: gtk::gio::DataInputStream,
    progress: Progress,
    process: Rc<gtk::gio::Subprocess>,
) {
    use gtk::prelude::*;
    stream.clone().read_line_async(
        gtk::glib::Priority::DEFAULT,
        gtk::gio::Cancellable::NONE,
        move |line| {
            // `None` is end of file, and an error is one too as far as this is
            // concerned: progress is decoration, and losing it must never stop
            // the output being collected.
            let Ok(Some(bytes)) = line else { return };
            let said = String::from_utf8_lossy(&bytes).trim().to_string();
            if !said.is_empty() {
                progress(&said);
            }
            read_progress(stream, progress, process);
        },
    );
}

/// Read stdout to the end, then answer exactly once.
#[allow(
    clippy::only_used_in_recursion,
    reason = "`process` is an ownership anchor, as in `read_progress`"
)]
fn gather(
    stream: gtk::gio::DataInputStream,
    mut collected: String,
    done: Settle,
    process: Rc<gtk::gio::Subprocess>,
) {
    use gtk::prelude::*;
    stream.clone().read_line_async(
        gtk::glib::Priority::DEFAULT,
        gtk::gio::Cancellable::NONE,
        move |line| match line {
            Ok(Some(bytes)) => {
                collected.push_str(&String::from_utf8_lossy(&bytes));
                collected.push('\n');
                gather(stream, collected, done, process);
            }
            // End of file, or a read that failed. Either way this is everything
            // there is going to be, and the contract is to answer exactly once.
            _ => {
                if let Some(done) = done.borrow_mut().take() {
                    done(Some(collected));
                }
            }
        },
    );
}

fn spawn<F>(
    directory: Option<&std::path::Path>,
    command: Vec<String>,
    stderr: gtk::gio::SubprocessFlags,
    done: F,
) where
    F: FnOnce(Option<String>) + 'static,
{
    // An argv, never a command line: there is no shell here, so a `;` or a `|`
    // in an argument is a string the program will reject rather than an
    // operator this process obeys.
    let arguments: Vec<&std::ffi::OsStr> = command.iter().map(std::ffi::OsStr::new).collect();
    let launcher =
        gtk::gio::SubprocessLauncher::new(gtk::gio::SubprocessFlags::STDOUT_PIPE | stderr);
    if let Some(directory) = directory {
        launcher.set_cwd(directory);
    }
    let Ok(process) = launcher.spawn(&arguments) else {
        done(None);
        return;
    };
    process.communicate_utf8_async(None, gtk::gio::Cancellable::NONE, move |result| {
        done(
            result
                .ok()
                .and_then(|(out, _)| out)
                .map(|out| out.to_string()),
        );
    });
}

/// One page extraction in progress.
///
/// `pdfseparate` handles one page per call, so the pages are pulled out one at
/// a time and each call's completion starts the next — the same sequential
/// shape `run_next` uses for a round of tools, and for the same reason: the
/// main loop keeps running between them.
struct Extraction {
    /// Held, not borrowed: the separated pages live here until `pdfunite` has
    /// read them, so dropping it early would delete them mid-job.
    scratch: tempfile::TempDir,
    source: std::path::PathBuf,
    /// Still to pull out, in the order asked for.
    wanted: std::collections::VecDeque<usize>,
    /// Pulled out already, in that same order.
    separated: Vec<std::path::PathBuf>,
    target: std::path::PathBuf,
    to: String,
    asked: String,
}

fn separate<F>(mut job: Extraction, done: F)
where
    F: FnOnce(ToolOutcome) + 'static,
{
    let Some(page) = job.wanted.pop_front() else {
        unite(job, done);
        return;
    };
    // A page asked for twice needs two files, so the name carries the position
    // in the output rather than the page number.
    let pattern = job
        .scratch
        .path()
        .join(format!("p{}-%d.pdf", job.separated.len()));
    let written = job
        .scratch
        .path()
        .join(format!("p{}-{page}.pdf", job.separated.len()));

    let command = documents::separate_page_command(&job.source, page, &pattern);
    run(command, move |output| {
        if output.is_none() {
            done(ToolOutcome::Failed(NO_POPPLER.into()));
            return;
        }
        if !written.is_file() {
            done(ToolOutcome::Failed(format!(
                "page {page} could not be taken out of {}",
                job.asked
            )));
            return;
        }
        job.separated.push(written);
        separate(job, done);
    });
}

fn unite<F>(job: Extraction, done: F)
where
    F: FnOnce(ToolOutcome) + 'static,
{
    // pdfunite needs two inputs; a single page is already the file we want.
    if job.separated.len() == 1 {
        let outcome = match std::fs::copy(&job.separated[0], &job.target) {
            Ok(_) => ToolOutcome::Ok(format!("Wrote 1 page from {} to {}.", job.asked, job.to)),
            Err(error) => ToolOutcome::Failed(format!("{} could not be written: {error}", job.to)),
        };
        done(outcome);
        return;
    }

    let count = job.separated.len();
    let command = documents::unite_command(&job.separated, &job.target);
    run(command, move |output| {
        // `job` is held across the call so the scratch directory outlives it.
        let job = job;
        done(match output {
            Some(_) if job.target.is_file() => ToolOutcome::Ok(format!(
                "Wrote {count} page(s) from {} to {}.",
                job.asked, job.to
            )),
            Some(_) => ToolOutcome::Failed(format!("nothing was written to {}", job.to)),
            None => ToolOutcome::Failed(NO_POPPLER.into()),
        });
    });
}

/// The forecast, which is the half worth having.
///
/// It comes first so that everything after it is allowed to fail: a grid with
/// no reporting station still answers the question, and losing the week to a
/// silent weather station would be losing the useful part to the optional one.
fn forecast<F>(session: soup::Session, grid: weather::Grid, done: F)
where
    F: FnOnce(ToolOutcome) + 'static,
{
    get(
        session.clone(),
        &weather::forecast_url(&grid),
        move |body| {
            let periods = body
                .as_deref()
                .map(weather::parse_forecast)
                .unwrap_or_default();
            // The hourly endpoint is 156 periods and 164 KB uncompressed — 5 KB
            // on the wire, because `get` asks for gzip. Only the next few hours
            // are kept.
            get(session.clone(), &weather::hourly_url(&grid), move |body| {
                let hours = body
                    .as_deref()
                    .map(weather::parse_hourly)
                    .unwrap_or_default();
                alerts(session, grid, hours, periods, done);
            });
        },
    );
}

fn alerts<F>(
    session: soup::Session,
    grid: weather::Grid,
    hours: Vec<weather::Hour>,
    periods: Vec<weather::Period>,
    done: F,
) where
    F: FnOnce(ToolOutcome) + 'static,
{
    get(session.clone(), &weather::alerts_url(&grid), move |body| {
        let alerts = body
            .as_deref()
            .map(weather::parse_alerts)
            .unwrap_or_default();
        get(
            session.clone(),
            &weather::stations_url(&grid),
            move |body| {
                let stations = body
                    .as_deref()
                    .map(weather::parse_stations)
                    .unwrap_or_default();
                observe(session, stations.into(), grid, hours, periods, alerts, done);
            },
        );
    });
}

/// Try each station in turn until one has an observation, then frame the lot.
///
/// A loop rather than "take the first" because for grid `ILN 74,80` the list
/// begins with `KOSU`, whose latest observation is a 404. Taking the first
/// would report no current conditions for a location whose second station is
/// reporting perfectly — and would do it every single time.
#[allow(clippy::too_many_arguments)]
fn observe<F>(
    session: soup::Session,
    mut stations: std::collections::VecDeque<String>,
    grid: weather::Grid,
    hours: Vec<weather::Hour>,
    periods: Vec<weather::Period>,
    alerts: Vec<weather::Alert>,
    done: F,
) where
    F: FnOnce(ToolOutcome) + 'static,
{
    let Some(station) = stations.pop_front() else {
        // Every station was silent. The forecast is still the useful half, and
        // the framing says plainly that the current conditions are missing.
        done(ToolOutcome::Ok(weather::frame(
            grid.place.as_deref(),
            None,
            &hours,
            &periods,
            &alerts,
        )));
        return;
    };

    let url = weather::observation_url(&station);
    get(session.clone(), &url, move |body| {
        match body
            .as_deref()
            .and_then(|body| weather::parse_observation(&station, body))
        {
            Some(observation) => done(ToolOutcome::Ok(weather::frame(
                grid.place.as_deref(),
                Some(&observation),
                &hours,
                &periods,
                &alerts,
            ))),
            None => observe(session, stations, grid, hours, periods, alerts, done),
        }
    });
}

/// A plain GET that hands back the body, or nothing.
///
/// Takes the session by value: it is a refcounted GObject, so a clone is a
/// pointer bump, and every caller here needs to keep using it inside the
/// callback it just handed the borrow to.
fn get<F>(session: soup::Session, url: &str, done: F)
where
    F: FnOnce(Option<String>) + 'static,
{
    client::get_with_agent(
        &session,
        url,
        weather::USER_AGENT,
        "application/geo+json",
        done,
    );
}

/// Hand over one skill's instructions.
fn read_skill(name: &str) -> ToolOutcome {
    match office::skills::named(name) {
        Some(skill) => ToolOutcome::Ok(skill.document()),
        None => {
            let known: Vec<&str> = office::skills::ALL.iter().map(|s| s.name).collect();
            ToolOutcome::Failed(format!(
                "there is no skill called {name:?}. There is one for each of: {}",
                known.join(", ")
            ))
        }
    }
}

/// A spreadsheet cell from whatever JSON the model wrote.
///
/// A model asked for strings sends numbers about half the time, and refusing
/// the whole workbook over it would be a tool that fails on its most common
/// correct-in-spirit input.
fn cell(value: &serde_json::Value) -> office::xlsx::Cell {
    use office::xlsx::Cell;
    match value {
        serde_json::Value::Null => Cell::Empty,
        serde_json::Value::Bool(yes) => Cell::Bool(*yes),
        serde_json::Value::Number(number) => number
            .as_f64()
            .map(Cell::Number)
            .unwrap_or_else(|| Cell::Text(number.to_string())),
        serde_json::Value::String(text) => Cell::infer(text),
        other => Cell::Text(other.to_string()),
    }
}

/// A slide bullet, taking its nesting from the leading spaces.
fn bullet(line: &str) -> office::pptx::Bullet {
    let indent = line.chars().take_while(|c| *c == ' ' || *c == '\t').count();
    let text = line.trim_start();
    // Markdown habits are hard to break, and a model writes "- point" inside a
    // bullets list often enough that the dash would otherwise be rendered.
    let text = text
        .strip_prefix("- ")
        .or_else(|| text.strip_prefix("* "))
        .unwrap_or(text);
    office::pptx::Bullet::new((indent / 2) as u8, text.trim())
}

/// What a written document says, with a word about what went into it.
fn report(
    written: Result<String, workspace::Refusal>,
    blocks: &[office::markup::Block],
) -> ToolOutcome {
    match written {
        Ok(said) => {
            let headings = blocks
                .iter()
                .filter(|block| matches!(block, office::markup::Block::Heading { .. }))
                .count();
            ToolOutcome::Ok(format!(
                "{said} {} block(s), {headings} heading(s).",
                blocks.len()
            ))
        }
        Err(refusal) => ToolOutcome::Failed(refusal.to_string()),
    }
}

fn no_key() -> String {
    "web search needs an Exa API key. Tell the user to add one in Preferences → \
     Assistant → Web, from dashboard.exa.ai/api-keys, and answer from what you \
     know in the meantime."
        .to_string()
}

/// A transport failure the model can act on rather than a stack of jargon.
fn explain(error: &client::ClientError) -> String {
    match error {
        client::ClientError::Http { status: 401, .. } => {
            "Exa rejected the API key. Tell the user to check it in Preferences.".into()
        }
        client::ClientError::Http { status: 429, .. } => {
            "Exa is rate-limiting these searches. Wait before trying again, and say so.".into()
        }
        other => format!("the search could not be run ({other})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A vault whose usage ledger is inside it.
    ///
    /// `Memory::open` puts the ledger under `$XDG_DATA_HOME/familiar/`, which
    /// without an override is the real one — a test that opened a vault would
    /// write counts into the notes you use every day.
    fn memory(root: &std::path::Path) -> Rc<RefCell<Option<Memory>>> {
        Rc::new(RefCell::new(Some(Memory::open_with(
            root,
            root.join(".ledger.json"),
        ))))
    }

    fn runner(root: &std::path::Path) -> Runner {
        Runner::new(memory(root), Some("test-key".into()))
    }

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: name.into(),
            arguments: arguments.into(),
            complete: true,
            outcome: None,
        }
    }

    /// Run a tool that answers immediately, and return what it said.
    fn immediate(runner: &Runner, call: &ToolCall) -> ToolOutcome {
        let seen: Rc<RefCell<Option<ToolOutcome>>> = Rc::new(RefCell::new(None));
        let recorded = seen.clone();
        runner.run(call, move |outcome| *recorded.borrow_mut() = Some(outcome));
        let answer = seen.borrow_mut().take();
        answer.expect("it answered immediately")
    }

    #[test]
    fn remembering_then_recalling_finds_what_was_saved() {
        let directory = tempfile::tempdir().expect("temp dir");
        let runner = runner(directory.path());

        let saved = immediate(
            &runner,
            &call(
                "remember",
                r#"{"subject":"Matthew","observation":"writes Rust for GNOME"}"#,
            ),
        );
        assert!(matches!(saved, ToolOutcome::Ok(ref text) if text.contains("Matthew.md")));

        let found = immediate(&runner, &call("recall", r#"{"query":"Matthew"}"#));
        match found {
            ToolOutcome::Ok(text) => assert!(text.contains("Matthew"), "{text}"),
            other => panic!("expected a hit, got {other:?}"),
        }
    }

    #[test]
    fn remembering_says_which_kind_it_filed_it_under() {
        // The kind decides whether the model still has this in front of it in
        // six weeks, so it is worth saying out loud rather than leaving the
        // user to open the note.
        let directory = tempfile::tempdir().expect("temp dir");
        let runner = runner(directory.path());
        let saved = immediate(
            &runner,
            &call(
                "remember",
                r#"{"subject":"Matthew","observation":"wants metric only","kind":"preference"}"#,
            ),
        );
        match saved {
            ToolOutcome::Ok(said) => assert!(said.contains("preference"), "{said}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unrecognised_kind_is_a_fact_rather_than_a_failure() {
        // A small model writes a word nobody declared often enough that
        // refusing would cost a memory. `fact` is the least privileged reading:
        // it decays fastest and does not ride in every prompt.
        let directory = tempfile::tempdir().expect("temp dir");
        let runner = runner(directory.path());
        let saved = immediate(
            &runner,
            &call(
                "remember",
                r#"{"subject":"Matthew","observation":"wants metric only","kind":"standing-order"}"#,
            ),
        );
        match saved {
            ToolOutcome::Ok(said) => assert!(said.contains("fact"), "{said}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn saving_the_same_thing_twice_says_so_rather_than_writing_it_again() {
        let directory = tempfile::tempdir().expect("temp dir");
        let runner = runner(directory.path());
        let arguments = r#"{"subject":"Matthew","observation":"writes Rust for GNOME"}"#;
        immediate(&runner, &call("remember", arguments));
        let again = immediate(&runner, &call("remember", arguments));
        match again {
            ToolOutcome::Ok(said) => assert!(said.contains("Already saved"), "{said}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_hit_the_vectors_liked_and_the_words_did_not_is_marked_as_related() {
        // A model that cannot tell the two apart reports a near-miss as the
        // thing that was asked for, which is the failure mode semantic search
        // introduces and lexical search never had.
        let found = [crate::model::memory::Recalled {
            title: "Contractors".into(),
            path: "Familiar/Contractors.md".into(),
            excerpt: "Vandenberg Roofing did the roof.".into(),
            observations: Vec::new(),
            lexical: false,
            semantic: true,
        }];
        match Runner::recalled("gutter work", &found) {
            ToolOutcome::Ok(said) => assert!(said.contains("not an exact match"), "{said}"),
            other => panic!("{other:?}"),
        }

        let exact = [crate::model::memory::Recalled {
            lexical: true,
            ..found[0].clone()
        }];
        match Runner::recalled("Vandenberg", &exact) {
            ToolOutcome::Ok(said) => assert!(!said.contains("not an exact match"), "{said}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn recalling_nothing_is_an_answer_not_a_failure() {
        let directory = tempfile::tempdir().expect("temp dir");
        let found = immediate(
            &runner(directory.path()),
            &call("recall", r#"{"query":"nothing"}"#),
        );
        assert!(matches!(found, ToolOutcome::Ok(ref text) if text.starts_with("No notes")));
    }

    #[test]
    fn malformed_arguments_are_explained_rather_than_fatal() {
        let directory = tempfile::tempdir().expect("temp dir");
        let outcome = immediate(
            &runner(directory.path()),
            &call("recall", "{query: Matthew"),
        );
        match outcome {
            ToolOutcome::Failed(text) => assert!(text.contains("valid JSON"), "{text}"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn the_workspace_tools_refuse_a_path_outside_it() {
        let directory = tempfile::tempdir().expect("temp dir");
        let space = tempfile::tempdir().expect("temp dir");
        let runner = Runner::new(memory(directory.path()), None)
            .with_workspace(Some(Workspace::new(space.path())));

        let outcome = immediate(
            &runner,
            &call("read_file", r#"{"path":"../../etc/passwd"}"#),
        );
        match outcome {
            ToolOutcome::Failed(text) => assert!(text.contains("outside the workspace"), "{text}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn without_a_workspace_the_tools_say_so() {
        let directory = tempfile::tempdir().expect("temp dir");
        let outcome = immediate(
            &runner(directory.path()),
            &call("read_file", r#"{"path":"a.txt"}"#),
        );
        assert!(matches!(outcome, ToolOutcome::Failed(ref t) if t.contains("no workspace")));
    }

    #[test]
    fn an_unknown_tool_says_so() {
        let directory = tempfile::tempdir().expect("temp dir");
        let outcome = immediate(&runner(directory.path()), &call("rm_rf", "{}"));
        assert!(matches!(outcome, ToolOutcome::Failed(ref text) if text.contains("no tool")));
    }

    #[test]
    fn without_a_key_the_search_says_where_to_get_one() {
        let directory = tempfile::tempdir().expect("temp dir");
        let runner = Runner::new(memory(directory.path()), None);
        let outcome = immediate(&runner, &call("web_search", r#"{"query":"x"}"#));
        match outcome {
            ToolOutcome::Failed(text) => {
                assert!(text.contains("Exa API key"), "{text}");
                assert!(text.contains("dashboard.exa.ai"), "{text}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn fetching_something_that_is_not_a_url_never_leaves_the_machine() {
        let directory = tempfile::tempdir().expect("temp dir");
        let outcome = immediate(
            &runner(directory.path()),
            &call("fetch_url", r#"{"url":"file:///etc/passwd"}"#),
        );
        assert!(matches!(outcome, ToolOutcome::Failed(ref text) if text.contains("http")));
    }

    #[test]
    fn a_tool_answers_exactly_once() {
        // The application settles a round when every call has answered, so a
        // tool that answered twice would run the next round twice.
        let directory = tempfile::tempdir().expect("temp dir");
        let count = Rc::new(Cell::new(0));
        runner(directory.path()).run(&call("recall", r#"{"query":"x"}"#), {
            let count = count.clone();
            move |_| count.set(count.get() + 1)
        });
        assert_eq!(count.get(), 1);
    }

    // -- documents ------------------------------------------------------------

    /// A runner with a workspace and no vault, which is what a document
    /// context looks like.
    fn in_space(root: &std::path::Path) -> Runner {
        Runner::new(Rc::new(RefCell::new(None)), None).with_workspace(Some(Workspace::new(root)))
    }

    fn ok(outcome: ToolOutcome) -> String {
        match outcome {
            ToolOutcome::Ok(said) => said,
            other => panic!("expected it to work, got {other:?}"),
        }
    }

    fn failed(outcome: ToolOutcome) -> String {
        match outcome {
            ToolOutcome::Failed(said) => said,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn reading_a_skill_hands_over_the_whole_markdown_file() {
        let directory = tempfile::tempdir().expect("temp dir");
        let said = ok(immediate(
            &in_space(directory.path()),
            &call("read_skill", r#"{"name":"xlsx"}"#),
        ));
        assert!(said.starts_with("---\nname: xlsx"), "{said}");
        assert!(said.contains("create_spreadsheet"), "{said}");
    }

    #[test]
    fn an_unknown_skill_names_the_ones_that_exist() {
        // A model that asked for the wrong thing should be able to fix it from
        // the answer rather than guess again.
        let directory = tempfile::tempdir().expect("temp dir");
        let said = failed(immediate(
            &in_space(directory.path()),
            &call("read_skill", r#"{"name":"keynote"}"#),
        ));
        for known in ["docx", "xlsx", "pptx", "pdf"] {
            assert!(said.contains(known), "{said}");
        }
    }

    #[test]
    fn a_document_lands_in_the_workspace_as_a_real_docx() {
        let directory = tempfile::tempdir().expect("temp dir");
        let said = ok(immediate(
            &in_space(directory.path()),
            &call(
                "create_document",
                r###"{"path":"out/report.docx","title":"Q3","markdown":"## Summary\n\nIt went well."}"###,
            ),
        ));
        assert!(said.contains("out/report.docx"), "{said}");
        assert!(said.contains("heading"), "it says what went in: {said}");

        // Written where it was asked for, creating the directory above it.
        let written = std::fs::read(directory.path().join("out/report.docx")).expect("the file");
        assert_eq!(&written[..2], b"PK");
        crate::model::office::tests::assert_well_formed_package(&written);
    }

    #[test]
    fn a_pdf_reports_how_many_pages_it_came_out_at() {
        // The one fact about the result nobody can see without opening it.
        let directory = tempfile::tempdir().expect("temp dir");
        let said = ok(immediate(
            &in_space(directory.path()),
            &call(
                "create_pdf",
                r#"{"path":"summary.pdf","markdown":"One line.\n\n\\pagebreak\n\nAnother."}"#,
            ),
        ));
        assert!(said.contains("2 page(s)"), "{said}");
        assert!(crate::model::documents::is_pdf(
            &std::fs::read(directory.path().join("summary.pdf")).expect("the file")
        ));
    }

    #[test]
    fn the_wrong_extension_is_refused_with_the_right_one() {
        // Silently renaming would hide the mistake; writing a .docx to
        // report.txt would produce a file nobody can open and no way to tell
        // the tool was at fault.
        let directory = tempfile::tempdir().expect("temp dir");
        let said = failed(immediate(
            &in_space(directory.path()),
            &call(
                "create_document",
                r#"{"path":"report.txt","markdown":"Body."}"#,
            ),
        ));
        assert!(said.contains("report.docx"), "{said}");
        assert!(!directory.path().join("report.txt").exists());
    }

    #[test]
    fn a_document_written_outside_the_workspace_is_refused() {
        // The same predicate as every other write. A new tool must not be a
        // new way around it.
        let directory = tempfile::tempdir().expect("temp dir");
        for (tool, arguments) in [
            (
                "create_document",
                r#"{"path":"../escape.docx","markdown":"x"}"#,
            ),
            ("create_pdf", r#"{"path":"/etc/escape.pdf","markdown":"x"}"#),
        ] {
            let said = failed(immediate(
                &in_space(directory.path()),
                &call(tool, arguments),
            ));
            assert!(said.contains("outside the workspace"), "{tool}: {said}");
        }
    }

    #[test]
    fn an_empty_document_is_refused_rather_than_written_blank() {
        let directory = tempfile::tempdir().expect("temp dir");
        let said = failed(immediate(
            &in_space(directory.path()),
            &call("create_document", r#"{"path":"a.docx","markdown":"   "}"#),
        ));
        assert!(said.contains("no content"), "{said}");
        assert!(!directory.path().join("a.docx").exists());
    }

    #[test]
    fn a_workbook_keeps_its_numbers_as_numbers() {
        let directory = tempfile::tempdir().expect("temp dir");
        let said = ok(immediate(
            &in_space(directory.path()),
            &call(
                "create_spreadsheet",
                r#"{"path":"d.xlsx","sheets":[{"name":"Q3","rows":[["Region","Value"],["North","48250"]]}]}"#,
            ),
        ));
        assert!(said.contains("1 sheet(s), 2 row(s)"), "{said}");

        let bytes = std::fs::read(directory.path().join("d.xlsx")).expect("the file");
        let sheet = crate::model::office::tests::part(&bytes, "xl/worksheets/sheet1.xml");
        assert!(sheet.contains("<v>48250</v>"), "{sheet}");
    }

    #[test]
    fn a_model_that_sends_json_numbers_gets_a_workbook_anyway() {
        // The declaration asks for strings and a model sends numbers about half
        // the time. Refusing would fail on the most common correct-in-spirit
        // input.
        use office::xlsx::Cell;
        assert_eq!(cell(&serde_json::json!(42)), Cell::Number(42.0));
        assert_eq!(cell(&serde_json::json!(1.5)), Cell::Number(1.5));
        assert_eq!(cell(&serde_json::json!(true)), Cell::Bool(true));
        assert_eq!(cell(&serde_json::json!(null)), Cell::Empty);
        assert_eq!(cell(&serde_json::json!("007")), Cell::Text("007".into()));
    }

    #[test]
    fn a_malformed_sheet_says_what_shape_it_wanted() {
        let directory = tempfile::tempdir().expect("temp dir");
        let runner = in_space(directory.path());
        for (arguments, expected) in [
            (r#"{"path":"d.xlsx","sheets":"Q3"}"#, "list of {name, rows}"),
            (r#"{"path":"d.xlsx","sheets":[{"name":"Q3"}]}"#, "no `rows`"),
            (
                r#"{"path":"d.xlsx","sheets":[{"name":"Q3","rows":["a,b"]}]}"#,
                "list of cell values",
            ),
        ] {
            let said = failed(immediate(&runner, &call("create_spreadsheet", arguments)));
            assert!(said.contains(expected), "{arguments}\ngot: {said}");
        }
    }

    #[test]
    fn a_deck_takes_its_nesting_from_the_leading_spaces() {
        let directory = tempfile::tempdir().expect("temp dir");
        let said = ok(immediate(
            &in_space(directory.path()),
            &call(
                "create_presentation",
                r#"{"path":"d.pptx","slides":[{"title":"One","bullets":["top","  nested"]},{"title":"Two"}]}"#,
            ),
        ));
        assert!(said.contains("2 slide(s)"), "{said}");

        let bytes = std::fs::read(directory.path().join("d.pptx")).expect("the file");
        let slide = crate::model::office::tests::part(&bytes, "ppt/slides/slide1.xml");
        assert!(slide.contains(r#"<a:pPr lvl="1"/>"#), "{slide}");
        crate::model::office::tests::assert_well_formed_package(&bytes);
    }

    #[test]
    fn a_bullet_written_as_markdown_does_not_keep_its_dash() {
        // A model writes "- point" inside a bullets list out of habit, and the
        // dash would otherwise be rendered next to the bullet glyph.
        assert_eq!(bullet("- top").text, "top");
        assert_eq!(bullet("  * nested").text, "nested");
        assert_eq!(bullet("  nested").level, 1);
        assert_eq!(bullet("      deep").level, 3);
        assert_eq!(bullet("plain").level, 0);
    }

    #[test]
    fn a_deck_with_no_slides_is_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let said = failed(immediate(
            &in_space(directory.path()),
            &call("create_presentation", r#"{"path":"d.pptx","slides":[]}"#),
        ));
        assert!(said.contains("no slides"), "{said}");
    }

    #[test]
    fn merging_refuses_to_write_over_one_of_its_own_inputs() {
        // pdfunite takes its output last and would happily consume a file it
        // is also reading.
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::write(directory.path().join("a.pdf"), b"%PDF-1.7\n").expect("a file");
        std::fs::write(directory.path().join("b.pdf"), b"%PDF-1.7\n").expect("a file");

        let said = failed(immediate(
            &in_space(directory.path()),
            &call("merge_pdfs", r#"{"from":["a.pdf","b.pdf"],"to":"a.pdf"}"#),
        ));
        assert!(
            said.contains("cannot be one of the files being merged"),
            "{said}"
        );
    }

    #[test]
    fn merging_needs_at_least_two_and_says_when_one_is_missing() {
        let directory = tempfile::tempdir().expect("temp dir");
        let runner = in_space(directory.path());
        std::fs::write(directory.path().join("a.pdf"), b"%PDF-1.7\n").expect("a file");

        let one = failed(immediate(
            &runner,
            &call("merge_pdfs", r#"{"from":["a.pdf"],"to":"out.pdf"}"#),
        ));
        assert!(one.contains("at least two"), "{one}");

        let gone = failed(immediate(
            &runner,
            &call(
                "merge_pdfs",
                r#"{"from":["a.pdf","gone.pdf"],"to":"out.pdf"}"#,
            ),
        ));
        assert!(gone.contains("gone.pdf"), "{gone}");
    }

    #[test]
    fn extracting_refuses_to_write_over_the_original() {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::write(directory.path().join("r.pdf"), b"%PDF-1.7\n").expect("a file");
        let said = failed(immediate(
            &in_space(directory.path()),
            &call(
                "extract_pages",
                r#"{"path":"r.pdf","pages":"1-2","to":"r.pdf"}"#,
            ),
        ));
        assert!(said.contains("rather than over the original"), "{said}");
    }

    #[test]
    fn reading_something_that_is_not_a_pdf_says_which_tool_to_use() {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::write(directory.path().join("notes.txt"), "just text").expect("a file");
        let said = failed(immediate(
            &in_space(directory.path()),
            &call("read_pdf", r#"{"path":"notes.txt"}"#),
        ));
        assert!(said.contains("read_file"), "{said}");
    }

    #[test]
    fn the_document_tools_say_so_when_there_is_no_workspace() {
        let directory = tempfile::tempdir().expect("temp dir");
        let runner = runner(directory.path());
        for (tool, arguments) in [
            ("create_document", r#"{"path":"a.docx","markdown":"x"}"#),
            ("create_pdf", r#"{"path":"a.pdf","markdown":"x"}"#),
            ("create_spreadsheet", r#"{"path":"a.xlsx","sheets":[]}"#),
            ("create_presentation", r#"{"path":"a.pptx","slides":[]}"#),
            ("read_pdf", r#"{"path":"a.pdf"}"#),
            ("merge_pdfs", r#"{"from":["a.pdf","b.pdf"],"to":"c.pdf"}"#),
            (
                "extract_pages",
                r#"{"path":"a.pdf","pages":"1","to":"b.pdf"}"#,
            ),
        ] {
            let said = failed(immediate(&runner, &call(tool, arguments)));
            assert!(said.contains("no workspace"), "{tool}: {said}");
        }
    }

    #[test]
    fn the_environment_beats_the_preference_for_one_run() {
        std::env::set_var("EXA_API_KEY", "from-env");
        assert_eq!(exa_key(Some("from-config")).as_deref(), Some("from-env"));
        std::env::remove_var("EXA_API_KEY");
        assert_eq!(exa_key(Some("from-config")).as_deref(), Some("from-config"));
        assert_eq!(exa_key(Some("   ")), None);
        assert_eq!(exa_key(None), None);
    }

    #[test]
    fn an_alert_signal_says_what_the_hazard_is_and_not_only_its_name() {
        // "Severe Thunderstorm Warning" on its own scored 0 of 6 in the lookout
        // suite: the model would not connect it to an afternoon on a roof. The
        // sentence that makes the connection is one the weather service has
        // already written, and it is free to carry.
        let alert = crate::model::weather::Alert {
            event: "Severe Thunderstorm Warning".into(),
            severity: "Severe".into(),
            headline: Some("Damaging winds up to 60 mph expected. Move indoors.".into()),
            description: Some("A much longer paragraph.".into()),
            ends: Some("21:00".into()),
        };
        let line = one_line(&alert);
        assert!(
            line.starts_with("Severe Thunderstorm Warning until 21:00"),
            "{line}"
        );
        assert!(line.contains("Move indoors"), "{line}");

        // An alert with nothing but a name is still a usable line.
        let bare = crate::model::weather::Alert {
            headline: None,
            description: None,
            ends: None,
            ..alert
        };
        assert_eq!(one_line(&bare), "Severe Thunderstorm Warning");
    }
}
