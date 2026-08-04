//! A workflow: a goal, a few steps, and where the work has got to.
//!
//! One word in three places. The window says **workflow**, the code says
//! [`Workflow`], and so does the prompt — which is worth stating because it is
//! the exception here. `model::project` maintains a three-way split (the window
//! says project, the code says `Project`, the prompt says *workspace folder*)
//! and pays for it with a test that greps the composed prompt, because Planner
//! and the memory tool had both claimed that word first. Nothing had claimed
//! this one.
//!
//! Almost nothing. `gh workflow list` is a real subcommand and the GitHub
//! guidance says "workflow runs" in prose, so a project with both switched on
//! offers the model two plausible landings for "run the deploy workflow". That
//! overlap is measured rather than argued about — see [`Overlap`] and the
//! `workflow/` eval family.
//!
//! # There is no second type for a saved one
//!
//! A workflow is a workflow whether it lives on the open thread or in a file.
//! Saving clears the outcomes and writes the same shape to
//! `projects/<slug>/workflows/<name>.md`; starting reads it back. An earlier
//! draft had a *run* and a *routine* and the pair bought nothing but two words
//! the user would have to learn, when "save" already carries the whole idea.
//!
//! # What the model may change, and what it may not
//!
//! The step text is the model's. The **note is the user's**, and no action here
//! rewrites one — [`Workflow::revise`] carries notes across by position so a
//! model reordering the remaining steps cannot silently drop the sentence the
//! user wrote to steer it. Steps already finished are not revisable at all: a
//! plan that can rewrite its own history is not a record of anything.
//!
//! Nothing in this module touches the disk or the model. It renders the block
//! the model reads and applies the changes an action asks for; `Store` writes
//! the files and `ui::runner` runs the tool.

use serde::{Deserialize, Serialize};

/// How many steps a workflow may have.
///
/// A ceiling rather than a target. Past a dozen the list stops being something
/// a person reads before saying go, which is the one thing it has to be — and a
/// small model asked for "steps" will otherwise produce a project plan.
pub const MAX_STEPS: usize = 12;

/// Fewer than this is not a workflow, it is an answer.
pub const MIN_STEPS: usize = 2;

/// How long a step's text may be. A step is a line, not a paragraph; a model
/// writing an essay into one has misunderstood the shape and is better told so
/// than accommodated.
pub const MAX_STEP: usize = 200;

/// Where a step has got to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum State {
    #[default]
    Pending,
    /// Finished, with whatever the model said it produced.
    Done { outcome: String },
    /// Deliberately not done, with the reason. Not a failure — "they already
    /// have the figures" is a perfectly good outcome for a step that was going
    /// to go and get them.
    Skipped { why: String },
    /// Tried and could not. The run stops here rather than carrying on into
    /// steps that depended on it.
    Stuck { why: String },
}

impl State {
    /// Whether this step is finished, however it finished.
    pub fn is_settled(&self) -> bool {
        !matches!(self, Self::Pending)
    }

    /// The word the checklist uses.
    fn word(&self) -> &'static str {
        match self {
            Self::Pending => "to do",
            Self::Done { .. } => "done",
            Self::Skipped { .. } => "skipped",
            Self::Stuck { .. } => "stuck",
        }
    }

    /// What happened, if anything did.
    fn detail(&self) -> Option<&str> {
        match self {
            Self::Pending => None,
            Self::Done { outcome } => Some(outcome),
            Self::Skipped { why } | Self::Stuck { why } => Some(why),
        }
    }
}

/// One step: what to do, what the user said about it, and how it went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub what: String,
    /// The user's steering. Written in the window, never by the model, and
    /// delivered to the model in the tool result that makes this step current
    /// rather than carried in the prompt for every other step's sake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Flattened, so a step reads as `{"what": …, "status": "done", "outcome":
    /// …}` rather than nesting a one-key object inside another.
    #[serde(flatten)]
    pub state: State,
}

impl Step {
    pub fn new(what: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            note: None,
            state: State::Pending,
        }
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        let note = note.into();
        self.note = (!note.trim().is_empty()).then(|| note.trim().to_string());
        self
    }
}

/// A goal and the steps to reach it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workflow {
    pub goal: String,
    pub steps: Vec<Step>,
    /// Whether the user has said go.
    ///
    /// Set by the Start button, and by the first [`Workflow::advance`] — a user
    /// who types "go" has greenlit it as surely as one who clicked, and a rail
    /// that insisted on the click would make the eval score the rail instead of
    /// the judgement.
    #[serde(default)]
    pub started: bool,
    /// The file this came from or was saved to, if there is one. A workflow
    /// that only ever existed in one chat has none, and that is the common case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_as: Option<String>,
    /// What the user changed since the model last read the plan.
    ///
    /// Set when they edit, and cleared by [`apply`] the moment the model is
    /// handed a render — which is the moment it has been told. A
    /// `thread::Note` cannot do this job: notes are addressed to the reader and
    /// [`crate::model::thread::Thread::messages_for_model`] deliberately never
    /// sends them, so a model carrying on against a plan it read three rounds
    /// ago would never learn the steering had moved. That is the exact failure
    /// this capability exists to prevent, so it cannot be the one it ships with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited: Option<String>,
}

/// What changed between two versions of a workflow, as a sentence.
///
/// Deliberately coarse. The model does not need a diff, it needs to know that
/// the plan under it moved and roughly where — enough to re-read rather than
/// carry on from memory.
pub fn changes(before: &Workflow, after: &Workflow) -> String {
    let mut said = Vec::new();
    if before.goal != after.goal {
        said.push(format!("the goal is now \"{}\"", after.goal));
    }

    let settled = before.current().unwrap_or(before.steps.len());
    let was: Vec<&str> = before.steps[settled.min(before.steps.len())..]
        .iter()
        .map(|step| step.what.as_str())
        .collect();
    let now: Vec<&str> = after
        .steps
        .get(settled.min(after.steps.len())..)
        .unwrap_or_default()
        .iter()
        .map(|step| step.what.as_str())
        .collect();
    if was != now {
        said.push(format!(
            "the remaining steps are now: {}",
            now.iter()
                .enumerate()
                .map(|(offset, what)| format!("{}. {what}", settled + offset + 1))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }

    let notes: Vec<String> = after
        .steps
        .iter()
        .enumerate()
        .filter(|(index, step)| before.steps.get(*index).map(|was| &was.note) != Some(&step.note))
        .filter_map(|(index, step)| {
            let note = step.note.as_deref()?;
            Some(format!("on step {}: {note}", index + 1))
        })
        .collect();
    if !notes.is_empty() {
        said.push(format!("they wrote {}", notes.join(", and ")));
    }

    said.join("; ")
}

impl Workflow {
    /// A proposed workflow, or why these steps are not one.
    pub fn proposed(goal: &str, steps: Vec<String>) -> Result<Self, String> {
        let goal = goal.trim();
        if goal.is_empty() {
            return Err("a workflow needs a goal — one line saying what it is for.".into());
        }
        let steps = check(steps)?;
        Ok(Self {
            goal: goal.to_string(),
            steps,
            started: false,
            saved_as: None,
            edited: None,
        })
    }

    /// The step being worked on: the first that has not settled.
    pub fn current(&self) -> Option<usize> {
        self.steps.iter().position(|step| !step.state.is_settled())
    }

    /// Every step has settled.
    pub fn is_finished(&self) -> bool {
        self.current().is_none()
    }

    /// A step got stuck, so the run is over whether or not steps remain.
    pub fn is_stuck(&self) -> bool {
        self.steps
            .iter()
            .any(|step| matches!(step.state, State::Stuck { .. }))
    }

    /// How many have settled, out of how many there are.
    pub fn progress(&self) -> (usize, usize) {
        (
            self.steps
                .iter()
                .filter(|step| step.state.is_settled())
                .count(),
            self.steps.len(),
        )
    }

    /// Settle the current step and move on, reporting what to do next.
    pub fn advance(&mut self, state: State) -> Result<String, String> {
        if state == State::Pending {
            return Err(
                "`advance` records what happened to the step you were on — pass an \
                        outcome, or `skipped` or `stuck` with a reason."
                    .into(),
            );
        }
        let Some(at) = self.current() else {
            return Err(format!(
                "every step of \"{}\" has already been settled. There is nothing left to \
                 advance — say what was produced, or plan a new workflow.",
                self.goal
            ));
        };
        self.started = true;
        self.steps[at].state = state;
        Ok(self.render())
    }

    /// Replace the steps that have not been done yet.
    ///
    /// Everything already settled stays exactly as it is. A model revising a
    /// plan is revising what is left of it — rewriting a step it has already
    /// reported on would make the record disagree with the conversation.
    ///
    /// Notes carry across **by the step's text**, not by its position. The first
    /// draft matched by position and a test caught what that does: the user
    /// annotates step 2, the model reorders, and the sentence ends up steering
    /// a step it was never about — which is worse than losing it, because
    /// nothing about the result looks wrong.
    ///
    /// A note whose step is gone is **dropped and said out loud**. Silently
    /// discarding the one thing in here the user wrote themselves is the failure
    /// this whole capability exists to avoid.
    pub fn revise(&mut self, steps: Vec<String>) -> Result<String, String> {
        let settled = self.current().unwrap_or(self.steps.len());
        let mut replacement = check(steps)?;

        if settled + replacement.len() > MAX_STEPS {
            return Err(format!(
                "that would make {} steps in all, and a workflow may have at most {MAX_STEPS}. \
                 {settled} are already settled.",
                settled + replacement.len()
            ));
        }

        let mut steering: Vec<(String, String)> = self.steps[settled..]
            .iter()
            .filter_map(|step| Some((key(&step.what), step.note.clone()?)))
            .collect();
        for step in &mut replacement {
            let wanted = key(&step.what);
            if let Some(at) = steering.iter().position(|(what, _)| *what == wanted) {
                step.note = Some(steering.remove(at).1);
            }
        }

        self.steps.truncate(settled);
        self.steps.extend(replacement);

        let mut said = self.render();
        if !steering.is_empty() {
            said.push_str(&format!(
                "\n\nThat revision dropped {} note(s) the user had written on steps you \
                 removed. Say so — they wrote them for a reason, and they cannot tell from \
                 your answer that the steering is gone.",
                steering.len()
            ));
        }
        Ok(said)
    }

    /// The same workflow with nothing having happened yet: what gets saved, and
    /// what a saved one becomes when it is started again.
    pub fn fresh(&self) -> Self {
        Self {
            goal: self.goal.clone(),
            steps: self
                .steps
                .iter()
                .map(|step| Step {
                    what: step.what.clone(),
                    note: step.note.clone(),
                    state: State::Pending,
                })
                .collect(),
            started: false,
            saved_as: self.saved_as.clone(),
            // A shape has nothing outstanding to tell anybody.
            edited: None,
        }
    }

    /// The block the model reads.
    ///
    /// Returned by every action rather than carried in the system prompt. The
    /// prompt's stable prefix is what llama-server keeps in its KV cache and
    /// `model::instructions` is built entirely around it staying byte-identical
    /// between turns; a checklist that changes every round would rewrite the
    /// tail of that prefix each time. In a tool result it is appended, which
    /// costs the cache nothing — and it lands where the model is acting instead
    /// of where it started.
    ///
    /// The user's note for the **current** step is here and no other step's is.
    /// A note is read at the moment it applies, which is the same argument
    /// `planner::note_for` makes for everything that depends on what a call
    /// returned.
    pub fn render(&self) -> String {
        let (settled, total) = self.progress();
        let mut out = String::new();
        // First, before the list, because it changes how the list should be
        // read. A model that skims past this and acts on what it remembers is
        // acting on a plan the user has already overruled.
        if let Some(edited) = self.edited.as_deref().filter(|e| !e.trim().is_empty()) {
            out.push_str(&format!(
                "The user has changed this plan since you last read it — {edited}. Work from \
                 what is below, not from what you remember.\n\n"
            ));
        }
        out.push_str(&format!("Workflow: {}\n", self.goal));
        out.push_str(&match (self.current(), self.is_stuck()) {
            (_, true) => format!("{settled} of {total} steps settled — a step is stuck.\n\n"),
            (None, _) => format!("All {total} steps are settled.\n\n"),
            (Some(at), _) => format!(
                "{settled} of {total} steps settled. You are on step {}.\n\n",
                at + 1
            ),
        });

        for (index, step) in self.steps.iter().enumerate() {
            let marker = if self.current() == Some(index) {
                "NOW".to_string()
            } else {
                step.state.word().to_string()
            };
            out.push_str(&format!("{}. [{marker}] {}\n", index + 1, step.what));
            if let Some(detail) = step.state.detail().filter(|d| !d.trim().is_empty()) {
                out.push_str(&format!("   → {}\n", one_line(detail)));
            }
        }

        if let Some((at, note)) = self.current().and_then(|at| {
            let note = self.steps[at].note.as_deref()?;
            (!note.trim().is_empty()).then_some((at, note))
        }) {
            out.push_str(&format!(
                "\nThe user's note for step {}: {note}\nIt is their instruction for this step \
                 and it overrides how you would otherwise have done it.\n",
                at + 1
            ));
        }

        out.push_str(&self.next_move());
        out
    }

    /// The sentence after the list: what to do now.
    ///
    /// Attached to the result rather than left to the system prompt for the
    /// reason the rest of this application attaches such things — a rule read at
    /// the moment it applies costs nothing on every other turn. It is also the
    /// one place that can say "you have not been given the go-ahead", because it
    /// is the only text that knows.
    fn next_move(&self) -> String {
        if self.is_stuck() {
            return "\nA step is stuck. Stop here: tell the user which step and why, and what \
                    you would need to get past it. Do not skip ahead to the steps after it."
                .to_string();
        }
        if self.is_finished() {
            return "\nEvery step is settled. Tell the user what was produced, in the answer \
                    rather than as another call. If they may want this shape again, you can \
                    offer to save it — do not save it uninvited."
                .to_string();
        }
        if !self.started {
            // The distinction between approving of a plan and starting it lives
            // here rather than in the guidance, and that placement was measured.
            // As a paragraph in the system prompt it took
            // `approval-shaped-is-not-approval` from 61% to 94% and dropped
            // `drafts-then-waits-for-the-word` from 93% to 60% — the model went
            // quiet about the *tool* as well as about starting, and stopped
            // calling `plan` at all. Net six points worse.
            //
            // In here it is read at the one moment it applies: a plan exists and
            // nobody has said go. Every other turn pays nothing for it, and it
            // cannot compete with the sentence that says to reach for `plan` in
            // the first place, because that sentence has already done its work
            // by the time this is on screen.
            return "\nProposed, not started. Show these steps to the user and let them say go \
                    — unless they had already asked you to do the work, in which case get on \
                    with step 1 now. They can change any step before it runs.\n\
                    Approving of the plan is not starting it: \"that looks reasonable\" is a \
                    remark about the plan, \"go ahead\" is an instruction. If you cannot tell \
                    which you were given, ask."
                .to_string();
        }
        "\nDo the step marked NOW, then call `workflow` with `advance` and what it produced. \
         One step at a time: do not report a step you have not done."
            .to_string()
    }

    /// What a saved workflow looks like on disk.
    ///
    /// Markdown, and deliberately the plainest Markdown that round-trips: a
    /// heading for the goal, a numbered list for the steps, a block quote under
    /// a step for the user's note. Threads and projects are JSON because nobody
    /// edits those by hand; this is a thing the user writes, so it is a file
    /// they can open in any editor — the same argument `model::memory` makes for
    /// leaving the vault as notes.
    ///
    /// Outcomes are not written. A saved workflow is a shape, not a record of
    /// the one time it ran.
    pub fn to_markdown(&self) -> String {
        let mut out = format!("# {}\n\n", self.goal);
        for step in &self.steps {
            out.push_str(&format!("1. {}\n", one_line(&step.what)));
            if let Some(note) = step.note.as_deref().filter(|n| !n.trim().is_empty()) {
                out.push_str(&format!("   > {}\n", one_line(note)));
            }
        }
        out
    }

    /// Read one back. `None` when the file holds no steps — an empty list is
    /// not a workflow with nothing in it, it is a file that is not one.
    pub fn from_markdown(text: &str) -> Option<Self> {
        let mut goal = String::new();
        let mut steps: Vec<Step> = Vec::new();

        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(heading) = trimmed.strip_prefix("# ") {
                if goal.is_empty() {
                    goal = heading.trim().to_string();
                }
                continue;
            }
            if let Some(note) = trimmed.strip_prefix('>') {
                // A quote before any step is prose about the file, not a note.
                if let Some(step) = steps.last_mut() {
                    let note = note.trim();
                    if !note.is_empty() {
                        step.note = Some(match step.note.take() {
                            Some(existing) => format!("{existing} {note}"),
                            None => note.to_string(),
                        });
                    }
                }
                continue;
            }
            if let Some(what) = numbered(trimmed) {
                steps.push(Step::new(what));
            }
        }

        if goal.is_empty() || steps.is_empty() {
            return None;
        }
        steps.truncate(MAX_STEPS);
        Some(Self {
            goal,
            steps,
            started: false,
            saved_as: None,
            edited: None,
        })
    }

    /// The line the collapsed strip shows: where this is up to.
    pub fn summary(&self) -> String {
        let (settled, total) = self.progress();
        match (self.current(), self.is_stuck()) {
            (_, true) => format!("Stuck at step {} of {total}", settled + 1),
            (None, _) => format!("Finished · {total} steps"),
            (Some(_), _) if !self.started => format!("{total} steps proposed · {}", self.goal),
            (Some(at), _) => format!("Step {} of {total} · {}", at + 1, self.steps[at].what),
        }
    }
}

/// The text after a `1.` / `1)` marker, if the line has one.
fn numbered(line: &str) -> Option<&str> {
    let digits: String = line.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = &line[digits.len()..];
    let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?;
    let what = rest.trim();
    (!what.is_empty()).then_some(what)
}

/// Steps as given, or why they are not usable ones.
fn check(steps: Vec<String>) -> Result<Vec<Step>, String> {
    let steps: Vec<String> = steps
        .into_iter()
        .map(|step| one_line(&step))
        .filter(|step| !step.is_empty())
        .collect();

    // Nothing at all and a single step are different mistakes, and the message
    // for one is actively wrong for the other. Measured: the model sent
    // `steps: ["Confirm which two quarters…"]` — the first step only — four
    // times running, and was told each time that fewer than two steps is not a
    // workflow and it should just do the job. That is true of a genuinely small
    // job and useless to a model that has a plan and sent one line of it.
    if steps.len() == 1 {
        return Err(format!(
            "only one step arrived. `steps` is the whole list in one call, not one step at a \
             time — send all of them together, {MIN_STEPS} to {MAX_STEPS}. If the job really is \
             a single step, it is not a workflow: just do it."
        ));
    }
    if steps.is_empty() {
        return Err(format!(
            "no steps arrived. `plan` needs a `goal` and a `steps` list of {MIN_STEPS} to \
             {MAX_STEPS} lines, each saying what to do."
        ));
    }
    if steps.len() > MAX_STEPS {
        return Err(format!(
            "that is {} steps and a workflow may have at most {MAX_STEPS}. Group the small ones \
             — the list has to be something the user reads before saying go.",
            steps.len()
        ));
    }
    if let Some(long) = steps.iter().find(|step| step.chars().count() > MAX_STEP) {
        return Err(format!(
            "step \"{}…\" is too long. A step is one line saying what to do, not a paragraph \
             saying how.",
            long.chars().take(40).collect::<String>()
        ));
    }
    Ok(steps.into_iter().map(Step::new).collect())
}

/// Whitespace flattened, so a step cannot break the list it is rendered into.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Two step texts are the same step if they differ only in case or spacing. A
/// model that rewrote "Draft the comparison" as "draft the comparison" has not
/// changed the step, and the user's note for it should survive that.
fn key(what: &str) -> String {
    one_line(what).to_lowercase()
}

/// What the model asked the tool to do.
///
/// Parsed here rather than at each call site because there are two: the
/// application, which owns a thread and a `Store`, and the eval harness, which
/// owns neither. Everything that does not touch storage is applied by
/// [`apply`], so the two cannot answer the same call differently — which they
/// would, eventually, and the difference would show up as a score that did not
/// reproduce rather than as a bug anyone could see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Plan {
        goal: String,
        steps: Vec<String>,
    },
    Advance(State),
    Show,
    /// Keep this workflow's shape. The caller writes it, because only the
    /// caller knows where.
    Save,
    /// Run a saved one. The caller finds it, for the same reason.
    Start(String),
    /// Not an action. Carries what to tell the model.
    Bad(String),
}

impl Action {
    pub fn parse(arguments: &str) -> Self {
        let parsed: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
        let text = |key: &str| {
            parsed
                .get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string()
        };

        match text("action").to_lowercase().as_str() {
            "plan" => Self::Plan {
                goal: text("goal"),
                steps: parsed
                    .get("steps")
                    .cloned()
                    .and_then(|steps| serde_json::from_value::<Vec<String>>(steps).ok())
                    .unwrap_or_default(),
            },
            "advance" => {
                let outcome = text("outcome");
                Self::Advance(match text("status").to_lowercase().as_str() {
                    "skipped" => State::Skipped { why: outcome },
                    "stuck" => State::Stuck { why: outcome },
                    // Anything else, including nothing, is "done". A model that
                    // sent an outcome and no status has said what it means.
                    _ => State::Done { outcome },
                })
            }
            "show" => Self::Show,
            "save" => Self::Save,
            "start" => Self::Start(text("name")),
            "" => Self::Bad(
                "`workflow` needs an `action`: `plan`, `advance`, `show`, `save` or `start`."
                    .into(),
            ),
            other => Self::Bad(format!(
                "`{other}` is not a workflow action. Use `plan`, `advance`, `show`, `save` or \
                 `start`."
            )),
        }
    }
}

/// The reply for an action that needs no storage.
///
/// `Save` and `Start` are absent by construction rather than by omission: they
/// are the two that touch files, and returning something plausible for them here
/// would be the easiest possible way for the harness and the application to
/// start disagreeing.
pub fn apply(state: &mut Option<Workflow>, action: &Action) -> Result<String, String> {
    let said = dispatch(state, action);
    // The model has now been handed a render, so it has been told. Cleared only
    // on success: a call that failed did not show it the plan.
    if said.is_ok() {
        if let Some(flow) = state.as_mut() {
            flow.edited = None;
        }
    }
    said
}

fn dispatch(state: &mut Option<Workflow>, action: &Action) -> Result<String, String> {
    let nothing_yet = || Err(nothing_planned());

    match action {
        Action::Plan { goal, steps } => {
            // Planning again while one is running revises what is left of it.
            // Replacing the whole thing would throw away steps already done and
            // reported, and leave the record disagreeing with the conversation.
            if let Some(existing) = state.as_mut().filter(|flow| flow.started) {
                return existing.revise(steps.clone());
            }
            let planned = Workflow::proposed(goal, steps.clone())?;
            let said = planned.render();
            *state = Some(planned);
            Ok(said)
        }
        Action::Advance(settled) => match state.as_mut() {
            Some(flow) => flow.advance(settled.clone()),
            None => nothing_yet(),
        },
        Action::Show => match state.as_ref() {
            Some(flow) => Ok(flow.render()),
            None => nothing_yet(),
        },
        Action::Save | Action::Start(_) => {
            Err("that action is the caller's to carry out.".to_string())
        }
        Action::Bad(why) => Err(why.clone()),
    }
}

/// What to say when there is no workflow to act on.
///
/// Public because three callers need the same sentence — [`apply`], the
/// application's `save`, and the harness's — and a message written out three
/// times is one that will be improved in one of them.
pub fn nothing_planned() -> String {
    "no workflow has been planned in this conversation. Use `plan` with a goal and steps \
     first, or `start` to run one that was saved."
        .to_string()
}

/// What to say when a workflow has just been filed under a name.
pub fn saved(goal: &str) -> String {
    format!(
        "Saved as \"{goal}\". Tell the user it is saved and what it is called — they can start \
         it again by name."
    )
}

/// What to say when there is no saved workflow by that name.
pub fn no_such(name: &str) -> String {
    format!(
        "no saved workflow is called \"{name}\". Say so rather than guessing at what was in it; \
         `plan` a new one if that is what they want."
    )
}

/// How the overlap with `gh workflow` is handled, so it can be measured rather
/// than argued about.
///
/// `gh workflow list` is a real subcommand, and the GitHub capability's own
/// prose says "workflow runs" — so a project with both switched on hands the
/// model two plausible landings for "run the deploy workflow". Three candidate
/// answers, one flag, and the `workflow/` eval family decides between them:
///
/// * [`Overlap::Current`] — change nothing, on the theory that the collision is
///   in the vocabulary and not in the traces.
/// * [`Overlap::Reword`] — the GitHub prose says "Actions runs". Costs nothing
///   per turn and risks the opposite failure, because GitHub's own interface
///   says "workflow" and so do the people asking about it.
/// * [`Overlap::Disambiguate`] — the GitHub prose is left alone and this
///   capability's guidance says which is which. Narrower blast radius, paid for
///   in prompt length, and only in contexts that have this switched on.
///
/// The rule was written down before the first run, which is the only thing that
/// makes this measurement rather than justification: if `Current` routes as well
/// as the github family usually scores, nothing changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Overlap {
    #[default]
    Current,
    Reword,
    Disambiguate,
}

/// The arm in force. Set once before anything composes a prompt, read wherever
/// GitHub's prose is built.
///
/// A global because the alternative is threading an experiment knob through
/// `tools::guidance`, `tools::for_tools`, `capability::catalogue` and the `gh`
/// declaration — six signatures changed so that one measurement can be taken,
/// and left changed afterwards. It is written once, by `main` or by the eval
/// driver, before any of those run.
static ARM: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

impl Overlap {
    /// The arm this process is running under.
    pub fn current() -> Self {
        match ARM.load(std::sync::atomic::Ordering::Relaxed) {
            1 => Self::Reword,
            2 => Self::Disambiguate,
            _ => Self::Current,
        }
    }

    /// Choose the arm. Call it before the first prompt is composed.
    pub fn install(self) {
        let code = match self {
            Self::Current => 0,
            Self::Reword => 1,
            Self::Disambiguate => 2,
        };
        ARM.store(code, std::sync::atomic::Ordering::Relaxed);
    }

    /// GitHub's own prose with this arm applied.
    ///
    /// A substitution rather than two copies of each sentence: the strings live
    /// in `capability::ALL` (a `const`) and in the `gh` declaration, and
    /// duplicating both to vary one phrase would mean every future edit to
    /// either had to be made twice — which is how an A/B ends up comparing two
    /// things that differ in more than the one thing.
    pub fn applied(self, text: &str) -> String {
        match self {
            Self::Reword => text.replace("workflow runs", "Actions runs"),
            _ => text.to_string(),
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().as_str() {
            "current" => Some(Self::Current),
            "reword" => Some(Self::Reword),
            "disambiguate" => Some(Self::Disambiguate),
            _ => None,
        }
    }

    /// What GitHub's own prose calls its runs.
    pub fn gh_runs(self) -> &'static str {
        match self {
            Self::Current | Self::Disambiguate => "workflow runs",
            Self::Reword => "Actions runs",
        }
    }

    /// The clause this capability's guidance adds, if any.
    pub fn note(self) -> Option<&'static str> {
        match self {
            Self::Disambiguate => Some(
                "\n\nGitHub Actions has its own workflows and this is not them: a `.yml` in a \
                 repository, a CI run, anything about deploys or checks passing is `gh \
                 workflow` / `gh run`. This tool is the user's own steps.",
            ),
            _ => None,
        }
    }
}

/// What the system prompt says about this capability.
///
/// Short on purpose, like every other note here. The rules that depend on where
/// a workflow has got to are in [`Workflow::render`], attached to the result
/// that raises them.
pub fn guidance(overlap: Overlap) -> String {
    format!(
        "`workflow` is for a job with several steps that you will carry out. Use `plan` with a \
         goal and {MIN_STEPS}–{MAX_STEPS} steps, `advance` to record what each one produced as \
         you finish it, `show` to re-read where you are, `save` to keep the shape for next \
         time, and `start` to run one you saved.\n\n\
         Reach for it when the work is long enough that the user would want to see the shape \
         before you begin — and not otherwise. A question you can answer, or a job that is two \
         tool calls, is not a workflow; planning it wastes their time and yours.\n\n\
         **When they ask you to work out the steps, call `plan` — do not write the steps out \
         in your answer instead.** A list in prose is not something they can reorder, annotate, \
         start or keep; it looks like the same thing and is not one.\n\n\
         **Plan, then stop.** Unless they have already told you to do the work, a plan is \
         something to show them, not something to start. They may rewrite any step, and a note \
         they add to a step is an instruction for it.\n\n\
         Do one step per `advance` and only after you have actually done it. If a step cannot \
         be done, say so with `stuck` and stop — do not carry on into the steps that depended \
         on it.\n\n\
         This is work *you* do. Something the user has to do themselves is a task for their \
         task list; something that should happen on a timer is a schedule.{}",
        overlap.note().unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps(count: usize) -> Vec<String> {
        (1..=count).map(|n| format!("step {n}")).collect()
    }

    fn planned() -> Workflow {
        Workflow::proposed("Quarterly comparison", steps(3)).expect("a workflow")
    }

    #[test]
    fn a_proposal_starts_on_its_first_step_and_has_not_been_greenlit() {
        let flow = planned();
        assert_eq!(flow.current(), Some(0));
        assert_eq!(flow.progress(), (0, 3));
        assert!(!flow.started);
        assert!(!flow.is_finished());
    }

    #[test]
    fn a_workflow_needs_a_goal_and_more_than_one_step() {
        assert!(Workflow::proposed("", steps(3)).is_err());

        // One step and no steps are different mistakes and get different
        // answers. The model sent the first line of its plan four times running
        // and was told each time that small jobs need no workflow, which was
        // true and not the problem it had.
        let Err(why) = Workflow::proposed("A thing", steps(1)) else {
            panic!("one step should not be a workflow");
        };
        assert!(why.contains("whole list in one call"), "{why}");
        assert!(why.contains("just do it"), "{why}");

        let Err(why) = Workflow::proposed("A thing", Vec::new()) else {
            panic!("no steps should not be a workflow");
        };
        assert!(why.contains("no steps arrived"), "{why}");
    }

    #[test]
    fn too_many_steps_is_refused_with_what_to_do_instead() {
        let Err(why) = Workflow::proposed("A thing", steps(MAX_STEPS + 1)) else {
            panic!("thirteen steps should be refused");
        };
        assert!(why.contains("Group the small ones"), "{why}");
    }

    #[test]
    fn a_step_that_is_a_paragraph_is_refused() {
        // A model given "steps" writes an essay into one often enough that
        // accommodating it would make the list unreadable in the window.
        let long = vec!["do a thing".to_string(), "x".repeat(MAX_STEP + 1)];
        let Err(why) = Workflow::proposed("A thing", long) else {
            panic!("a paragraph should not be a step");
        };
        assert!(why.contains("one line"), "{why}");
    }

    #[test]
    fn advancing_settles_the_current_step_and_moves_on() {
        let mut flow = planned();
        flow.advance(State::Done {
            outcome: "found 4 sheets".into(),
        })
        .expect("advance");

        assert_eq!(flow.current(), Some(1));
        assert_eq!(flow.progress(), (1, 3));
        // The first `advance` is the greenlight: a user who types "go" has said
        // yes as surely as one who clicked Start.
        assert!(flow.started);
    }

    #[test]
    fn advancing_past_the_end_says_so_rather_than_doing_nothing() {
        let mut flow = planned();
        for _ in 0..3 {
            flow.advance(State::Done {
                outcome: "ok".into(),
            })
            .expect("advance");
        }
        assert!(flow.is_finished());

        let Err(why) = flow.advance(State::Done {
            outcome: "ok".into(),
        }) else {
            panic!("a finished workflow has nothing to advance");
        };
        assert!(why.contains("already been settled"), "{why}");
    }

    #[test]
    fn a_stuck_step_stops_the_run_and_the_text_says_to_stop() {
        let mut flow = planned();
        let rendered = flow
            .advance(State::Stuck {
                why: "no access to the figures".into(),
            })
            .expect("advance");

        assert!(flow.is_stuck());
        assert!(rendered.contains("Stop here"), "{rendered}");
        assert!(rendered.contains("Do not skip ahead"), "{rendered}");
        // Still reports a current step — the run is over, not the list.
        assert_eq!(flow.current(), Some(1));
    }

    #[test]
    fn advancing_with_no_outcome_at_all_is_refused() {
        let mut flow = planned();
        let Err(why) = flow.advance(State::Pending) else {
            panic!("pending is not an outcome");
        };
        assert!(why.contains("pass an outcome"), "{why}");
    }

    #[test]
    fn revising_replaces_what_is_left_and_leaves_the_record_alone() {
        let mut flow = planned();
        flow.advance(State::Done {
            outcome: "found 4 sheets".into(),
        })
        .expect("advance");

        flow.revise(vec!["a new second".into(), "a new third".into()])
            .expect("revise");

        assert_eq!(flow.steps.len(), 3);
        assert_eq!(flow.steps[0].what, "step 1");
        assert_eq!(
            flow.steps[0].state,
            State::Done {
                outcome: "found 4 sheets".into()
            }
        );
        assert_eq!(flow.steps[1].what, "a new second");
        assert_eq!(flow.steps[2].what, "a new third");
    }

    #[test]
    fn a_revision_follows_the_users_note_to_wherever_its_step_went() {
        // The steering has to survive a reorder. Carrying it by position — the
        // first thing this did — moved the sentence onto a step it was never
        // about, which is worse than losing it: nothing about the result looks
        // wrong.
        let mut flow = planned();
        flow.steps[1].note = Some("use Q2, not Q1".into());

        flow.revise(vec![
            "Step 2".into(), // reordered to the front, and recapitalised
            "step 3".into(),
            "step 1".into(),
        ])
        .expect("revise");

        assert_eq!(flow.steps[0].note.as_deref(), Some("use Q2, not Q1"));
        assert_eq!(flow.steps[1].note, None);
        assert_eq!(flow.steps[2].note, None);
    }

    #[test]
    fn a_revision_that_drops_a_note_says_so_rather_than_swallowing_it() {
        // The one thing in here the user wrote themselves. Losing it quietly is
        // the failure the capability exists to avoid.
        let mut flow = planned();
        flow.steps[1].note = Some("use Q2, not Q1".into());

        let said = flow
            .revise(vec!["something else".into(), "and another".into()])
            .expect("revise");

        assert!(flow.steps.iter().all(|step| step.note.is_none()));
        assert!(said.contains("dropped 1 note"), "{said}");
        assert!(said.contains("Say so"), "{said}");
    }

    #[test]
    fn an_ordinary_revision_says_nothing_about_notes() {
        let mut flow = planned();
        let said = flow
            .revise(vec!["a new second".into(), "a new third".into()])
            .expect("revise");
        assert!(!said.contains("note"), "{said}");
    }

    #[test]
    fn a_revision_that_would_overflow_the_ceiling_is_refused() {
        let mut flow = Workflow::proposed("A thing", steps(4)).expect("a workflow");
        for _ in 0..3 {
            flow.advance(State::Done {
                outcome: "ok".into(),
            })
            .expect("advance");
        }
        let Err(why) = flow.revise(steps(MAX_STEPS)) else {
            panic!("three settled plus twelve is too many");
        };
        assert!(why.contains("already settled"), "{why}");
    }

    #[test]
    fn the_rendered_block_marks_the_current_step_and_carries_only_its_note() {
        // Every other step's note would be noise on this round, and the whole
        // reason a note lives in the result rather than the prompt is that it
        // is read at the moment it applies.
        let mut flow = planned();
        flow.steps[0].note = Some("start with the archive".into());
        flow.steps[1].note = Some("use Q2, not Q1".into());
        flow.advance(State::Done {
            outcome: "found 4 sheets".into(),
        })
        .expect("advance");

        let rendered = flow.render();
        assert!(rendered.contains("[NOW] step 2"), "{rendered}");
        assert!(rendered.contains("use Q2, not Q1"), "{rendered}");
        assert!(
            !rendered.contains("start with the archive"),
            "a settled step's note is not this round's business: {rendered}"
        );
        assert!(rendered.contains("→ found 4 sheets"), "{rendered}");
    }

    #[test]
    fn a_proposal_says_to_stop_and_a_started_one_says_to_carry_on() {
        let flow = planned();
        let proposed = flow.render();
        assert!(proposed.contains("Proposed, not started"), "{proposed}");
        assert!(proposed.contains("let them say go"), "{proposed}");
        // Read at the one moment it applies. In the system prompt this cost six
        // points net; see the comment on `next_move`.
        assert!(proposed.contains("not starting it"), "{proposed}");
        // The exception has to be in the same sentence, or a model told to
        // stop will stop even when the user's message was "do this".
        assert!(
            proposed.contains("already asked you to do the work"),
            "{proposed}"
        );

        let mut running = planned();
        running
            .advance(State::Done {
                outcome: "ok".into(),
            })
            .expect("advance");
        let carrying = running.render();
        assert!(carrying.contains("Do the step marked NOW"), "{carrying}");
        assert!(!carrying.contains("Proposed"), "{carrying}");
    }

    #[test]
    fn a_finished_workflow_says_to_answer_and_not_to_save_uninvited() {
        let mut flow = planned();
        for _ in 0..3 {
            flow.advance(State::Done {
                outcome: "ok".into(),
            })
            .expect("advance");
        }
        let rendered = flow.render();
        assert!(rendered.contains("do not save it uninvited"), "{rendered}");
        assert!(
            rendered.contains("rather than as another call"),
            "{rendered}"
        );
    }

    #[test]
    fn saving_keeps_the_shape_and_forgets_the_run() {
        let mut flow = planned();
        flow.steps[1].note = Some("use Q2".into());
        flow.advance(State::Done {
            outcome: "found 4 sheets".into(),
        })
        .expect("advance");

        let fresh = flow.fresh();
        assert!(!fresh.started);
        assert_eq!(fresh.progress(), (0, 3));
        // The user's steering is part of the shape; the outcome is not.
        assert_eq!(fresh.steps[1].note.as_deref(), Some("use Q2"));
        assert_eq!(fresh.steps[0].state, State::Pending);
    }

    #[test]
    fn a_saved_workflow_round_trips_through_markdown() {
        let mut flow = planned();
        flow.steps[1].note = Some("use Q2, not Q1".into());

        let text = flow.to_markdown();
        let read = Workflow::from_markdown(&text).expect("a workflow");
        assert_eq!(read.goal, "Quarterly comparison");
        assert_eq!(read.steps.len(), 3);
        assert_eq!(read.steps[1].what, "step 2");
        assert_eq!(read.steps[1].note.as_deref(), Some("use Q2, not Q1"));
    }

    #[test]
    fn markdown_a_person_wrote_by_hand_still_reads() {
        // The whole reason this is Markdown rather than JSON. Numbers that do
        // not ascend, `)` instead of `.`, ragged indentation — all of it is what
        // a text editor leaves behind.
        let text = "# Weekly review\n\nSome prose about it.\n\n\
                    1) Read the week's notes\n\
                    2) Pull the numbers\n   > only the ones from Planner\n\
                    2) Write the summary\n";
        let flow = Workflow::from_markdown(text).expect("a workflow");
        assert_eq!(flow.goal, "Weekly review");
        assert_eq!(flow.steps.len(), 3);
        assert_eq!(
            flow.steps[1].note.as_deref(),
            Some("only the ones from Planner")
        );
    }

    #[test]
    fn a_file_that_is_not_a_workflow_reads_as_none() {
        assert!(Workflow::from_markdown("").is_none());
        assert!(Workflow::from_markdown("# Just a heading\n\nand prose.").is_none());
        // Steps with no heading: there is no goal, so there is no workflow.
        assert!(Workflow::from_markdown("1. do a thing\n2. do another\n").is_none());
        // A quote before any step belongs to nothing and must not panic.
        assert!(Workflow::from_markdown("> a note about the file\n").is_none());
    }

    #[test]
    fn the_summary_says_where_it_is_without_the_whole_list() {
        let mut flow = planned();
        assert!(
            flow.summary().contains("3 steps proposed"),
            "{}",
            flow.summary()
        );

        flow.advance(State::Done {
            outcome: "ok".into(),
        })
        .expect("advance");
        assert_eq!(flow.summary(), "Step 2 of 3 · step 2");

        flow.advance(State::Stuck { why: "no".into() })
            .expect("advance");
        assert!(flow.summary().starts_with("Stuck at"), "{}", flow.summary());
    }

    #[test]
    fn an_action_is_read_out_of_whatever_the_model_sent() {
        assert_eq!(
            Action::parse(r#"{"action":"plan","goal":"A thing","steps":["one","two"]}"#),
            Action::Plan {
                goal: "A thing".into(),
                steps: vec!["one".into(), "two".into()]
            }
        );
        // No status means done — a model that sent an outcome has said what it
        // means, and refusing it would be pedantry.
        assert_eq!(
            Action::parse(r#"{"action":"advance","outcome":"found 4"}"#),
            Action::Advance(State::Done {
                outcome: "found 4".into()
            })
        );
        assert_eq!(
            Action::parse(r#"{"action":"ADVANCE","status":"stuck","outcome":"no access"}"#),
            Action::Advance(State::Stuck {
                why: "no access".into()
            })
        );
        assert_eq!(Action::parse(r#"{"action":"show"}"#), Action::Show);
        assert_eq!(
            Action::parse(r#"{"action":"start","name":"Weekly review"}"#),
            Action::Start("Weekly review".into())
        );
    }

    #[test]
    fn a_call_that_is_not_an_action_says_which_ones_are() {
        for arguments in ["{}", r#"{"action":"begin"}"#, "not json at all"] {
            let Action::Bad(why) = Action::parse(arguments) else {
                panic!("{arguments} is not an action");
            };
            assert!(why.contains("`advance`"), "{why}");
        }
    }

    #[test]
    fn advancing_or_showing_nothing_says_to_plan_first() {
        for action in [
            Action::Show,
            Action::Advance(State::Done {
                outcome: "x".into(),
            }),
        ] {
            let Err(why) = apply(&mut None, &action) else {
                panic!("there is nothing to act on");
            };
            assert!(why.contains("no workflow has been planned"), "{why}");
        }
    }

    #[test]
    fn planning_again_mid_run_revises_rather_than_starting_over() {
        // Replacing the whole thing would throw away steps already done and
        // reported, leaving the record disagreeing with the conversation.
        let mut state = Some(planned());
        apply(
            &mut state,
            &Action::Advance(State::Done {
                outcome: "found 4 sheets".into(),
            }),
        )
        .expect("advance");

        apply(
            &mut state,
            &Action::Plan {
                goal: "ignored while running".into(),
                steps: vec!["a new second".into(), "a new third".into()],
            },
        )
        .expect("plan again");

        let flow = state.expect("a workflow");
        assert_eq!(flow.goal, "Quarterly comparison");
        assert_eq!(flow.steps.len(), 3);
        assert_eq!(
            flow.steps[0].state,
            State::Done {
                outcome: "found 4 sheets".into()
            }
        );
        assert_eq!(flow.steps[1].what, "a new second");
    }

    #[test]
    fn planning_over_a_proposal_nobody_started_replaces_it() {
        let mut state = Some(planned());
        apply(
            &mut state,
            &Action::Plan {
                goal: "Something else".into(),
                steps: vec!["one".into(), "two".into()],
            },
        )
        .expect("plan");
        let flow = state.expect("a workflow");
        assert_eq!(flow.goal, "Something else");
        assert_eq!(flow.steps.len(), 2);
    }

    #[test]
    fn the_two_actions_that_touch_storage_are_the_callers() {
        // Absent by construction rather than by omission. Answering them here
        // with something plausible is the easiest way for the harness and the
        // application to start disagreeing about what a call did.
        for action in [Action::Save, Action::Start("anything".into())] {
            assert!(apply(&mut Some(planned()), &action).is_err());
        }
    }

    #[test]
    fn the_overlap_arms_differ_in_exactly_one_thing_each() {
        assert_eq!(Overlap::Current.gh_runs(), "workflow runs");
        assert_eq!(Overlap::Disambiguate.gh_runs(), "workflow runs");
        assert_eq!(Overlap::Reword.gh_runs(), "Actions runs");

        assert!(Overlap::Current.note().is_none());
        assert!(Overlap::Reword.note().is_none());
        assert!(Overlap::Disambiguate.note().is_some());

        assert_eq!(Overlap::parse("REWORD"), Some(Overlap::Reword));
        assert_eq!(Overlap::parse("nonsense"), None);
    }

    #[test]
    fn only_the_reword_arm_touches_githubs_prose() {
        // Read the const strings through `applied` rather than duplicating them
        // per arm, or the two sides of the experiment would drift and the run
        // would be comparing more than the one phrase.
        let said = "pull requests, issues and workflow runs, through the `gh` command line";
        assert_eq!(Overlap::Current.applied(said), said);
        assert_eq!(Overlap::Disambiguate.applied(said), said);
        assert!(
            Overlap::Reword.applied(said).contains("Actions runs"),
            "{}",
            Overlap::Reword.applied(said)
        );
    }

    #[test]
    fn the_guidance_says_what_this_is_not() {
        // The substitution this exists to stop is the one already on record:
        // asked for something recurring with only `planner` available, the
        // assistant made a task reminding the user to ask for it.
        let note = guidance(Overlap::Current);
        assert!(note.contains("task list"), "{note}");
        assert!(note.contains("schedule"), "{note}");
        assert!(note.contains("work *you* do"), "{note}");
        assert!(!note.contains("Actions"), "{note}");

        let disambiguated = guidance(Overlap::Disambiguate);
        assert!(disambiguated.contains("gh workflow"), "{disambiguated}");
    }
}
