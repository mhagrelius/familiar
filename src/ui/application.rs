//! The application: owns the projects, the open chat, and the client.
//!
//! Everything that writes a file funnels through here. The window reports what
//! the user did; this object applies it, persists it, and pushes the result
//! back down. Widgets emit intent and nothing else, so there is exactly one
//! place a conversation can be lost.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::{self, clone};

use crate::model::compaction::{self, Compacted, Headings};
use crate::model::images::{self, Attachment};
use crate::model::instructions::{date_line, Prompt, DEFAULT_PERSONA};
use crate::model::memory::{self, dream, harvest, Memory};
use crate::model::project::{Project, Store, ThreadSummary, DEFAULT_PROJECT};
use crate::model::settings::{Config, Settings};
use crate::model::thread::{StoredTurn, Thread, ThreadId};
use crate::model::tools;
use crate::model::turn::{Event, ToolCall, ToolOutcome, TurnState, TurnStream};
use crate::model::web::Budget;
use crate::model::wire::{ChatRequest, Message, ToolInvocation};
use crate::ui::approval::{self, Decision};
use crate::ui::client::{Client, ClientError};
use crate::ui::embedder::Embeddings;
use crate::ui::preferences::{self, Preferences};
use crate::ui::runner::Runner;
use crate::ui::{dialogs, Chip, TurnView, Window, WorkflowBar};
use crate::APP_ID;

/// How long to wait after a turn before reading it for anything durable.
///
/// Long enough that a follow-up typed straight away goes first, short enough
/// that closing the window seldom loses the read.
const HARVEST_DELAY: u32 = 5;

/// How long the nightly pass waits when it finds a turn in flight.
const DREAM_YIELD: u32 = 30;

/// How many rounds of tools one turn may take before Familiar stops it.
///
/// Room for real work, which is long: reading four documents and writing a deck
/// is six rounds before a single malformed argument or declined approval is
/// retried, and an old limit of six cut those turns off mid-chain. This is about
/// two and a half times the longest legitimate chain, which leaves the retries
/// room and still ends a runaway inside a minute.
///
/// It used to be 64, which was a runaway guard and nothing else. The eval suite
/// showed why that is not enough: a search spiral — a dozen or more queries and
/// no reply — is over in seven rounds, so a ceiling of 64 never came near it.
/// The ceiling cannot be the search budget either, since lowering it far enough
/// to catch one would cut the document chains this number was raised for. What
/// bounds searching is [`web::Budget`], per tool and per turn; this bounds
/// everything else.
///
/// What stops a turn the user has lost patience with is still Escape, which
/// cancels through the same `gio::Cancellable` the stop button uses.
const MAX_TOOL_ROUNDS: usize = 16;

/// How often the schedule is checked.
///
/// A minute is what Pika Backup and Claude Code Desktop both use, and it is the
/// right granularity: the grace window is twenty minutes, so a tick that lands
/// a few seconds late costs nothing, and the check is a comparison against a
/// `chrono` value rather than anything that touches the disk.
const HEARTBEAT_TICK: u32 = 60;

/// What the model is told as the last round of tools comes back. Defined beside
/// the fold in `model::turn` so the eval harness sends the same sentence at the
/// same moment; it used not to, and scored the app's honest ending as silence.
use crate::model::turn::{LAST_ROUND, WRAP_UP, WRAP_UP_AFTER};

/// The tools [`Budget`] governs: the two that go out and look something up.
///
/// `fetch_url` is not one of them *while the budget still has room* — it reads
/// a page the user named, which is a different act from searching for one, and
/// the guidance already tells the model not to fetch what a search returned.
/// Once the searches are gone it is counted, because at that point it has
/// stopped being "read this page" and become "find me that fact by another
/// route": see [`is_a_lookup`].
fn is_a_search(tool: &str) -> bool {
    matches!(tool, "web_search" | "news")
}

/// Whether this call is the turn still trying to look something up.
///
/// The measured failure it closes: three searches, then the budget refuses, and
/// the model goes after the same fact with `fetch_url` on a URL it invented and
/// `gh release list` — eight lookups in a turn that answered nothing. The
/// refusal told it to stop searching and it heard "stop using that tool".
///
/// `gh` is not in here and must not be: it acts on repositories, and a turn that
/// is genuinely doing GitHub work would be crippled by a budget meant for the
/// web. `fetch_url` is, but only once the searches are spent — before that it is
/// the ordinary "read this page" it has always been.
fn is_a_lookup(tool: &str) -> bool {
    is_a_search(tool) || tool == "fetch_url"
}

/// Whether a settled call was the budget refusing rather than a search.
fn refused_for_budget(call: &ToolCall) -> bool {
    matches!(&call.outcome, Some(ToolOutcome::Ok(text)) if text.starts_with("Not run:"))
}

/// A turn in flight. Dropped when it settles, which is also what makes a second
/// submission while one is running impossible to get wrong.
pub struct InFlight {
    pub question: String,
    /// Documents whose text was extracted, framed and ready to go into the
    /// question. Held for the turn, so a tool round still sees them.
    pub documents: Vec<String>,
    /// Images the question was asked with, sent on every round of the turn so
    /// the model can still see them when it reads its tool results.
    pub images: Vec<Attachment>,
    pub stream: TurnStream,
    pub view: Rc<TurnView>,
    pub cancellable: gio::Cancellable,
    /// What has already been said and done this turn, across tool rounds. A
    /// turn that calls a tool is several requests, and this is what makes them
    /// one turn on screen and one turn on disk.
    pub answer: String,
    pub thinking: String,
    pub calls: Vec<ToolCall>,
    /// Every tool exchange of this turn, in order: the assistant message that
    /// asked for a round of calls, then the result of each.
    ///
    /// The whole chain rides in every subsequent request. It used to be only
    /// the newest round, which meant round three could not see what round one
    /// found — a model doing multi-step work lost its own findings and either
    /// re-ran the call or answered from nothing. Carrying it is what makes a
    /// long chain add up to anything.
    pub exchanges: Vec<Message>,
    /// How many times the model has been given tool results and asked again.
    /// Bounded: a model that keeps calling tools forever must not be able to
    /// hold the app open.
    pub rounds: usize,
    /// An overflow has already been recovered from once this turn. Twice would
    /// be a loop.
    pub retried: bool,
    /// How many times the turn's tool results have been emptied to make room.
    /// Each attempt keeps fewer of them whole, and when there is nothing left
    /// to shrink the floor takes over.
    pub shrinks: usize,
    /// Send the floor — the first message and the current one — because the
    /// full history did not fit.
    pub floor: bool,
    /// The rolling summary this turn is being sent under, fixed at its first
    /// request. A fold that lands mid-turn belongs to the next one: applying it
    /// between two rounds would rewrite a prompt the server has already cached.
    pub fold: Option<compaction::Fold>,
    /// A gated tool has run. Retrying after this would repeat its side effects,
    /// so an overflow past this point is reported rather than recovered from.
    pub approved_something: bool,
    /// The clock started this turn, not a person. Decides whether the answer
    /// is announced, and what the thread records about the run.
    pub scheduled: bool,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Application {
        pub config: RefCell<Config>,
        pub settings: RefCell<Settings>,
        pub settings_path: RefCell<PathBuf>,
        pub store: RefCell<Option<Store>>,
        pub client: RefCell<Option<Rc<Client>>>,
        /// Every project, the default one first. Rebuilt from disk whenever one
        /// changes, so the sidebar and the store cannot disagree.
        pub projects: RefCell<Vec<Project>>,
        /// The one the open chat belongs to.
        pub project: RefCell<Project>,
        /// The open thread, including turns not yet written out.
        pub thread: RefCell<Thread>,
        pub in_flight: RefCell<Option<InFlight>>,
        pub window: RefCell<Option<Window>>,
        /// Brain's vault. `None` until one is configured, which is a state the
        /// tools report honestly rather than papering over.
        pub memory: Rc<RefCell<Option<Memory>>>,
        /// Watches the vault for changes made by Brain, git, or you.
        pub watcher: RefCell<Option<gio::FileMonitor>>,
        /// The memory block as it stood at the last thread boundary. Semi-
        /// volatile on purpose: recomputing it mid-turn would change the
        /// prompt and throw away the KV prefix llama-server cached.
        pub ambient: RefCell<Option<String>>,
        /// What the server said about itself, once it has been asked.
        pub server: RefCell<Option<crate::ui::client::ServerInfo>>,
        /// Which sibling command lines are on the `PATH`, remembered. Asked once
        /// per capability per round when the catalogue is composed, and the
        /// answer cannot change while this process is alive.
        pub installed: RefCell<std::collections::HashMap<String, bool>>,
        /// A summarization is in flight. One at a time, or a slow fold and the
        /// turn behind it would both extend the same summary and one would win.
        pub folding: Cell<bool>,
        /// The last thing the running tool said about itself, shown on its chip
        /// in place of the argument. Only a transcript produces any: for a call
        /// that takes four minutes, an unchanging spinner is indistinguishable
        /// from a hang, and this is the difference between the two.
        pub progress: RefCell<Option<String>>,
        /// The embedding thread, when a vault is configured. `None` is the
        /// ordinary state on a machine with no embedding server, and `recall`
        /// searches lexically without it.
        pub embeddings: RefCell<Option<Rc<Embeddings>>>,
        /// A passive read of a finished turn is in flight. One at a time: two
        /// readers proposing overlapping facts would both write, and the second
        /// would be checking for duplicates against a vault the first had not
        /// finished changing.
        pub harvesting: Cell<bool>,
        /// The nightly consolidation is running. A pass is many requests over
        /// several minutes and must not be started twice by two ticks.
        pub dreaming: Cell<bool>,
    }

    impl Default for Project {
        fn default() -> Self {
            Project::default_project()
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Application {
        const NAME: &'static str = "FamiliarApplication";
        type Type = super::Application;
        type ParentType = adw::Application;
    }

    impl ObjectImpl for Application {}

    impl ApplicationImpl for Application {
        fn startup(&self) {
            // Chain up first: the toolkit initialises in the parent handler and
            // anything touching GTK before it is undefined.
            self.parent_startup();

            let obj = self.obj();
            if let Some(display) = gtk::gdk::Display::default() {
                crate::ui::load_stylesheet(&display);
            }
            obj.install_actions();
            obj.load_state();
            obj.watch_vault();
            // Not only on a change: a vault that has never been embedded, or
            // one whose embedding server was down last time, has to be caught
            // up by something, and nothing here waits on it.
            obj.catch_up_vectors();
            obj.start_heartbeat();
        }

        fn activate(&self) {
            self.parent_activate();
            let obj = self.obj();

            let existing = self.window.borrow().clone();
            let window = match existing {
                Some(window) => window,
                None => {
                    let window = Window::new(&*obj);
                    obj.connect_window(&window);
                    self.window.replace(Some(window.clone()));
                    window
                }
            };

            // Restore the size before presenting, so the window never appears
            // at one size and jumps to another.
            {
                let settings = self.settings.borrow();
                if let (Some(width), Some(height)) = (settings.window_width, settings.window_height)
                {
                    // A stored size of zero is a record of a window that was
                    // never mapped; the built-in default is the better answer.
                    if width > 0 && height > 0 {
                        window.set_default_size(width.max(360), height.max(360));
                    }
                }
                if settings.window_maximized {
                    window.maximize();
                }
            }
            window.present();

            obj.refresh_threads();
            obj.open_latest_thread();
            obj.probe_server();
            window.composer().focus_entry();
        }

        /// Entry point for the desktop file's actions and for a second launch
        /// of an already-running instance.
        fn command_line(&self, command_line: &gio::ApplicationCommandLine) -> glib::ExitCode {
            let obj = self.obj();
            // Activating first means the window exists before anything is asked
            // of it, whether this is the first launch or the fifth.
            obj.activate();

            // `--new-thread` is what the desktop file passed before chats were
            // called chats, and a launcher entry the shell cached still does.
            if command_line.arguments().iter().any(|argument| {
                matches!(
                    argument.to_string_lossy().as_ref(),
                    "--new-chat" | "--new-thread"
                )
            }) {
                let slug = self.project.borrow().slug.clone();
                obj.new_thread(&slug);
            }
            glib::ExitCode::SUCCESS
        }

        fn shutdown(&self) {
            let obj = self.obj();
            obj.remember_window();
            obj.save_thread();
            self.parent_shutdown();
        }
    }

    impl GtkApplicationImpl for Application {}
    impl AdwApplicationImpl for Application {}
}

glib::wrapper! {
    pub struct Application(ObjectSubclass<imp::Application>)
        @extends adw::Application, gtk::Application, gio::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl Default for Application {
    fn default() -> Self {
        Self::new()
    }
}

impl Application {
    pub fn new() -> Self {
        glib::Object::builder()
            .property("application-id", APP_ID)
            // The desktop file offers a "New Chat" action, which arrives as
            // a command line on the running instance.
            .property("flags", gio::ApplicationFlags::HANDLES_COMMAND_LINE)
            .build()
    }

    fn install_actions(&self) {
        let quit = gio::SimpleAction::new("quit", None);
        quit.connect_activate(clone!(
            #[weak(rename_to = app)]
            self,
            move |_, _| app.quit()
        ));
        self.add_action(&quit);
        self.set_accels_for_action("app.quit", &["<Control>q"]);

        let show_preferences = gio::SimpleAction::new("preferences", None);
        show_preferences.connect_activate(clone!(
            #[weak(rename_to = app)]
            self,
            move |_, _| app.show_preferences()
        ));
        self.add_action(&show_preferences);
        self.set_accels_for_action("app.preferences", &["<Control>comma"]);

        // Opens the thread a notification came from. Takes the id as a target
        // so one action serves every scheduled thread, and it can activate the
        // app from cold — a notification outlives the process that sent it.
        let show_thread = gio::SimpleAction::new("show-thread", Some(glib::VariantTy::STRING));
        show_thread.connect_activate(clone!(
            #[weak(rename_to = app)]
            self,
            move |_, target| {
                // "slug/id": a chat id is unique within a project, not
                // across them, so the notification has to name both.
                let Some(target) = target.and_then(|target| target.str().map(str::to_string))
                else {
                    return;
                };
                if let Some((slug, id)) = target.split_once('/') {
                    app.open_thread(slug, id);
                }
                if let Some(window) = app.window() {
                    window.present();
                }
            }
        ));
        self.add_action(&show_thread);

        // The window that lists every scheduled thread, so one can be paused,
        // edited or deleted without hunting through the sidebar for it.
        let schedules = gio::SimpleAction::new("schedules", None);
        schedules.connect_activate(clone!(
            #[weak(rename_to = app)]
            self,
            move |_, _| app.show_schedules()
        ));
        self.add_action(&schedules);

        let about = gio::SimpleAction::new("about", None);
        about.connect_activate(clone!(
            #[weak(rename_to = app)]
            self,
            move |_, _| app.show_about()
        ));
        self.add_action(&about);

        self.set_accels_for_action("win.new-thread", &["<Control>n"]);
        // Shift as well as Control: a plain Ctrl+E is close enough to the
        // editing bindings a text view already answers to.
        self.set_accels_for_action("win.explain-selection", &["<Control><Shift>e"]);
        self.set_accels_for_action("win.shortcuts", &["<Control>question"]);
        self.set_accels_for_action("win.toggle-sidebar", &["F9"]);
    }

    fn load_state(&self) {
        let imp = self.imp();

        let (config, _) = Config::load(&Config::default_path());
        let settings_path = Settings::default_path();
        let (settings, _) = Settings::load(&settings_path);

        imp.client
            .replace(Some(Rc::new(Client::new(&config.server_url))));

        // The vault is Brain's, and by default it is literally Brain's: the
        // path it recorded when you chose one. Nothing is copied and nothing is
        // imported — the two apps hold the same folder open.
        let vault = config.vault.clone().or_else(memory::brain_vault);
        if let Some(root) = vault.filter(|root| root.is_dir()) {
            imp.memory.replace(Some(Memory::open(&root)));
            // Started here and connected lazily, so a machine with no embedding
            // server pays nothing at launch and picks one up if it appears.
            imp.embeddings.replace(Some(Rc::new(Embeddings::start(
                &memory::brain_embedding_url(),
            ))));
        }
        imp.store.replace(Some(Store::new(Store::default_root())));
        imp.config.replace(config);
        imp.settings.replace(settings);
        imp.settings_path.replace(settings_path);

        let store = imp.store.borrow().clone();
        if let Some(store) = store {
            // Before anything reads the store: the layout it wrote before
            // projects had a name is renamed once, in place, and everything
            // after this point sees only the new one.
            if let Err(error) = store.migrate() {
                eprintln!("familiar: could not move the old layout across: {error}");
            }
            let projects = store
                .projects()
                .unwrap_or_else(|_| vec![Project::default_project()]);
            if let Some(default) = projects.iter().find(|p| p.is_default()).cloned() {
                imp.project.replace(default);
            }
            imp.projects.replace(projects);
            if let Ok(thread) = store.new_thread(DEFAULT_PROJECT) {
                imp.thread.replace(thread);
            }
        }
    }

    /// Notice when Brain, git or you change a note.
    ///
    /// `gio::FileMonitor` rather than the `notify` crate: gio is already a
    /// dependency and its monitors deliver on the GLib main loop, so there is
    /// no channel and no thread. One monitor on the vault root is enough to
    /// know that *something* changed, and re-indexing is 29 ms.
    fn watch_vault(&self) {
        let imp = self.imp();
        let root = imp
            .memory
            .borrow()
            .as_ref()
            .map(|memory| memory.root().to_path_buf());
        let Some(root) = root else { return };

        let Ok(monitor) = gio::File::for_path(&root)
            .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        else {
            return;
        };

        monitor.connect_changed(clone!(
            #[weak(rename_to = app)]
            self,
            move |_, _, _, event| {
                use gio::FileMonitorEvent::*;
                if !matches!(
                    event,
                    Created | Deleted | MovedIn | MovedOut | ChangesDoneHint
                ) {
                    return;
                }
                if let Some(memory) = app.imp().memory.borrow_mut().as_mut() {
                    memory.rescan();
                }
                app.catch_up_vectors();
            }
        ));
        imp.watcher.replace(Some(monitor));
    }

    /// Bring the vault's vectors level with the vault.
    ///
    /// Fire and forget. The pass runs on the embedding thread, and what comes
    /// back is a store that [`Memory::recall`] will use next time it is asked.
    /// Nothing waits on it: until it lands — or for ever, on a machine with no
    /// embedding server — `recall` matches words, which is what it did before
    /// any of this existed.
    fn catch_up_vectors(&self) {
        let imp = self.imp();
        let Some(embeddings) = imp.embeddings.borrow().clone() else {
            return;
        };
        let (passages, store_path) = {
            let borrowed = imp.memory.borrow();
            let Some(memory) = borrowed.as_ref() else {
                return;
            };
            (
                memory.passages().to_vec(),
                memory.store_path().to_path_buf(),
            )
        };
        if passages.is_empty() {
            return;
        }
        embeddings.catch_up(
            passages,
            store_path,
            clone!(
                #[weak(rename_to = app)]
                self,
                move |store| {
                    let Some(store) = store else { return };
                    if let Some(memory) = app.imp().memory.borrow_mut().as_mut() {
                        memory.set_semantic(store);
                    }
                }
            ),
        );
    }

    // -- remembering without being asked --------------------------------------

    /// Read a finished turn for anything durable, and save it.
    ///
    /// The conversational model has already answered and moved on; this is a
    /// second, separate generation with no tools, whose only job is to notice.
    /// It exists because the facts worth keeping arrive in the middle of asking
    /// for something else, and a model in the middle of answering has one job it
    /// is already doing.
    ///
    /// Everything about it is best-effort. A server that refuses, a reply that
    /// is not JSON, a candidate the vetting throws out — each of those ends with
    /// nothing saved and nothing said, which is the same outcome as the turn
    /// having held nothing worth keeping.
    fn harvest_turn(&self, question: &str, answer: &str) {
        let imp = self.imp();
        if !imp.settings.borrow().passive_memory
            || !imp.project.borrow().tools.memory
            || !imp.config.borrow().memory
            || imp.harvesting.get()
        {
            return;
        }
        // The cheap gate first, so most turns cost nothing at all.
        if !harvest::worth_reading(question) {
            return;
        }
        if imp.client.borrow().is_none() {
            return;
        }

        // A short pause before it goes anywhere. This is a second generation
        // against the same GPU the conversation uses, and on a one-slot server
        // a follow-up typed straight away would queue behind four hundred
        // tokens of it. Whoever is still typing wins the race; whoever has
        // stopped gets their memory saved a few seconds later, which is a
        // moment nobody is waiting on.
        imp.harvesting.set(true);
        let question = question.to_string();
        let answer = answer.to_string();
        glib::timeout_add_seconds_local_once(
            HARVEST_DELAY,
            clone!(
                #[weak(rename_to = app)]
                self,
                move || app.read_turn(&question, &answer)
            ),
        );
    }

    /// The part of [`Self::harvest_turn`] that actually sends.
    ///
    /// What the vault already says is gathered here rather than before the
    /// pause, so the reader is told what is true when it runs — a turn's own
    /// `remember` call, or another thread, may have written something in the
    /// meantime.
    fn read_turn(&self, question: &str, answer: &str) {
        let imp = self.imp();
        // Someone started typing. Their turn matters and this does not; the
        // next one they finish will be read instead.
        let Some(client) = imp
            .client
            .borrow()
            .clone()
            .filter(|_| imp.in_flight.borrow().is_none())
        else {
            imp.harvesting.set(false);
            return;
        };
        let known: Vec<String> = {
            let borrowed = imp.memory.borrow();
            let Some(memory) = borrowed.as_ref() else {
                imp.harvesting.set(false);
                return;
            };
            let (core, _) = memory.ranked(chrono::Utc::now());
            let mut lines: Vec<String> = core
                .iter()
                .map(|ranked| ranked.observation.line())
                .collect();
            // Whatever the note for a subject in this turn already says, so the
            // reader is not made to re-derive what is already written down.
            lines.extend(
                memory
                    .observations()
                    .iter()
                    .filter(|held| {
                        question
                            .to_lowercase()
                            .contains(&held.subject.to_lowercase())
                    })
                    .map(|held| held.line()),
            );
            lines.sort();
            lines.dedup();
            lines
        };

        let request = harvest::request(question, answer, &known);
        let stream = Rc::new(RefCell::new(TurnStream::new()));
        let cancellable = client.stream(
            &request,
            {
                let stream = stream.clone();
                move |text: &str| {
                    stream.borrow_mut().push(text);
                }
            },
            clone!(
                #[weak(rename_to = app)]
                self,
                move |outcome| {
                    app.imp().harvesting.set(false);
                    if outcome.is_err() {
                        return;
                    }
                    let mut borrowed = stream.borrow_mut();
                    borrowed.end();
                    let state = std::mem::take(&mut *borrowed).finish();
                    drop(borrowed);
                    app.save_harvest(&known, &state.answer);
                }
            ),
        );
        std::mem::forget(cancellable);
    }

    /// Write what the reader proposed, and say so in the thread.
    ///
    /// Said out loud rather than done quietly. A note in the conversation is
    /// the same mechanism compaction uses to report what left the model's view,
    /// and for the same reason: an assistant that changes your notes without
    /// telling you is one you cannot trust with them.
    fn save_harvest(&self, known: &[String], reply: &str) {
        let candidates = harvest::vet(harvest::parse(reply), known);
        if candidates.is_empty() {
            return;
        }
        let now = chrono::Utc::now();
        let mut saved = Vec::new();
        {
            let mut borrowed = self.imp().memory.borrow_mut();
            let Some(memory) = borrowed.as_mut() else {
                return;
            };
            for candidate in candidates {
                match memory.remember(
                    &candidate.subject,
                    &candidate.observation,
                    candidate.kind,
                    None,
                    now,
                ) {
                    Ok(written) if !written.already_there => {
                        saved.push(format!("{}: {}", candidate.subject, candidate.observation));
                    }
                    _ => {}
                }
            }
        }
        if saved.is_empty() {
            return;
        }
        // A toast and a thread note, which is exactly what compaction does when
        // it changes the model's view — one place to look for "what did it just
        // do to my conversation", whichever of the two did it.
        let note = match saved.len() {
            1 => format!("Noted — {}", saved[0]),
            _ => format!("Noted:\n- {}", saved.join("\n- ")),
        };
        let toast = match saved.len() {
            1 => format!("Noted — {}", saved[0]),
            many => format!("Noted {many} things in your notes."),
        };
        self.imp().thread.borrow_mut().push_note(note);
        self.save_thread();
        if let Some(window) = self.window() {
            window.toast(&toast);
        }
    }

    // -- dreaming -------------------------------------------------------------

    /// Start the nightly consolidation, if it is time and nothing is in the way.
    fn dream_if_due(&self) {
        let imp = self.imp();
        if !imp.settings.borrow().dreaming || imp.dreaming.get() || imp.memory.borrow().is_none() {
            return;
        }
        let now = chrono::Local::now();
        let (schedule, last) = {
            let settings = imp.settings.borrow();
            (
                settings.dream_schedule(),
                settings
                    .last_dream
                    .map(|at| at.with_timezone(&chrono::Local)),
            )
        };
        // Never run means start counting from now rather than from the epoch,
        // which is the same rule a scheduled thread follows. Written out, so a
        // relaunch does not reset the clock every time.
        if last.is_none() {
            let mut settings = imp.settings.borrow_mut();
            settings.last_dream = Some(chrono::Utc::now());
            let _ = settings.save(&imp.settings_path.borrow());
            return;
        }
        if schedule.due(last, now).is_none() {
            return;
        }
        // Recorded before the pass rather than after: if it never finishes, the
        // schedule must still have moved on, or every tick inside the grace
        // window starts it again.
        {
            let mut settings = imp.settings.borrow_mut();
            settings.last_dream = Some(chrono::Utc::now());
            let _ = settings.save(&imp.settings_path.borrow());
        }
        self.dream();
    }

    /// One night's pass.
    ///
    /// The arithmetic half runs first and needs no server, so a machine whose
    /// model is not running still gets duplicates collapsed and dead lines
    /// cleared. Then the model's half, one batch at a time — chained rather than
    /// launched together, because this is the same GPU the conversation uses and
    /// nobody is waiting for the result.
    fn dream(&self) {
        let imp = self.imp();
        imp.dreaming.set(true);
        let now = chrono::Utc::now();

        // What has actually come up lately. Slow, and this is the one caller
        // that can afford it.
        let transcripts = self
            .store()
            .map(|store| store.recent_conversations(now - chrono::Duration::days(60), 200))
            .unwrap_or_default();

        let (applied, batches) = {
            let mut borrowed = imp.memory.borrow_mut();
            let Some(memory) = borrowed.as_mut() else {
                imp.dreaming.set(false);
                return;
            };
            let mentions = dream::mentions(memory.observations(), &transcripts);
            let held = memory.held(&mentions);
            let applied = memory.dream(
                &dream::arithmetic(&held, now, &dream::Policy::default()),
                now,
            );

            // Recomputed after the arithmetic pass, so the model is shown the
            // vault as it now is rather than as it was an hour ago.
            let mentions = dream::mentions(memory.observations(), &transcripts);
            let held = memory.held(&mentions);
            let batches: Vec<Vec<dream::Held>> = held
                .chunks(dream::BATCH)
                .map(<[dream::Held]>::to_vec)
                .collect();
            (applied, batches)
        };

        let Some(client) = imp.client.borrow().clone() else {
            self.finish_dream(applied, now);
            return;
        };
        if batches.is_empty() {
            self.finish_dream(applied, now);
            return;
        }
        // What the night has left to spend, after the arithmetic pass took its
        // share. Carried from batch to batch because `Policy` bounds one plan,
        // and a large vault is ten of them: without this, a hundred-line memory
        // could lose a quarter of every batch and blow through the whole-night
        // ceiling ten times over.
        let budget = dream::Policy::default()
            .most_drops
            .saturating_sub(applied.dropped.len());
        self.dream_batch(client, batches, 0, budget, applied, now);
    }

    /// Ask about one batch, apply what comes back, then move to the next.
    fn dream_batch(
        &self,
        client: Rc<Client>,
        batches: Vec<Vec<dream::Held>>,
        at: usize,
        budget: usize,
        applied: dream::Applied,
        now: chrono::DateTime<chrono::Utc>,
    ) {
        let Some(batch) = batches.get(at).cloned() else {
            self.finish_dream(applied, now);
            return;
        };
        // A pass is many requests over several minutes against the same server
        // the conversation uses. If somebody is awake after all, wait for them
        // rather than putting a batch in front of their answer.
        if self.imp().in_flight.borrow().is_some() {
            glib::timeout_add_seconds_local_once(
                DREAM_YIELD,
                clone!(
                    #[weak(rename_to = app)]
                    self,
                    move || app.dream_batch(client, batches, at, budget, applied, now)
                ),
            );
            return;
        }
        // The night is out of room. What is left waits for tomorrow, which is
        // the whole point of a budget: nothing here is urgent.
        if budget == 0 {
            self.finish_dream(applied, now);
            return;
        }
        let policy = dream::Policy {
            most_drops: budget,
            ..dream::Policy::default()
        };
        let request = dream::request(&batch, now);
        let stream = Rc::new(RefCell::new(TurnStream::new()));
        let cancellable = client.clone().stream(
            &request,
            {
                let stream = stream.clone();
                move |text: &str| {
                    stream.borrow_mut().push(text);
                }
            },
            clone!(
                #[weak(rename_to = app)]
                self,
                move |outcome| {
                    let mut applied = applied;
                    let mut spent = 0;
                    if outcome.is_ok() {
                        let mut borrowed = stream.borrow_mut();
                        borrowed.end();
                        let state = std::mem::take(&mut *borrowed).finish();
                        drop(borrowed);

                        let plan =
                            dream::parse(&state.answer, &batch).bounded(&batch, &policy, now);
                        spent = plan.drops();
                        if let Some(memory) = app.imp().memory.borrow_mut().as_mut() {
                            let done = memory.dream(&plan, now);
                            applied.dropped.extend(done.dropped);
                            applied.merged += done.merged;
                            applied.reclassified += done.reclassified;
                            applied.failed += done.failed;
                        }
                    }
                    // A batch the server never answered is skipped rather than
                    // ending the night: the rest of the vault is still worth
                    // looking at, and the next pass sees this batch again.
                    app.dream_batch(
                        client,
                        batches,
                        at + 1,
                        budget.saturating_sub(spent),
                        applied,
                        now,
                    );
                }
            ),
        );
        std::mem::forget(cancellable);
    }

    /// Write the night down and say what it did.
    fn finish_dream(&self, applied: dream::Applied, now: chrono::DateTime<chrono::Utc>) {
        let imp = self.imp();
        imp.dreaming.set(false);
        if let Some(memory) = imp.memory.borrow_mut().as_mut() {
            memory.flush_ledger();
        }
        let Some(summary) = applied.describe() else {
            return;
        };

        // Every removed sentence, kept outside the vault, so "it forgot
        // something I wanted" has an answer that is not "it is gone".
        let path = dream::Journal::default_path();
        let mut journal = dream::Journal::load(&path);
        journal.record(applied, now);
        let _ = journal.save(&path);

        // Low priority: this happened at three in the morning and is a report,
        // not a request. GNOME holds it in the shade until somebody looks.
        let notification = gio::Notification::new("Tidied your saved memory");
        notification.set_body(Some(&format!(
            "{summary}. What went is kept in dreams.json."
        )));
        notification.set_priority(gio::NotificationPriority::Low);
        self.send_notification(Some("dream"), &notification);

        self.refresh_ambient();
    }

    /// Recompute the ambient memory block.
    ///
    /// At thread boundaries only. A fact the model writes mid-turn shows up in
    /// Background at the next thread switch, and is findable with `recall` in
    /// the meantime — which is the trade that keeps the cached prefix intact.
    fn refresh_ambient(&self) {
        let imp = self.imp();
        let block = if imp.project.borrow().tools.memory && imp.config.borrow().memory {
            imp.memory
                .borrow()
                .as_ref()
                .and_then(|memory| memory.ambient(chrono::Utc::now()))
        } else {
            None
        };
        imp.ambient.replace(block);
    }

    fn connect_window(&self, window: &Window) {
        // A closed window reports a size of zero, and shutdown runs after the
        // close, so the shape has to be taken while the window is still up.
        window.connect_close_request(glib::clone!(
            #[weak(rename_to = app)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_| {
                app.remember_window();
                glib::Propagation::Proceed
            }
        ));
        window.connect_closure(
            "submit",
            false,
            glib::closure_local!(
                #[watch(rename_to = app)]
                self,
                move |_: Window, text: String| app.submit(&text)
            ),
        );

        let bar = window.workflow_bar();
        // Start is the greenlight by the other route. The model treats the
        // user's "go" the same way — `Workflow::advance` sets `started` — so
        // this button and that sentence have to do the same thing, and both
        // just say the plan is live and ask for the first step.
        bar.connect_closure(
            "start",
            false,
            glib::closure_local!(
                #[watch(rename_to = app)]
                self,
                move |_: WorkflowBar| app.start_workflow()
            ),
        );
        bar.connect_closure(
            "edit",
            false,
            glib::closure_local!(
                #[watch(rename_to = app)]
                self,
                move |_: WorkflowBar| app.edit_workflow()
            ),
        );
        bar.connect_closure(
            "stop",
            false,
            glib::closure_local!(
                #[watch(rename_to = app)]
                self,
                move |_: WorkflowBar| app.stop_workflow()
            ),
        );
        window.connect_closure(
            "stop",
            false,
            glib::closure_local!(
                #[watch(rename_to = app)]
                self,
                move |_: Window| app.stop()
            ),
        );
        window.connect_closure(
            "new-thread",
            false,
            glib::closure_local!(
                #[watch(rename_to = app)]
                self,
                move |_: Window, slug: String| app.new_thread(&slug)
            ),
        );
        window.connect_closure(
            "thread-chosen",
            false,
            glib::closure_local!(
                #[watch(rename_to = app)]
                self,
                move |_: Window, slug: String, id: String| app.open_thread(&slug, &id)
            ),
        );
        window.connect_closure(
            "thread-rename",
            false,
            glib::closure_local!(
                #[watch(rename_to = app)]
                self,
                move |_: Window, slug: String, id: String| app.rename_thread(&slug, &id)
            ),
        );
        window.connect_closure(
            "thread-delete",
            false,
            glib::closure_local!(
                #[watch(rename_to = app)]
                self,
                move |_: Window, slug: String, id: String| app.delete_thread(&slug, &id)
            ),
        );
        window.connect_closure(
            "project-action",
            false,
            glib::closure_local!(
                #[watch(rename_to = app)]
                self,
                move |_: Window, action: String, slug: String| app.project_action(&action, &slug)
            ),
        );
        // From the open project's page, which does not name a project because
        // the page can only ever be the open one's.
        window.connect_closure(
            "page-project-action",
            false,
            glib::closure_local!(
                #[watch(rename_to = app)]
                self,
                move |_: Window, action: String| {
                    let slug = app.imp().project.borrow().slug.clone();
                    app.project_action(&action, &slug);
                }
            ),
        );
        window.connect_closure(
            "page-chat-chosen",
            false,
            glib::closure_local!(
                #[watch(rename_to = app)]
                self,
                move |_: Window, id: String| {
                    let slug = app.imp().project.borrow().slug.clone();
                    app.open_thread(&slug, &id);
                }
            ),
        );
        window.connect_closure(
            "file-action",
            false,
            glib::closure_local!(
                #[watch(rename_to = app)]
                self,
                move |_: Window, action: String, path: String| {
                    let slug = app.imp().project.borrow().slug.clone();
                    app.file_action(&action, &slug, std::path::Path::new(&path));
                }
            ),
        );
        window.connect_closure(
            "complain",
            false,
            glib::closure_local!(
                #[watch(rename_to = app)]
                self,
                move |_: Window, text: String| {
                    if let Some(window) = app.window() {
                        window.toast(&text);
                    }
                }
            ),
        );
        window.connect_closure(
            "retry",
            false,
            glib::closure_local!(
                #[watch(rename_to = app)]
                self,
                move |_: Window| app.probe_server()
            ),
        );

        // Window actions act on what is open, which is what a menu bar item
        // means when it says "Rename Thread…".
        let new_thread = gio::SimpleAction::new("new-thread", None);
        new_thread.connect_activate(clone!(
            #[weak(rename_to = app)]
            self,
            move |_, _| {
                let slug = app.imp().project.borrow().slug.clone();
                app.new_thread(&slug);
            }
        ));
        window.add_action(&new_thread);

        let rename_thread = gio::SimpleAction::new("rename-thread", None);
        rename_thread.connect_activate(clone!(
            #[weak(rename_to = app)]
            self,
            move |_, _| {
                let (slug, id) = app.open_thread_id();
                app.rename_thread(&slug, &id);
            }
        ));
        window.add_action(&rename_thread);

        let delete_thread = gio::SimpleAction::new("delete-thread", None);
        delete_thread.connect_activate(clone!(
            #[weak(rename_to = app)]
            self,
            move |_, _| {
                let (slug, id) = app.open_thread_id();
                app.delete_thread(&slug, &id);
            }
        ));
        window.add_action(&delete_thread);

        let schedule_thread = gio::SimpleAction::new("schedule-thread", None);
        schedule_thread.connect_activate(clone!(
            #[weak(rename_to = app)]
            self,
            move |_, _| app.schedule_thread()
        ));
        window.add_action(&schedule_thread);

        let new_project = gio::SimpleAction::new("new-project", None);
        new_project.connect_activate(clone!(
            #[weak(rename_to = app)]
            self,
            move |_, _| app.new_project()
        ));
        window.add_action(&new_project);

        let open_project = gio::SimpleAction::new("open-project", None);
        open_project.connect_activate(clone!(
            #[weak(rename_to = app)]
            self,
            move |_, _| {
                let slug = app.imp().project.borrow().slug.clone();
                app.open_project(&slug);
            }
        ));
        window.add_action(&open_project);

        let edit_project = gio::SimpleAction::new("edit-project", None);
        edit_project.connect_activate(clone!(
            #[weak(rename_to = app)]
            self,
            move |_, _| {
                let slug = app.imp().project.borrow().slug.clone();
                app.edit_project(&slug);
            }
        ));
        window.add_action(&edit_project);

        let delete_project = gio::SimpleAction::new("delete-project", None);
        delete_project.connect_activate(clone!(
            #[weak(rename_to = app)]
            self,
            move |_, _| {
                let slug = app.imp().project.borrow().slug.clone();
                app.delete_project(&slug);
            }
        ));
        window.add_action(&delete_project);

        let shortcuts = gio::SimpleAction::new("shortcuts", None);
        shortcuts.connect_activate(clone!(
            #[weak]
            window,
            move |_, _| show_shortcuts(&window)
        ));
        window.add_action(&shortcuts);
    }

    fn window(&self) -> Option<Window> {
        self.imp().window.borrow().clone()
    }

    fn store(&self) -> Option<Store> {
        self.imp().store.borrow().clone()
    }

    // -- threads -------------------------------------------------------------

    fn refresh_threads(&self) {
        let (Some(window), Some(store)) = (self.window(), self.store()) else {
            return;
        };

        let projects = match store.projects() {
            Ok(projects) => projects,
            Err(error) => {
                window.toast(&format!("Could not read the projects: {error}"));
                vec![Project::default_project()]
            }
        };

        let listed: Vec<(Project, Vec<ThreadSummary>)> = projects
            .iter()
            .map(|project| {
                let threads = store.threads(&project.slug).unwrap_or_default();
                (project.clone(), threads)
            })
            .collect();

        window.set_projects(&listed);
        self.imp().projects.replace(projects);

        let slug = self.imp().project.borrow().slug.clone();
        // A project page is a view of the same things the sidebar just
        // rebuilt — its chats, its files, what it has running — so it is drawn
        // again here rather than going stale behind a finished turn.
        if window.showing_project() {
            self.render_project_page(&slug);
            return;
        }
        let thread = self.imp().thread.borrow();
        let open = (!thread.is_empty()).then(|| thread.id.clone());
        drop(thread);
        window.select_thread(&slug, open.as_ref());
    }

    /// The open chat, as the pair the actions want.
    fn open_thread_id(&self) -> (String, String) {
        let imp = self.imp();
        let slug = imp.project.borrow().slug.clone();
        let id = imp.thread.borrow().id.to_string();
        (slug, id)
    }

    /// Reopen the most recent conversation, which is almost always the one you
    /// were in.
    fn open_latest_thread(&self) {
        let Some(store) = self.store() else {
            return;
        };
        let slug = self.imp().project.borrow().slug.clone();
        let latest = store
            .threads(&slug)
            .ok()
            .and_then(|threads| threads.into_iter().next());
        match latest {
            Some(summary) => self.open_thread(&slug, summary.id.as_str()),
            None => self.show_thread(),
        }
    }

    fn open_thread(&self, slug: &str, id: &str) {
        let (Some(store), Some(id)) = (self.store(), ThreadId::from_stem(id)) else {
            return;
        };
        if self.imp().thread.borrow().id == id && self.imp().project.borrow().slug == slug {
            // Already the open chat, and reloading it would be work for
            // nothing — unless the project's page is what is on screen, in
            // which case "open it" means show the conversation again. Without
            // this, clicking the chat you were last in from its own project
            // page did nothing at all.
            if let Some(window) = self.window().filter(Window::showing_project) {
                self.show_thread();
                // And the highlight comes back off the project row onto the
                // chat, which is where you now are.
                window.select_thread(slug, Some(&id));
            }
            return;
        }
        self.save_thread();
        match store.load_thread(slug, &id) {
            Ok(thread) => {
                self.enter_project(slug);
                self.imp().thread.replace(thread);
                self.show_thread();
                self.refresh_threads();
            }
            Err(error) => {
                if let Some(window) = self.window() {
                    window.toast(&format!("Could not open that chat: {error}"));
                }
            }
        }
    }

    fn new_thread(&self, slug: &str) {
        self.save_thread();
        self.start_fresh_thread(slug);
    }

    /// Open a new thread *without* writing the current one out.
    ///
    /// Split from `new_thread` because deleting the open thread used to go
    /// through it: the file was removed, then the still-open copy was saved
    /// straight back, and the row reappeared in the sidebar under a toast
    /// saying it had been deleted.
    fn start_fresh_thread(&self, slug: &str) {
        let Some(store) = self.store() else {
            return;
        };
        self.enter_project(slug);
        if let Ok(thread) = store.new_thread(slug) {
            self.imp().thread.replace(thread);
        }
        self.show_thread();
        self.refresh_threads();
        if let Some(window) = self.window() {
            window.composer().focus_entry();
        }
    }

    fn rename_thread(&self, slug: &str, id: &str) {
        let (Some(window), Some(store), Some(id)) =
            (self.window(), self.store(), ThreadId::from_stem(id))
        else {
            return;
        };
        let Ok(thread) = store.load_thread(slug, &id) else {
            return;
        };

        let slug = slug.to_string();
        dialogs::ask_name(
            &window,
            "Rename Chat",
            "Rename",
            &thread.display_title(),
            clone!(
                #[weak(rename_to = app)]
                self,
                move |name: String| {
                    let Some(store) = app.store() else { return };
                    let Ok(mut thread) = store.load_thread(&slug, &id) else {
                        return;
                    };
                    thread.title = Some(name);
                    let _ = store.save_thread(&slug, &thread);
                    // The open thread is a copy, so it is retitled too or the
                    // header would keep the old name until it is reopened.
                    if app.imp().thread.borrow().id == id {
                        app.imp().thread.borrow_mut().title = thread.title.clone();
                        if let Some(window) = app.window() {
                            window.set_thread_title(&thread.display_title());
                        }
                    }
                    app.refresh_threads();
                }
            ),
        );
    }

    /// Delete a chat, and offer it back.
    ///
    /// Undo rather than a confirmation: the whole chat is held in memory
    /// until the toast goes away, so putting it back is writing the same file
    /// again. A dialog before every delete would be the wrong trade for
    /// something this cheap to reverse.
    fn delete_thread(&self, slug: &str, id: &str) {
        let (Some(window), Some(store), Some(id)) =
            (self.window(), self.store(), ThreadId::from_stem(id))
        else {
            return;
        };
        let Ok(deleted) = store.load_thread(slug, &id) else {
            return;
        };
        if store.delete_thread(slug, &id).is_err() {
            window.toast("Could not delete that chat");
            return;
        }

        // Deleting what is open leaves a fresh chat in its place rather than
        // an empty screen with no way back — and emphatically does not save the
        // open copy on the way, which would put the file back.
        if self.imp().thread.borrow().id == id {
            self.start_fresh_thread(slug);
        } else {
            self.refresh_threads();
        }

        let toast = adw::Toast::new(&format!("Deleted “{}”", deleted.display_title()));
        toast.set_button_label(Some("Undo"));
        let slug = slug.to_string();
        toast.connect_button_clicked(clone!(
            #[weak(rename_to = app)]
            self,
            move |_| {
                if let Some(store) = app.store() {
                    let _ = store.save_thread(&slug, &deleted);
                }
                app.refresh_threads();
            }
        ));
        window.present_toast(&toast);
    }

    // -- projects --------------------------------------------------------------

    /// What the sidebar's row menus ask for. One entry point rather than three
    /// signals, because the window's only job with any of them is to pass it on.
    fn project_action(&self, action: &str, slug: &str) {
        match action {
            "open" => self.open_project(slug),
            "edit" => self.edit_project(slug),
            "folder" => self.choose_folder(slug),
            "delete" => self.delete_project(slug),
            _ => {}
        }
    }

    /// Show a project's page, in place of the conversation.
    ///
    /// The open chat is written out first and left open underneath: coming back
    /// from a project page to the conversation you were in is one click on the
    /// chat, and losing your place because you looked at a folder would be a
    /// poor trade.
    fn open_project(&self, slug: &str) {
        self.save_thread();
        self.enter_project(slug);
        self.render_project_page(slug);
    }

    /// Draw a project's page and put it on screen.
    fn render_project_page(&self, slug: &str) {
        let (Some(window), Some(store), Some(project)) =
            (self.window(), self.store(), self.project(slug))
        else {
            return;
        };
        let chats = store.threads(slug).unwrap_or_default();
        let scheduled: Vec<dialogs::Scheduled> = self
            .scheduled()
            .into_iter()
            .filter(|entry| entry.slug == slug)
            .collect();
        // The menu first — it decides whether "Delete Project…" is there at all
        // — and then the page, which has the last word on the title bar.
        self.show_project();
        window.show_project_page(&project, &chats, &scheduled);
        // The highlight goes on the project, not on whichever chat was open
        // behind it — the page is where you are.
        window.select_thread(slug, None);
    }

    /// The project `slug` names, from what was read at the last refresh.
    fn project(&self, slug: &str) -> Option<Project> {
        let imp = self.imp();
        if imp.project.borrow().slug == slug {
            return Some(imp.project.borrow().clone());
        }
        let found = imp
            .projects
            .borrow()
            .iter()
            .find(|project| project.slug == slug)
            .cloned();
        found.or_else(|| self.store()?.load_project(slug).ok())
    }

    /// Make `slug` the project new work belongs to.
    fn enter_project(&self, slug: &str) {
        let imp = self.imp();
        if imp.project.borrow().slug == slug {
            return;
        }
        if let Some(project) = self.project(slug) {
            imp.project.replace(project);
        }
        self.refresh_status();
    }

    fn new_project(&self) {
        let Some(window) = self.window() else { return };
        dialogs::ask_name(
            &window,
            "New Project",
            "Create",
            "",
            clone!(
                #[weak(rename_to = app)]
                self,
                move |name: String| {
                    let Some(store) = app.store() else { return };
                    match store.create_project(&name) {
                        Ok(project) => {
                            app.refresh_threads();
                            // Land in it, and open its settings: a project with
                            // no folder and no instructions is an empty shell,
                            // and the moment somebody has just named one is the
                            // moment they know what it is for.
                            app.new_thread(&project.slug);
                            app.edit_project(&project.slug);
                        }
                        Err(error) => {
                            if let Some(window) = app.window() {
                                window.toast(&format!("Could not create that project: {error}"));
                            }
                        }
                    }
                }
            ),
        );
    }

    fn edit_project(&self, slug: &str) {
        let (Some(window), Some(project)) = (self.window(), self.project(slug)) else {
            return;
        };
        dialogs::edit_project(
            &window,
            &project,
            clone!(
                #[weak(rename_to = app)]
                self,
                move |edited: Project| app.save_project(edited)
            ),
        );
    }

    /// Choose the folder a project is about, without going through its
    /// settings — which is the one thing somebody wants from the sidebar and
    /// otherwise takes a dialog and two clicks to reach.
    fn choose_folder(&self, slug: &str) {
        let (Some(window), Some(project)) = (self.window(), self.project(slug)) else {
            return;
        };
        let picker = gtk::FileDialog::builder().title("Choose a Folder").build();
        picker.select_folder(
            Some(&window),
            gio::Cancellable::NONE,
            clone!(
                #[weak(rename_to = app)]
                self,
                move |result| {
                    let Ok(Some(path)) = result.map(|folder| folder.path()) else {
                        return;
                    };
                    let mut edited = project.clone();
                    edited.workspace = Some(path);
                    // Choosing a folder is what switching the tools on means.
                    // Choosing one and finding the assistant still cannot read
                    // it would be a switch nobody knew to look for.
                    edited.tools.workspace = true;
                    app.save_project(edited);
                }
            ),
        );
    }

    /// Write a project back, and put what changed on screen.
    fn save_project(&self, project: Project) {
        let Some(store) = self.store() else { return };
        if let Err(error) = store.save_project(&project) {
            if let Some(window) = self.window() {
                window.toast(&format!("Could not save that project: {error}"));
            }
            return;
        }
        // A folder that was not there a moment ago is the one change worth
        // showing rather than announcing: the files appear, open, under the
        // project they were just attached to.
        let arrived = self
            .project(&project.slug)
            .is_some_and(|before| before.workspace != project.workspace)
            && project.workspace.is_some();

        let slug = project.slug.clone();
        if self.imp().project.borrow().slug == project.slug {
            self.imp().project.replace(project);
        }
        self.refresh_threads();
        self.show_project();
        self.refresh_status();
        // A folder that was not there a moment ago is worth showing rather than
        // announcing, and the page is where it shows.
        if arrived {
            self.open_project(&slug);
        }
    }

    /// Deleting a project takes its chats with it, and there is no single file
    /// to put back — so this one asks first.
    fn delete_project(&self, slug: &str) {
        let (Some(window), Some(project)) = (self.window(), self.project(slug)) else {
            return;
        };
        if project.is_default() {
            window.toast("Chats cannot be deleted");
            return;
        }
        let Some(store) = self.store() else { return };
        let threads = store.threads(&project.slug).map(|t| t.len()).unwrap_or(0);
        let body = match threads {
            0 => format!("“{}” has no chats.", project.name),
            1 => format!("“{}” and its one chat will be deleted.", project.name),
            n => format!("“{}” and its {n} chats will be deleted.", project.name),
        };
        // The folder is the user's and stays where it is. Deleting a project
        // that took a folder of real work with it would be unforgivable, and
        // the dialog says so rather than leaving anyone to wonder.
        let body = match project.workspace.as_ref() {
            Some(folder) => format!(
                "{body} The folder {} is not touched.",
                crate::ui::home_relative(folder)
            ),
            None => body,
        };

        let slug = project.slug.clone();
        dialogs::confirm_destructive(
            &window,
            "Delete Project?",
            &body,
            "Delete",
            clone!(
                #[weak(rename_to = app)]
                self,
                move || {
                    let Some(store) = app.store() else { return };
                    if let Err(error) = store.delete_project(&slug) {
                        if let Some(window) = app.window() {
                            window.toast(&format!("Could not delete that project: {error}"));
                        }
                        return;
                    }
                    app.enter_project(DEFAULT_PROJECT);
                    app.refresh_threads();
                    app.open_latest_thread();
                }
            ),
        );
    }

    // -- files -----------------------------------------------------------------

    /// What the sidebar's file rows ask for.
    ///
    /// Every one of these is checked against the project's folder first. The
    /// paths come from a listing this application produced, so they are inside
    /// it by construction — and "by construction" is exactly the argument that
    /// stops being true the first time somebody changes how the listing is
    /// made, which is why it is checked rather than argued.
    fn file_action(&self, action: &str, slug: &str, path: &Path) {
        let Some(window) = self.window() else { return };
        let Some(root) = self
            .project(slug)
            .and_then(|project| project.workspace)
            .filter(|root| root.is_dir())
        else {
            return;
        };
        if !path.starts_with(&root) {
            window.toast("That file is outside the project folder");
            return;
        }

        let file = gio::File::for_path(path);
        match action {
            // The system handler, so a spreadsheet opens in a spreadsheet
            // application. This is not the assistant reading the file; it is
            // the user opening their own file.
            "open" => {
                gtk::FileLauncher::new(Some(&file)).launch(
                    Some(&window),
                    gio::Cancellable::NONE,
                    |_| {},
                );
            }
            "reveal" => {
                gtk::FileLauncher::new(Some(&file)).open_containing_folder(
                    Some(&window),
                    gio::Cancellable::NONE,
                    |_| {},
                );
            }
            "new-folder" => self.new_folder(&window, path),
            "rename" => self.rename_file(&window, path),
            "trash" => self.trash_file(&window, &file, path),
            _ => {}
        }
    }

    fn new_folder(&self, window: &Window, parent: &Path) {
        let parent = parent.to_path_buf();
        dialogs::ask_name(
            window,
            "New Folder",
            "Create",
            "",
            clone!(
                #[weak(rename_to = app)]
                self,
                move |name: String| {
                    let Some(name) = one_name(&name) else {
                        if let Some(window) = app.window() {
                            window.toast("A folder name cannot contain a slash");
                        }
                        return;
                    };
                    match std::fs::create_dir(parent.join(&name)) {
                        Ok(()) => app.refresh_threads(),
                        Err(error) => {
                            if let Some(window) = app.window() {
                                window.toast(&format!("Could not make that folder: {error}"));
                            }
                        }
                    }
                }
            ),
        );
    }

    fn rename_file(&self, window: &Window, path: &Path) {
        let current = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let path = path.to_path_buf();
        dialogs::ask_name(
            window,
            "Rename",
            "Rename",
            &current,
            clone!(
                #[weak(rename_to = app)]
                self,
                move |name: String| {
                    let Some(name) = one_name(&name) else {
                        if let Some(window) = app.window() {
                            window.toast("A name cannot contain a slash");
                        }
                        return;
                    };
                    let Some(parent) = path.parent() else { return };
                    let to = parent.join(&name);
                    // Renaming onto something that is already there would
                    // silently replace it.
                    if to.exists() {
                        if let Some(window) = app.window() {
                            window.toast(&format!("“{name}” is already there"));
                        }
                        return;
                    }
                    match std::fs::rename(&path, &to) {
                        Ok(()) => app.refresh_threads(),
                        Err(error) => {
                            if let Some(window) = app.window() {
                                window.toast(&format!("Could not rename that: {error}"));
                            }
                        }
                    }
                }
            ),
        );
    }

    /// The trash, not `unlink`. It is the user's file, they can put it back,
    /// and this application is not the place to be sure they meant it.
    fn trash_file(&self, window: &Window, file: &gio::File, path: &Path) {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        match file.trash(gio::Cancellable::NONE) {
            Ok(()) => {
                window.toast(&format!("Moved “{name}” to the trash"));
                self.refresh_threads();
            }
            Err(error) => window.toast(&format!("Could not move that to the trash: {error}")),
        }
    }

    /// Draw the open thread from scratch. Replay goes through the same fold as
    /// the live stream, so a reopened conversation cannot look different.
    fn show_thread(&self) {
        let Some(window) = self.window() else {
            return;
        };
        let conversation = window.conversation();
        conversation.clear();

        let show_thinking = self.imp().settings.borrow().show_thinking;
        let thread = self.imp().thread.borrow();
        for turn in thread.turns() {
            let view = TurnView::replayed_with(turn, show_thinking, &self.images_of(turn));
            conversation.append(view.widget());
        }
        window.set_thread_title(&thread.display_title());
        drop(thread);
        // Drawing a chat is what leaves a project page: the conversation is
        // the thing being asked for.
        window.show_chat();
        self.show_project();
        self.refresh_ambient();
        self.refresh_status();
        // A workflow belongs to one chat, so switching chats has to redraw the
        // strip — including drawing nothing, which is what leaving a chat that
        // had one looks like.
        self.refresh_workflow();
    }

    /// Tell the window which project it is in. Its name goes under the chat's
    /// title, and the menu that acts on it says the right words — there is no
    /// project to delete when the answer is "these are just chats".
    fn show_project(&self) {
        let Some(window) = self.window() else { return };
        let project = self.imp().project.borrow();
        window.set_project(&project.name, project.is_default());
    }

    fn save_thread(&self) {
        let (Some(store), Some(window)) = (self.store(), self.window()) else {
            return;
        };
        let slug = self.imp().project.borrow().slug.clone();
        let thread = self.imp().thread.borrow();
        if let Err(error) = store.save_thread(&slug, &thread) {
            // Losing what was said is worth a banner, not a toast: a toast is
            // missed while typing and the cost of missing it is the
            // conversation.
            window.set_trouble(Some(&format!("This chat is not being saved: {error}")));
        }
    }

    // -- the heartbeat --------------------------------------------------------

    /// Start the minute tick that wakes scheduled threads.
    ///
    /// A poll rather than a timer set for the deadline, and the reason is that
    /// `glib::timeout_add_seconds` is measured in **monotonic** time, which on
    /// Linux does not advance while the machine is suspended. A timer set for
    /// 07:00 silently drifts by however long the laptop slept. So the tick is
    /// short and fixed, and every tick asks the *wall* clock whether anything
    /// is due — the same shape Pika Backup uses for the same reason.
    fn start_heartbeat(&self) {
        glib::timeout_add_seconds_local(
            HEARTBEAT_TICK,
            clone!(
                #[weak(rename_to = app)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    app.tick();
                    glib::ControlFlow::Continue
                }
            ),
        );
    }

    /// One tick: is anything due, and can it run right now?
    ///
    /// Only the open thread is woken. A schedule on a thread that is not open
    /// is noticed the next time it is opened — running a turn against a thread
    /// that is not loaded would mean a second conversation pipeline, and the
    /// whole point of putting the schedule on the thread was to avoid that.
    fn tick(&self) {
        // Never mid-answer. A scheduled run fires *between* turns, so it never
        // interleaves with something the user is watching — and the next tick
        // is a minute away, well inside the grace window.
        if self.imp().in_flight.borrow().is_some() {
            return;
        }
        // Nothing is in flight, which is also the only time consolidation may
        // start: it is many requests against the same server the conversation
        // uses, and a person waiting on an answer must not queue behind it.
        self.dream_if_due();
        self.look_out_if_due();
        let now = chrono::Local::now();
        let Some((due, prompt)) =
            self.imp()
                .thread
                .borrow()
                .heartbeat
                .as_ref()
                .and_then(|heartbeat| {
                    heartbeat
                        .due(now)
                        .map(|due| (due, heartbeat.prompt.clone()))
                })
        else {
            return;
        };
        if prompt.trim().is_empty() {
            return;
        }
        // The server being down is a reason to wait for the next occurrence,
        // not to submit a turn that will fail.
        if self.imp().client.borrow().is_none() {
            return;
        }

        // Recorded before the turn rather than after: if the answer never
        // arrives, the schedule must still have moved on, or every tick for
        // the next twenty minutes tries again.
        let scheduled = {
            let mut thread = self.imp().thread.borrow_mut();
            let Some(heartbeat) = thread.heartbeat.as_mut() else {
                return;
            };
            let scheduled = heartbeat
                .last_run
                .map(|last| {
                    heartbeat
                        .schedule
                        .next_after(last.with_timezone(&chrono::Local))
                })
                .unwrap_or(now);
            heartbeat.last_run = Some(chrono::Utc::now());
            scheduled
        };
        self.save_thread();

        let preamble = crate::model::heartbeat::preamble(due, scheduled);
        self.submit_turn(&format!("{preamble}\n\n{prompt}"), true);
    }

    // -- the lookout ----------------------------------------------------------

    /// Look over the day, and say something only if it is worth saying.
    ///
    /// Unlike the heartbeat this is not a turn: no tools, no thread, no
    /// conversation. Signals are gathered *here*, by the application, and one
    /// call decides whether any of it is worth a notification — because a
    /// proactive check that could run tools is one that spends money and
    /// changes files while nobody is watching.
    ///
    /// Most of the time it says nothing and nothing happens, which is the
    /// design rather than a disappointment. See `model::lookout`.
    fn look_out_if_due(&self) {
        let (enabled, hours, last) = {
            let settings = self.imp().settings.borrow();
            (
                settings.lookout,
                settings.lookout_hours.max(1),
                settings.last_lookout,
            )
        };
        if !enabled {
            return;
        }
        let now = chrono::Utc::now();
        if let Some(last) = last {
            if now - last < chrono::Duration::hours(i64::from(hours)) {
                return;
            }
        }
        // Stamped before anything runs. A check that fails should not retry
        // every minute for the rest of the day.
        {
            let imp = self.imp();
            let mut settings = imp.settings.borrow_mut();
            settings.last_lookout = Some(now);
            let _ = settings.save(&imp.settings_path.borrow());
        }

        let app = self.clone();
        self.gather_signals(move |signals| {
            if !signals.worth_asking() {
                return;
            }
            let request = crate::model::lookout::request(&signals, chrono::Local::now());
            let stream = Rc::new(RefCell::new(crate::model::turn::TurnStream::new()));
            let cancellable = app.imp().client.borrow().as_ref().map(|client| {
                client.stream(
                    &request,
                    {
                        let stream = stream.clone();
                        move |text: &str| {
                            stream.borrow_mut().push(text);
                        }
                    },
                    {
                        let app = app.clone();
                        let stream = stream.clone();
                        move |outcome| {
                            if outcome.is_err() {
                                return;
                            }
                            let mut borrowed = stream.borrow_mut();
                            borrowed.end();
                            let state = std::mem::take(&mut *borrowed).finish();
                            drop(borrowed);
                            if let crate::model::lookout::Outcome::Notice { headline, detail } =
                                crate::model::lookout::read(&state.answer)
                            {
                                app.announce_notice(&headline, &detail);
                            }
                        }
                    },
                )
            });
            if let Some(cancellable) = cancellable {
                std::mem::forget(cancellable);
            }
        });
    }

    /// What can be found out cheaply, without asking anybody.
    ///
    /// Planner first, then mail, because each is a subprocess or a socket and
    /// they chain the way every other multi-step read here does. Anything not
    /// configured contributes nothing rather than failing the check.
    fn gather_signals<F>(&self, done: F)
    where
        F: FnOnce(crate::model::lookout::Signals) + 'static,
    {
        let mut signals = crate::model::lookout::Signals {
            context: self
                .imp()
                .memory
                .borrow()
                .as_ref()
                .and_then(|memory| memory.ambient(chrono::Utc::now()))
                .map(|block| {
                    block
                        .lines()
                        .filter(|line| line.starts_with("- "))
                        .take(6)
                        .map(|line| line.trim_start_matches("- ").to_string())
                        .collect()
                })
                .unwrap_or_default(),
            ..crate::model::lookout::Signals::default()
        };

        let runner = Runner::new(self.imp().memory.clone(), None)
            .with_mail(self.mail_account_from_settings());
        let mail_account = self.mail_account_from_settings();

        // Planner's own idea of what is due, which is the signal with the most
        // in it and the cheapest to get.
        let planner = crate::model::planner::command(&[
            "list".to_string(),
            "due: today | overdue".to_string(),
        ]);
        let _ = runner;
        // The weather, which the lookout's rubric has always talked about and
        // which nothing was ever putting in front of it. `Signals.alerts` was
        // filled in by the eval suite and by no other caller, so the whole
        // weather half of the proactive check was dead in the application while
        // scoring 25% in the harness.
        let at = self.imp().config.borrow().weather_point();

        crate::ui::runner::run_for_signals(planner, move |listed| {
            if let Some(text) = listed {
                signals.tasks = crate::model::lookout::tasks_in(&text);
            }
            let with_mail =
                move |mut signals: crate::model::lookout::Signals,
                      done: Box<dyn FnOnce(crate::model::lookout::Signals)>| {
                    let Some(account) = mail_account else {
                        done(signals);
                        return;
                    };
                    crate::ui::mail::run(
                        &account,
                        &["search".to_string(), "unread".to_string()],
                        move |found| {
                            if let Ok(text) = found {
                                signals.mail = crate::model::lookout::subjects_in(&text);
                            }
                            done(signals);
                        },
                    );
                };

            let Some(at) = at else {
                with_mail(signals, Box::new(done));
                return;
            };
            crate::ui::runner::alerts_for_signals(at, move |alerts| {
                signals.alerts = alerts;
                with_mail(signals, Box::new(done));
            });
        });
    }

    /// The mail account as the settings hold it, whatever context is open.
    ///
    /// The lookout is an application-level job and has no context; it reads the
    /// account directly rather than through the open conversation's tool set.
    fn mail_account_from_settings(&self) -> Option<crate::ui::mail::Account> {
        let settings = self.imp().settings.borrow();
        let account = settings.mail.as_ref()?;
        Some(crate::ui::mail::Account {
            host: account.host.clone(),
            port: account.port,
            user: account.user.clone(),
            password: account.password.clone(),
            tls: account.tls,
            from: account.from.clone(),
            smtp_host: account.smtp_host.clone(),
            smtp_port: account.smtp_port,
        })
    }

    /// A notice from the lookout, as a notification.
    ///
    /// One id, so a later notice replaces an earlier one rather than stacking a
    /// day of them in the shade.
    fn announce_notice(&self, headline: &str, detail: &str) {
        let notification = gio::Notification::new(headline);
        notification.set_body(Some(detail));
        notification.set_priority(gio::NotificationPriority::Normal);
        self.send_notification(Some("lookout"), &notification);
    }

    /// Tell the user a scheduled run finished, whether or not the window is up.
    ///
    /// A stable id per thread, so tomorrow's briefing *replaces* today's in the
    /// shade rather than stacking a week of them — and a default action that
    /// opens the thread it ran in, which works even if the app has since
    /// exited, because the desktop file is `DBusActivatable`.
    fn announce_run(&self, title: &str, summary: &str) {
        let notification = gio::Notification::new(title);
        notification.set_body(Some(summary));
        notification.set_priority(gio::NotificationPriority::Normal);

        let id = self.imp().thread.borrow().id.to_string();
        notification.set_default_action_and_target_value("app.show-thread", Some(&id.to_variant()));
        self.send_notification(Some(&format!("heartbeat:{id}")), &notification);
    }

    // -- a turn ---------------------------------------------------------------

    fn submit(&self, text: &str) {
        self.submit_turn(text, false);
    }

    fn submit_turn(&self, text: &str, scheduled: bool) {
        let text = text.trim().to_string();
        if self.imp().in_flight.borrow().is_some() {
            // The composer refuses to send while a turn streams, and its button
            // says why by having become Stop. Everything else that asks a
            // question — the explain shortcut, a schedule coming due — has no
            // such control to look at, so it is told.
            if let (false, Some(window)) = (scheduled, self.window()) {
                window.toast("Wait for this answer to finish");
            }
            return;
        }
        let Some(window) = self.window() else { return };

        // Taken now, not when the request is built: the staging area empties
        // as the question is sent, and a turn owns what it was asked with.
        let staging = window.composer().staging();
        let attached = staging.take();
        let documents = staging.take_documents();
        if text.is_empty() && attached.is_empty() && documents.is_empty() {
            return;
        }

        let view = TurnView::new(&text, self.imp().settings.borrow().show_thinking);
        view.set_images(&textures(&attached));
        window.conversation().append(view.widget());
        window.composer().set_busy(true);
        window.set_trouble(None);

        self.imp().in_flight.replace(Some(InFlight {
            question: text,
            documents,
            images: attached,
            stream: TurnStream::new(),
            view,
            cancellable: gio::Cancellable::new(),
            answer: String::new(),
            thinking: String::new(),
            calls: Vec::new(),
            rounds: 0,
            retried: false,
            floor: false,
            fold: None,
            shrinks: 0,
            exchanges: Vec::new(),
            approved_something: false,
            scheduled,
        }));
        self.ask(None);
    }

    /// Send the request and stream the reply.
    ///
    /// Called once when you press Enter, and once more after every round of
    /// tools — the model gets its results and keeps going, which is what makes
    /// this an agentic loop rather than a single shot.
    fn ask(&self, tool_results: Option<Vec<Message>>) {
        let Some(client) = self.imp().client.borrow().clone() else {
            return;
        };
        let question = match self.imp().in_flight.borrow().as_ref() {
            Some(turn) => turn.question.clone(),
            None => return,
        };

        let request = self.build_request(&question, tool_results.unwrap_or_default());

        // A fresh fold per round: each request is its own stream, and the
        // pieces are gathered onto the turn as rounds complete.
        if let Some(turn) = self.imp().in_flight.borrow_mut().as_mut() {
            turn.stream = TurnStream::new();
        }

        let cancellable = client.stream(
            &request,
            clone!(
                #[weak(rename_to = app)]
                self,
                move |text: &str| app.on_text(text)
            ),
            clone!(
                #[weak(rename_to = app)]
                self,
                move |outcome| app.on_finished(outcome)
            ),
        );
        if let Some(turn) = self.imp().in_flight.borrow_mut().as_mut() {
            turn.cancellable = cancellable;
        }
    }

    fn on_text(&self, text: &str) {
        let Some(window) = self.window() else {
            return;
        };
        let mut in_flight = self.imp().in_flight.borrow_mut();
        let Some(turn) = in_flight.as_mut() else {
            return;
        };
        let events = turn.stream.push(text);
        let view = turn.view.clone();
        let calls = turn.stream.state().tool_calls.clone();
        let settled = turn.calls.clone();
        // The borrow is dropped before the widgets are touched: a handler that
        // re-enters the application would otherwise find the RefCell held.
        drop(in_flight);

        for event in &events {
            view.apply(event);
        }
        if events
            .iter()
            .any(|event| matches!(event, Event::ToolCall(_)))
        {
            self.draw_chips(&settled, &calls);
        }
        if !events.is_empty() {
            window.conversation().follow();
        }
    }

    fn on_finished(&self, outcome: Result<(), ClientError>) {
        let Some(window) = self.window() else {
            return;
        };
        let Some(mut turn) = self.imp().in_flight.borrow_mut().take() else {
            return;
        };

        // The context window overflowed. Recover in two steps, gentlest first.
        if let Err(ClientError::Http { body, .. }) = &outcome {
            if compaction::is_overflow(body) {
                // Step one: empty the oldest tool results of this turn. It
                // keeps every call and every pairing, so the model still reads
                // "these ran, here is what came back" and does not repeat a
                // write — which is why, unlike the floor, this is safe after an
                // approved tool. Each attempt keeps fewer.
                let keep = 2usize.saturating_sub(turn.shrinks);
                let shrunk = compaction::shrink_tool_results(&mut turn.exchanges, keep);
                if let compaction::Compacted::Shrunk { results, .. } = shrunk {
                    turn.shrinks += 1;
                    turn.stream = TurnStream::new();
                    let chain = turn.exchanges.clone();
                    self.imp().in_flight.replace(Some(turn));
                    if let Some(window) = self.window() {
                        window.toast(&format!(
                            "That did not fit. Dropped {results} earlier tool result(s) and \
                             trying again."
                        ));
                    }
                    self.announce(shrunk);
                    self.ask(Some(chain));
                    return;
                }

                // Step two: the floor. It drops the record that the calls
                // happened, so a turn that has already written something is
                // reported rather than retried — redoing a write is worse than
                // failing.
                if !turn.retried && !turn.approved_something {
                    turn.retried = true;
                    turn.floor = true;
                    turn.stream = TurnStream::new();
                    self.imp().in_flight.replace(Some(turn));
                    if let Some(window) = self.window() {
                        window.toast("That did not fit. Trying again with a shorter history.");
                    }
                    self.ask(None);
                    return;
                }
            }
        }

        let cancelled = matches!(outcome, Err(ClientError::Cancelled));
        match &outcome {
            Ok(()) => {
                turn.stream.end();
            }
            Err(ClientError::Cancelled) => {
                turn.stream.cancel();
            }
            Err(error) => {
                turn.view.set_failure(Some(&error.to_string()));
                if matches!(error, ClientError::Unreachable(_)) {
                    self.report_unreachable(&error.to_string());
                }
            }
        }

        let state = std::mem::take(&mut turn.stream).finish();

        // Gather this round onto the turn.
        if !state.thinking.is_empty() {
            turn.thinking.push_str(&state.thinking);
        }
        if !state.answer.is_empty() {
            if !turn.answer.is_empty() {
                turn.answer.push_str("\n\n");
            }
            turn.answer.push_str(&state.answer);
        }

        let wants_tools = !cancelled && outcome.is_ok() && !state.tool_calls.is_empty();
        if wants_tools && turn.rounds < MAX_TOOL_ROUNDS {
            turn.rounds += 1;
            let pending = state.tool_calls.clone();
            self.imp().in_flight.replace(Some(turn));
            self.run_tools(pending, state);
            return;
        }

        if wants_tools {
            // The model is looping. Say so rather than going round again.
            turn.view
                .set_failure(Some("Stopped after too many tool calls in one turn."));
        }

        self.settle_turn(turn, state, &window);
    }

    /// Run each call the model asked for, pausing at the gate when one needs
    /// approval, then hand the results back and continue the turn.
    fn run_tools(&self, calls: Vec<ToolCall>, state: TurnState) {
        let Some(window) = self.window() else { return };
        let gated = calls
            .iter()
            .find(|call| tools::gate_for(&call.name, &call.arguments) == tools::Gate::Always)
            .cloned();

        let Some(gated) = gated else {
            self.run_all(calls, state);
            return;
        };

        // One dialog at a time: the rest of the round waits behind it.
        approval::ask(
            &window,
            &gated.name,
            &gated.arguments,
            clone!(
                #[weak(rename_to = app)]
                self,
                #[strong]
                calls,
                #[strong]
                state,
                move |decision: Decision| {
                    // Stopping while the dialog was open means stop: the turn
                    // is over and nothing else runs.
                    let cancelled = app
                        .imp()
                        .in_flight
                        .borrow()
                        .as_ref()
                        .map_or(true, |turn| turn.cancellable.is_cancelled());
                    if cancelled {
                        app.abandon();
                        return;
                    }

                    if decision == Decision::Approve {
                        if let Some(turn) = app.imp().in_flight.borrow_mut().as_mut() {
                            turn.approved_something = true;
                        }
                    }

                    let mut settled = Vec::new();
                    for call in &calls {
                        let mut call = call.clone();
                        if call.id == gated.id && decision == Decision::Deny {
                            call.outcome = Some(ToolOutcome::Denied);
                        }
                        settled.push(call);
                    }
                    app.run_all(settled, state.clone());
                }
            ),
        );
    }

    /// End a turn that was stopped while something else was in front of it.
    fn abandon(&self) {
        let Some(window) = self.window() else { return };
        let Some(turn) = self.imp().in_flight.borrow_mut().take() else {
            return;
        };
        let state = TurnState {
            thinking: turn.thinking.clone(),
            answer: turn.answer.clone(),
            tool_calls: turn.calls.clone(),
            finish: Some(crate::model::turn::Finish::Cancelled),
            ..Default::default()
        };
        turn.view.settle(&state);
        window.composer().set_busy(false);
        self.record(&turn.question, &state, &turn.images);
    }

    /// Run the calls one after another, then continue the turn.
    ///
    /// Sequential rather than parallel: a search and a note write in flight
    /// together would interleave their results into the transcript, and the
    /// model reads them in order. Each call's completion starts the next, the
    /// way the transport's read loop does.
    fn run_all(&self, calls: Vec<ToolCall>, state: TurnState) {
        self.run_next(calls, Vec::new(), state);
    }

    fn run_next(&self, mut pending: Vec<ToolCall>, mut ran: Vec<ToolCall>, state: TurnState) {
        if pending.is_empty() {
            self.finish_round(ran, state);
            return;
        }
        let mut call = pending.remove(0);

        // Already decided — denied at the gate — so it does not run.
        if call.outcome.is_some() {
            ran.push(call);
            self.run_next(pending, ran, state);
            return;
        }

        // Decided here for the same reason: the turn has spent its searches, so
        // this one comes back without going out. Counted across the whole turn,
        // not the round, because the spiral is a dozen searches over seven
        // rounds and a per-round limit would not see it.
        //
        // `is_a_lookup` is wider than `is_a_search` on purpose. Only searches
        // *spend* the budget, but once it is gone a `fetch_url` is the same
        // hunt by another route, and refusing it is what turns "that tool is
        // closed" into "looking things up is over".
        if is_a_lookup(&call.name) {
            let spent = self.searches_this_turn(&ran);
            if !Budget::allows(spent) {
                call.outcome = Some(ToolOutcome::Ok(Budget::refuse(spent)));
                ran.push(call);
                self.run_next(pending, ran, state);
                return;
            }
        }

        let runner = Runner::new(
            self.imp().memory.clone(),
            crate::ui::runner::exa_key(self.imp().config.borrow().exa_api_key.as_deref()),
        )
        .with_embeddings(self.imp().embeddings.borrow().clone())
        .with_workspace(self.workspace())
        .with_sandbox(self.sandbox())
        .with_escalation(self.escalation())
        .with_mail(self.mail_account())
        .with_switch({
            let app = self.clone();
            Rc::new(move |names: &[String]| app.switch_capabilities_on(names))
        })
        .with_scheduler({
            let app = self.clone();
            Rc::new(move |action: &str, when: &str, prompt: &str, title: &str| {
                app.set_schedule(action, when, prompt, title)
            })
        })
        .with_workflows({
            let app = self.clone();
            Rc::new(move |arguments: &str| app.keep_workflow(arguments))
        })
        .with_weather(self.imp().config.borrow().weather_point())
        .with_progress(Rc::new({
            // Redrawn from the same two lists the first draw used, so a
            // progress line replaces the argument on the one running chip and
            // leaves every settled one alone.
            let app = self.clone();
            let settled = ran.clone();
            let running = call.clone();
            move |line: &str| {
                app.imp().progress.replace(Some(line.to_string()));
                app.draw_chips(&settled, std::slice::from_ref(&running));
            }
        }));
        // The chips show it running before it has an answer, which for a web
        // search is the difference between "working" and "frozen".
        self.imp().progress.replace(None);
        self.draw_chips(&ran, std::slice::from_ref(&call));

        let app = self.clone();
        runner.run(&call.clone(), move |outcome| {
            // Cleared before the next call is drawn, or a fast tool behind a
            // slow one would inherit the transcript's last percentage.
            app.imp().progress.replace(None);
            call.outcome = Some(outcome);
            ran.push(call);
            app.run_next(pending, ran, state);
        });
    }

    /// How many searches this turn has run: the rounds already settled, plus
    /// the ones decided so far in this one.
    ///
    /// A refused call is not counted — it never went out, and counting it would
    /// mean the number in the refusal climbed while nothing was searched.
    fn searches_this_turn(&self, ran: &[ToolCall]) -> usize {
        let settled = self
            .imp()
            .in_flight
            .borrow()
            .as_ref()
            .map(|turn| turn.calls.iter().filter(|c| is_a_search(&c.name)).count())
            .unwrap_or(0);
        let this_round = ran
            .iter()
            .filter(|c| is_a_search(&c.name) && !refused_for_budget(c))
            .count();
        settled + this_round
    }

    /// Every call in the round has answered: hand the results back.
    fn finish_round(&self, ran: Vec<ToolCall>, state: TurnState) {
        let mut results = Vec::new();
        for call in &ran {
            let text = match call.outcome.as_ref() {
                Some(ToolOutcome::Ok(result)) => result.clone(),
                Some(ToolOutcome::Failed(error)) => format!("Error: {error}"),
                Some(ToolOutcome::Denied) => "The user declined to run this tool.".to_string(),
                None => String::new(),
            };
            results.push(Message::tool_result(call.id.clone(), text));
        }

        // The assistant message that asked for them has to precede the results.
        let invocations: Vec<ToolInvocation> = ran
            .iter()
            .map(|call| {
                ToolInvocation::new(call.id.clone(), call.name.clone(), call.arguments.clone())
            })
            .collect();
        let mut messages = vec![Message {
            role: crate::model::wire::Role::Assistant,
            content: (!state.answer.is_empty())
                .then(|| crate::model::wire::Content::Text(state.answer.clone())),
            // The reasoning that led to these calls, so the model can see why
            // it asked for them when it reads the results.
            reasoning_content: (!state.thinking.is_empty()).then(|| state.thinking.clone()),
            tool_calls: invocations,
            tool_call_id: None,
        }];
        messages.extend(results);

        let (settled, chain) = if let Some(turn) = self.imp().in_flight.borrow_mut().as_mut() {
            turn.calls.extend(ran.clone());
            // These results came from the last round the turn is allowed, so
            // the next response cannot call anything. Say so, rather than
            // letting the model ask again and be cut off with nothing written.
            if turn.rounds >= MAX_TOOL_ROUNDS {
                messages.push(Message::user(LAST_ROUND.to_string()));
            } else if turn.calls.len() >= WRAP_UP_AFTER && turn.answer.trim().is_empty() {
                // Not a ceiling — a nudge, and only while the turn still has
                // nothing written. A long chain that is actually producing
                // something is not what this is for.
                messages.push(Message::user(WRAP_UP.to_string()));
            }
            // The chain, not just this round: everything the turn has done so
            // far goes back, or the model cannot build on it.
            turn.exchanges.extend(messages);
            (turn.calls.clone(), turn.exchanges.clone())
        } else {
            (ran.clone(), messages)
        };
        self.draw_chips(&settled, &[]);

        self.ask(Some(chain));
    }

    /// The turn is over: draw it settled, keep it, and let the composer go.
    fn settle_turn(&self, turn: InFlight, state: TurnState, window: &Window) {
        let settled = TurnState {
            thinking: turn.thinking.clone(),
            answer: turn.answer.clone(),
            tool_calls: turn.calls.clone(),
            ..state
        };
        let announce = turn.scheduled;
        // The model thought and then said nothing, and the fold could not find
        // a call in the thinking to rescue. An empty bubble is the one outcome
        // a person can read nothing into — so say what happened. See
        // `turn::recover_tool_calls` for what this is usually a symptom of.
        if settled.answer.trim().is_empty()
            && settled.tool_calls.is_empty()
            && !settled.thinking.trim().is_empty()
        {
            turn.view.set_failure(Some(
                "The model finished without answering — it reasoned, but its reply did not \
                 survive the server's parsing. Asking again usually works.",
            ));
        }
        turn.view.settle(&settled);
        self.draw_chips(&turn.calls, &[]);
        window.composer().set_busy(false);
        window.conversation().follow();
        self.record(&turn.question, &settled, &turn.images);
        self.fold_if_needed();
        // Not on a scheduled run: the "question" there is a standing prompt the
        // clock submitted, and nobody stated anything. Not on a turn that
        // produced nothing either — there is no exchange to read.
        if !announce && !settled.answer.trim().is_empty() {
            self.harvest_turn(&turn.question, &settled.answer);
        }

        if announce {
            // The first line of the answer, which is what a notification body
            // has room for and what the model was asked to lead with.
            let summary = settled
                .answer
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("The scheduled run finished with nothing to report.")
                .chars()
                .take(200)
                .collect::<String>();
            let title = self.imp().thread.borrow().display_title();
            if let Some(heartbeat) = self.imp().thread.borrow_mut().heartbeat.as_mut() {
                heartbeat.last_outcome = Some(summary.clone());
            }
            self.save_thread();
            self.announce_run(&title, &summary);
        }
    }

    /// Where this project runs Python, if it has the sandbox switched on.
    ///
    /// One directory per project, beside its chats, for the reason projects
    /// exist at all: work in one should not turn up in another. A half-finished
    /// dataframe from the Planning project has no business being visible to a
    /// script in an unrelated chat.
    ///
    /// The folder goes in read-only when the project has one, which is what
    /// lets a script compute over the user's files without being able to change
    /// them — every write still goes through a tool the user approves.
    fn sandbox(&self) -> Option<crate::model::sandbox::Sandbox> {
        if !self.imp().project.borrow().tools.python {
            return None;
        }
        let store = self.store()?;
        let root = store
            .root()
            .join("projects")
            .join(&self.imp().project.borrow().slug)
            .join("sandbox");
        Some(
            crate::model::sandbox::Sandbox::new(root)
                .reading(self.workspace().map(|space| space.root().to_path_buf())),
        )
    }

    /// Switch capabilities on for the open project, and say what happened.
    ///
    /// This writes the project to disk, which is the point: a capability the
    /// conversation turned out to need is one the *next* conversation in this
    /// project probably needs too, and having to ask for it again every time
    /// would be a worse version of the menu it replaces. It is visible three
    /// ways — the chip in the transcript, a toast, and the switch in the
    /// project's settings now being on — because a capability that switches
    /// itself on invisibly is one nobody can switch back off.
    fn switch_capabilities_on(&self, names: &[String]) -> String {
        use crate::model::capability;

        let mut turned_on = Vec::new();
        let mut already = Vec::new();
        let mut unknown = Vec::new();
        let mut said_where: Option<std::path::PathBuf> = None;
        {
            let mut project = self.imp().project.borrow_mut();
            for name in names {
                let name = name.trim().to_lowercase();
                if capability::named(&name).is_none() {
                    unknown.push(name);
                    continue;
                }
                if capability::switch_on(&mut project.tools, &name) {
                    turned_on.push(name);
                } else {
                    already.push(name);
                }
            }
        }

        if turned_on.is_empty() && already.is_empty() {
            return format!(
                "There is no capability called {}. There is one for each of: {}.",
                unknown.join(", "),
                capability::ALL
                    .iter()
                    .map(|c| c.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        // A project that has just been given the file tools and has nowhere to
        // put anything gets somewhere. `~/Documents/Familiar` rather than a
        // directory under the application's own data: a spreadsheet somebody
        // asked for should land where they would look for a spreadsheet. It is
        // made only when a capability that needs it is switched on, it is named
        // after the application, and every write into it still asks first.
        if turned_on
            .iter()
            .any(|name| name == "workspace" || name == "documents")
        {
            let has_one = self
                .imp()
                .project
                .borrow()
                .workspace
                .as_ref()
                .is_some_and(|root| root.is_dir());
            if !has_one {
                if let Some(root) = Self::default_workspace() {
                    match std::fs::create_dir_all(&root) {
                        Ok(()) => {
                            self.imp().project.borrow_mut().workspace = Some(root.clone());
                            said_where = Some(root);
                        }
                        Err(error) => {
                            eprintln!("familiar: could not make a workspace: {error}");
                        }
                    }
                }
            }
        }

        if !turned_on.is_empty() {
            let project = self.imp().project.borrow().clone();
            if let Some(store) = self.store() {
                if let Err(error) = store.save_project(&project) {
                    // Worth saying and not worth stopping for: the capability is
                    // on for this conversation either way, and the only thing
                    // lost is that it will be off again next time.
                    eprintln!("familiar: could not save the project: {error}");
                }
            }
            self.refresh_threads();
            if let Some(window) = self.window() {
                window.toast(&format!("Switched on {}", turned_on.join(", ")));
            }
        }

        let mut said = capability::switched(&turned_on, &already);
        if let Some(root) = said_where {
            // The model has to be able to tell the user where the file went,
            // and this is the only moment it could know.
            said.push_str(&format!(
                " This conversation had no workspace folder, so one was made at {} — say so, \
                 and write there.",
                root.display()
            ));
        }
        if !unknown.is_empty() {
            said.push_str(&format!(
                " There is no capability called {}.",
                unknown.join(", ")
            ));
        }
        said
    }

    /// Where files go for a project that never had a folder chosen for it.
    ///
    /// `XDG_DOCUMENTS_DIR` when the desktop reports one, and `~/Documents`
    /// otherwise. Not a directory under the application's own data, because a
    /// document somebody asked for should be somewhere they would look for a
    /// document rather than somewhere they would look for a cache.
    fn default_workspace() -> Option<std::path::PathBuf> {
        gtk::glib::user_special_dir(gtk::glib::UserDirectory::Documents)
            .or_else(|| gtk::glib::home_dir().into())
            .map(|documents| documents.join("Familiar"))
    }

    /// Set, show or clear the open chat's schedule.
    ///
    /// The schedule lives on the thread, so this is the same write the
    /// Scheduled Chats window makes — the assistant is not getting a private
    /// mechanism, it is getting the one the menu already drives, which is why
    /// anything it sets up can be paused or removed there.
    fn set_schedule(
        &self,
        action: &str,
        when: &str,
        prompt: &str,
        title: &str,
    ) -> Result<String, String> {
        use crate::model::heartbeat;
        use crate::model::thread::Heartbeat;

        match action {
            "show" | "list" => {
                let thread = self.imp().thread.borrow();
                return Ok(match thread.heartbeat.as_ref() {
                    Some(beat) => format!(
                        "This chat runs {} and asks: {:?}{}",
                        beat.schedule.describe().to_lowercase(),
                        beat.prompt,
                        if beat.enabled { "" } else { " (paused)" }
                    ),
                    None => "This chat has no schedule.".to_string(),
                });
            }
            "clear" | "stop" | "remove" | "cancel" => {
                let had = self.imp().thread.borrow_mut().heartbeat.take();
                self.save_thread();
                self.refresh_threads();
                return Ok(match had {
                    Some(_) => "Stopped. This chat no longer runs on its own.".to_string(),
                    None => "This chat had no schedule, so nothing changed.".to_string(),
                });
            }
            _ => {}
        }

        let Some(schedule) = heartbeat::parse(when) else {
            return Err(format!(
                "{when:?} is not a schedule I can set. Use `daily at 07:00`, `weekdays at \
                 08:30`, `Mondays at 09:00` or `every 4 hours`."
            ));
        };
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(
                "a schedule needs a standing prompt — the instruction to run each time.".into(),
            );
        }

        // The clock starts now rather than at the epoch, or the first tick
        // would fire every occurrence since 1970.
        let mut beat = Heartbeat::new(schedule, prompt);
        beat.last_run = Some(chrono::Utc::now());
        let next = schedule.first_after(chrono::Local::now());
        let named;
        {
            let mut thread = self.imp().thread.borrow_mut();
            thread.heartbeat = Some(beat);
            // A scheduled chat is the one kind you go looking for weeks later,
            // by name, in a list — and its name would otherwise be the first
            // line of however the conversation happened to open: "could you
            // help me setup a morning brief with…". The model has just written
            // the standing prompt, so it knows what this chat is for better
            // than the first sentence does.
            //
            // Only when nothing has named it yet. A title the user typed under
            // Rename Chat is theirs, and a tool call must not quietly replace
            // it.
            named = crate::model::thread::tidy_title(title);
            let unnamed = thread
                .title
                .as_ref()
                .map_or(true, |had| had.trim().is_empty());
            if unnamed {
                thread.title.clone_from(&named);
            }
        }
        // An empty chat is not written, and a schedule on an unwritten chat is
        // a schedule that evaporates. The turn that set it up is saved with it
        // when the turn finishes; this makes sure the thread exists either way.
        self.save_thread();
        self.refresh_threads();
        if let Some(window) = self.window() {
            window.toast(&format!(
                "Scheduled — {}",
                schedule.describe().to_lowercase()
            ));
        }
        Ok(format!(
            "Set: this chat will run {} on its own, starting {}{}. Tell the user it is set up, \
             what it will do, and that they can pause or remove it under Scheduled Chats.",
            schedule.describe().to_lowercase(),
            next.format("%A %-d %B at %H:%M"),
            // Said here rather than only in the tool's description, because this
            // is where the model finds out whether it got it right. A scheduled
            // chat with no name of its own is listed under whatever its first
            // sentence happened to be, weeks later, in a list of them.
            match named {
                Some(named) => format!(", and this chat is now called {named:?}"),
                None => ", and this chat still has no name of its own — pass `title` next time so \
                     it is findable under Scheduled Chats"
                    .to_string(),
            }
        ))
    }

    // -- workflows ------------------------------------------------------------

    /// Act on this chat's workflow.
    ///
    /// Here rather than in `ui::runner` because two things this owns are needed
    /// and neither belongs there: the open thread, which the steps live on, and
    /// the project's folder of saved ones. Everything that touches neither goes
    /// through `workflow::apply`, which the eval harness also calls — so a call
    /// cannot mean one thing in the application and another in the suite.
    fn keep_workflow(&self, arguments: &str) -> Result<String, String> {
        use crate::model::workflow::{self, Action};

        let action = Action::parse(arguments);
        let slug = self.imp().project.borrow().slug.clone();

        let said = match &action {
            Action::Save => {
                let Some(store) = self.store() else {
                    return Err("there is nowhere to save it".into());
                };
                let flow = self
                    .imp()
                    .thread
                    .borrow()
                    .workflow
                    .clone()
                    .ok_or_else(workflow::nothing_planned)?;
                store
                    .save_workflow(&slug, &flow)
                    .map_err(|error| format!("that could not be saved: {error}"))?;
                // The thread's copy remembers where it went, so the strip can
                // stop offering to save what is already saved.
                if let Some(open) = self.imp().thread.borrow_mut().workflow.as_mut() {
                    open.saved_as = Some(crate::model::project::slugify(&flow.goal));
                }
                workflow::saved(&flow.goal)
            }
            Action::Start(name) => {
                let store = self
                    .store()
                    .ok_or_else(|| "there is nowhere to look".to_string())?;
                let found = store
                    .load_workflow(&slug, name)
                    .map_err(|error| format!("those could not be read: {error}"))?
                    .ok_or_else(|| workflow::no_such(name))?;
                let said = found.render();
                self.imp().thread.borrow_mut().workflow = Some(found);
                said
            }
            _ => {
                // Taken out and put back rather than held across the call: the
                // borrow would still be live when `refresh_workflow` redraws,
                // and a handler that re-entered the application would find the
                // RefCell held. Every other borrow here keeps the same rule.
                let mut open = self.imp().thread.borrow_mut().workflow.take();
                let outcome = workflow::apply(&mut open, &action);
                self.imp().thread.borrow_mut().workflow = open;
                outcome?
            }
        };

        self.save_thread();
        self.refresh_workflow();
        Ok(said)
    }

    /// The user clicked Start.
    ///
    /// It marks the plan live and then *asks* — as a turn, in their own words —
    /// rather than reaching into the tool loop. A workflow is carried out by the
    /// model taking the next step, and there is exactly one way for that to
    /// begin. A second entry point that drove the steps directly would be a
    /// second implementation of the loop, and the one the eval measures would
    /// not be the one the button used.
    fn start_workflow(&self) {
        {
            let mut thread = self.imp().thread.borrow_mut();
            let Some(workflow) = thread.workflow.as_mut() else {
                return;
            };
            if workflow.started {
                return;
            }
            workflow.started = true;
        }
        self.save_thread();
        self.refresh_workflow();
        self.submit("Go ahead with that — start at step 1.");
    }

    /// Stop the workflow, keeping what it did.
    ///
    /// The steps and their outcomes stay in the thread's record; what goes is
    /// the strip and the model's sense that there is a job in progress. Deleting
    /// the record would throw away the part the user might want to save.
    fn stop_workflow(&self) {
        let Some(window) = self.window() else { return };
        let had = self.imp().thread.borrow_mut().workflow.take();
        if had.is_none() {
            return;
        }
        self.save_thread();
        self.refresh_workflow();

        // An undo, not a question: it is one field and it is still in memory,
        // which is the same call `delete_thread` makes.
        let toast = adw::Toast::new("Workflow stopped");
        toast.set_button_label(Some("Undo"));
        toast.connect_button_clicked(clone!(
            #[weak(rename_to = app)]
            self,
            #[strong]
            had,
            move |_| {
                app.imp().thread.borrow_mut().workflow = had.clone();
                app.save_thread();
                app.refresh_workflow();
            }
        ));
        window.present_toast(&toast);
    }

    /// Open the editor, and put back whatever comes out of it.
    ///
    /// The user's edit wins outright — it is not merged with anything, because
    /// there is nothing to merge it with. What the *model* must not do is carry
    /// on against a plan it last read three rounds ago, so the change is
    /// announced in the transcript rather than swapped in underneath it.
    fn edit_workflow(&self) {
        let Some(window) = self.window() else { return };
        let Some(before) = self.imp().thread.borrow().workflow.clone() else {
            return;
        };
        crate::ui::dialogs::edit_workflow(
            &window,
            &before,
            clone!(
                #[weak(rename_to = app)]
                self,
                move |after: crate::model::workflow::Workflow| {
                    // `after.edited` is what the model will be told, and the
                    // dialog set it — it is the only thing that knows what the
                    // user touched. An unchanged save leaves it `None` and
                    // nothing is announced.
                    let announced = after.edited.clone();
                    app.imp().thread.borrow_mut().workflow = Some(after);
                    if let Some(changed) = announced {
                        app.imp()
                            .thread
                            .borrow_mut()
                            .push_note(format!("You changed the workflow — {changed}."));
                        app.show_thread();
                    }
                    app.save_thread();
                    app.refresh_workflow();
                }
            ),
        );
    }

    /// Redraw the strip from the open thread.
    ///
    /// Called after anything that could have changed the workflow: a tool call,
    /// an edit, opening another chat. The bar draws nothing at all when there is
    /// none, so a chat that never plans one never sees it.
    fn refresh_workflow(&self) {
        let Some(window) = self.window() else { return };
        let workflow = self.imp().thread.borrow().workflow.clone();
        window.workflow_bar().set_workflow(workflow.as_ref());
    }

    /// Whether a capability that is switched off could actually work here.
    ///
    /// The catalogue only offers what passes this. A model told it can switch
    /// on `magpie` on a machine with no Magpie will switch it on, call it, and
    /// be told the command does not exist — which is a wasted round and, worse,
    /// teaches it that the catalogue is not to be believed.
    ///
    /// Answered from a cache: it is asked once per capability per round, and
    /// nothing in it changes while the application is running except the mail
    /// account, which is read live for that reason.
    fn usable(&self, capability: &str) -> bool {
        let workspace = || {
            self.imp()
                .project
                .borrow()
                .workspace
                .as_ref()
                .is_some_and(|root| root.is_dir())
        };
        match capability {
            // Offered whether or not a folder has been chosen, because
            // switching them on establishes one. Refusing until the user goes
            // and configures a workspace would be the discovery problem this
            // whole mechanism exists to remove, moved one step along.
            "workspace" | "documents" => true,
            // `gh` acts on the repository it is standing in, so this one really
            // does need a folder somebody chose — a default directory is not a
            // checkout of anything.
            "github" => workspace() && self.installed("gh"),
            "python" => self.installed("podman"),
            "planner" => self.installed("planner"),
            "magpie" => self.installed("magpie"),
            // Live, because it is the one that changes mid-session: somebody
            // adds an account in Preferences and the capability becomes real
            // without a restart.
            "mail" => self
                .imp()
                .settings
                .borrow()
                .mail
                .as_ref()
                .is_some_and(|account| {
                    !account.host.trim().is_empty() && !account.user.trim().is_empty()
                }),
            // Needs nothing installed and nothing configured — it is this
            // application's own timer over this application's own thread.
            "scheduling" => true,
            // Nor does this: the steps live on the thread and the saved ones are
            // files beside it, so the catalogue can always offer it.
            "workflow" => true,
            "escalate" => {
                let backend = crate::model::escalate::Backend::parse(
                    &self.imp().settings.borrow().escalate_to,
                )
                .unwrap_or_default();
                self.installed(backend.label())
            }
            _ => false,
        }
    }

    /// `PATH` lookups, remembered. A program does not appear on the `PATH` while
    /// this process is running, and `build_request` runs on every round.
    fn installed(&self, program: &str) -> bool {
        if let Some(known) = self.imp().installed.borrow().get(program) {
            return *known;
        }
        let found = crate::model::capability::installed(program);
        self.imp()
            .installed
            .borrow_mut()
            .insert(program.to_string(), found);
        found
    }

    /// Which stronger model this project may ask, if it may ask one.
    fn escalation(&self) -> Option<crate::ui::runner::Escalation> {
        if !self.imp().project.borrow().tools.escalate {
            return None;
        }
        let store = self.store()?;
        let settings = self.imp().settings.borrow();
        Some(crate::ui::runner::Escalation {
            backend: crate::model::escalate::Backend::parse(&settings.escalate_to)
                .unwrap_or_default(),
            model: settings.escalate_model.clone(),
            root: store.root().to_path_buf(),
        })
    }

    /// The mail account this project may read, if it may read one.
    fn mail_account(&self) -> Option<crate::ui::mail::Account> {
        if !self.imp().project.borrow().tools.mail {
            return None;
        }
        let settings = self.imp().settings.borrow();
        let account = settings.mail.as_ref()?;
        Some(crate::ui::mail::Account {
            host: account.host.clone(),
            port: account.port,
            user: account.user.clone(),
            password: account.password.clone(),
            tls: account.tls,
            from: if account.from.trim().is_empty() {
                account.user.clone()
            } else {
                account.from.clone()
            },
            smtp_host: if account.smtp_host.trim().is_empty() {
                account.host.clone()
            } else {
                account.smtp_host.clone()
            },
            smtp_port: account.smtp_port,
        })
    }

    /// The folder this project works in, if it has one that exists.
    fn workspace(&self) -> Option<crate::model::workspace::Workspace> {
        self.imp()
            .project
            .borrow()
            .workspace
            .clone()
            .filter(|root| root.is_dir())
            .map(crate::model::workspace::Workspace::new)
    }

    /// Write the images a turn was asked with into the project, and return the
    /// names the chat refers to them by.
    fn keep_images(&self, attached: &[Attachment]) -> Vec<String> {
        let Some(store) = self.store() else {
            return Vec::new();
        };
        let directory = store
            .root()
            .join("projects")
            .join(&self.imp().project.borrow().slug);
        attached
            .iter()
            .filter(|image| images::store(&directory, image).is_ok())
            .map(|image| image.name.clone())
            .collect()
    }

    /// Load the images a stored turn refers to, skipping any that have gone.
    fn images_of(&self, turn: &StoredTurn) -> Vec<gtk::gdk::Texture> {
        let Some(store) = self.store() else {
            return Vec::new();
        };
        let directory = store
            .root()
            .join("projects")
            .join(&self.imp().project.borrow().slug);
        turn.images
            .iter()
            .filter_map(|name| images::load(&directory, name).ok())
            .filter_map(|image| {
                gtk::gdk::Texture::from_bytes(&glib::Bytes::from(&image.bytes)).ok()
            })
            .collect()
    }

    /// Chips for the calls that have run, plus the one still arriving.
    fn draw_chips(&self, settled: &[ToolCall], streaming: &[ToolCall]) {
        let Some(turn) = self
            .imp()
            .in_flight
            .borrow()
            .as_ref()
            .map(|t| t.view.clone())
        else {
            return;
        };
        let mut chips: Vec<Chip> = settled.iter().map(Chip::of).collect();
        // A running tool that is saying something says it here, in place of the
        // argument — which for a four-minute transcript is the whole difference
        // between "working" and "frozen".
        let progress = self.imp().progress.borrow().clone();
        for call in streaming {
            if settled.iter().any(|done| done.id == call.id) {
                continue;
            }
            chips.push(Chip::of(call).saying(progress.clone()));
        }
        turn.widget().set_tool_calls(&chips);
    }

    /// Keep what was said, whatever happened to the connection. A turn that
    /// produced nothing at all is not a turn.
    fn record(&self, question: &str, state: &TurnState, attached: &[Attachment]) {
        if state.is_empty() {
            return;
        }
        let mut stored = StoredTurn::new(question, state);
        stored.images = self.keep_images(attached);
        {
            let mut thread = self.imp().thread.borrow_mut();
            thread.push_turn(stored);
        }
        self.save_thread();
        self.refresh_threads();
        if let Some(window) = self.window() {
            window.set_thread_title(&self.imp().thread.borrow().display_title());
        }
        self.refresh_status();
    }

    fn stop(&self) {
        // Cancel and leave the rest to `on_finished`: the transport reports the
        // cancellation like any other ending, so there is one settle path.
        if let Some(turn) = self.imp().in_flight.borrow().as_ref() {
            turn.cancellable.cancel();
        }
    }

    fn build_request(&self, question: &str, extra: Vec<Message>) -> ChatRequest {
        let imp = self.imp();
        let settings = imp.settings.borrow();
        let project = imp.project.borrow();
        let has_vault = imp.memory.borrow().is_some();

        // The file tools switched on with no folder chosen offer nothing: they
        // would fail on every call and teach the model not to use them.
        let mut available = project.tools;
        available.workspace =
            available.workspace && project.workspace.as_ref().is_some_and(|root| root.is_dir());

        let mut offered = tools::for_tools(&available, has_vault);
        let mut capabilities = tools::guidance(&available, has_vault);

        // What this project has not switched on but could. Rebuilt per round,
        // like everything else here, which is what lets a capability switched on
        // mid-turn be callable on the very next one.
        let reachable = crate::model::capability::offerable(&available, |name| self.usable(name));
        if let Some(tool) = tools::discovery_tool(&reachable) {
            offered.push(tool);
            if let Some(note) = crate::model::capability::catalogue(&reachable) {
                capabilities.push(note);
            }
        }

        // Semi-volatile: recomputed at thread boundaries, never mid-turn, so
        // the KV prefix survives a turn that calls tools.
        let ambient = imp.ambient.borrow().clone();

        // The project's instructions, and never its name: see the header of
        // `model::project` for why the model is not told the word.
        let volatile = date_line(chrono::Local::now());
        let system = Prompt {
            persona: DEFAULT_PERSONA,
            instructions: project.instructions.as_deref(),
            capabilities: &capabilities,
            ambient: ambient.as_deref(),
            volatile: &volatile,
        }
        .compose();

        // Whether this is the first request of the turn or a round after a
        // tool: the difference decides whether compaction may run.
        let boundary = extra.is_empty();
        let mut history = imp
            .thread
            .borrow()
            .messages_with_reasoning(settings.carry_reasoning);

        let (attached, documents) = imp
            .in_flight
            .borrow()
            .as_ref()
            .map(|turn| {
                (
                    turn.images
                        .iter()
                        .map(|image| image.data_url())
                        .collect::<Vec<_>>(),
                    turn.documents.clone(),
                )
            })
            .unwrap_or_default();

        // Documents come before the question: the model should know what it is
        // reading before it is asked about it.
        let asked = if documents.is_empty() {
            question.to_string()
        } else {
            format!("{}\n\n{question}", documents.join("\n\n"))
        };

        history.push(if attached.is_empty() {
            Message::user(asked)
        } else {
            Message::user_with_images(asked, attached)
        });
        history.extend(extra);

        let floor = imp
            .in_flight
            .borrow()
            .as_ref()
            .is_some_and(|turn| turn.floor);
        if floor {
            // The emergency path. Nothing smaller can be asked, so a thread can
            // never permanently wedge.
            compaction::reduce_to_floor(&mut history);
        } else if settings.compaction {
            // Applying a fold is pure and cheap, so it runs on every request.
            // *Which* fold is fixed at the boundary and carried on the turn:
            // summarising is asynchronous, so one can land between two rounds of
            // a turn already in flight, and taking the thread's newest here
            // would rewrite the prompt mid-turn and throw away the KV prefix
            // the server cached — the one thing this file keeps saying not to do.
            let fold = if boundary {
                let current = imp.thread.borrow().fold.clone();
                if let Some(turn) = imp.in_flight.borrow_mut().as_mut() {
                    turn.fold.clone_from(&current);
                }
                current
            } else {
                imp.in_flight
                    .borrow()
                    .as_ref()
                    .and_then(|turn| turn.fold.clone())
            };
            history = compaction::view(&history, fold.as_ref());
        }

        let mut messages = vec![Message::system(system)];
        messages.extend(history);

        ChatRequest {
            model: imp.config.borrow().model.clone(),
            messages,
            stream: true,
            stream_options: Default::default(),
            temperature: Some(settings.temperature),
            top_p: Some(settings.top_p),
            max_tokens: settings.max_tokens,
            reasoning_budget: settings.reasoning_budget,
            tools: offered.iter().map(|tool| tool.declaration()).collect(),
        }
    }

    /// Put a note in the thread saying what left the model's view.
    fn announce(&self, folded: Compacted) {
        let Some(note) = folded.note() else { return };
        if let Some(window) = self.window() {
            window.toast(&note);
        }
        self.imp().thread.borrow_mut().push_note(note);
    }

    /// Fold the thread if it has grown into the top of the context window.
    ///
    /// Between turns, never during one, and asynchronously: summarising is a
    /// second call to the same server, and the request is built on the main
    /// thread where waiting for one would freeze the window. So the turn that
    /// crossed the line is sent unfolded and the *next* one benefits — which is
    /// what the margin under [`compaction::FOLD_ABOVE`] is for.
    fn fold_if_needed(&self) {
        let imp = self.imp();
        let (compaction_on, keep_recent, carry_reasoning) = {
            let settings = imp.settings.borrow();
            (
                settings.compaction,
                settings.keep_recent_turns,
                settings.carry_reasoning,
            )
        };
        if !compaction_on || imp.folding.get() {
            return;
        }
        let Some(client) = imp.client.borrow().clone() else {
            return;
        };

        let used = imp
            .thread
            .borrow()
            .turns()
            .last()
            .and_then(|turn| turn.metrics)
            .map(|metrics| metrics.prompt_tokens + metrics.generated_tokens);
        let window = imp
            .server
            .borrow()
            .as_ref()
            .and_then(|info| info.context_window)
            .unwrap_or(imp.settings.borrow().context_window);
        if !compaction::should_fold(used, window) {
            return;
        }

        let fold = imp.thread.borrow().fold.clone();
        let history = imp.thread.borrow().messages_with_reasoning(carry_reasoning);
        let Some((chunk, more)) = compaction::to_summarize(&history, fold.as_ref(), keep_recent)
        else {
            return;
        };

        let request =
            compaction::summary_request(fold.as_ref().map(|fold| fold.summary.as_str()), &chunk);
        imp.folding.set(true);

        // The summarizer's reply arrives as an ordinary stream, so it is parsed
        // by the ordinary parser — a small model will put thinking in front of
        // the summary and `TurnState::answer` is what has it stripped off.
        let stream = Rc::new(RefCell::new(TurnStream::new()));
        client.stream(
            &request,
            clone!(
                #[strong]
                stream,
                move |text: &str| {
                    stream.borrow_mut().push(text);
                }
            ),
            clone!(
                #[weak(rename_to = app)]
                self,
                #[strong]
                stream,
                #[strong]
                chunk,
                move |outcome| {
                    let summary = match outcome {
                        Ok(()) => {
                            let state = std::mem::take(&mut *stream.borrow_mut()).finish();
                            Some(state.answer.trim().to_string())
                                .filter(|answer| !answer.is_empty())
                        }
                        Err(_) => None,
                    };
                    app.install_fold(summary, &chunk, more);
                }
            ),
        );
    }

    /// Store what the summarizer produced, and say so in the thread.
    ///
    /// A summary the server would not produce falls back to [`Headings`], which
    /// needs nothing and cannot fail. Folding is what keeps a long thread
    /// sendable, so the one outcome that must not happen is not folding at all.
    fn install_fold(&self, summary: Option<String>, chunk: &[Message], more: usize) {
        let imp = self.imp();
        imp.folding.set(false);

        let previous = imp.thread.borrow().fold.clone();
        let fold = match summary {
            Some(summary) => compaction::Fold {
                summary,
                covers: previous.as_ref().map_or(0, |fold| fold.covers) + more,
            },
            None => compaction::extend(previous.as_ref(), chunk, more, &Headings),
        };

        imp.thread.borrow_mut().fold = Some(fold);
        self.announce(Compacted::Folded { turns: more });
        self.save_thread();
        self.refresh_status();
    }

    // -- the server -----------------------------------------------------------

    fn probe_server(&self) {
        let Some(client) = self.imp().client.borrow().clone() else {
            return;
        };
        client.probe(clone!(
            #[weak(rename_to = app)]
            self,
            move |result| match result {
                Ok(info) => {
                    app.imp().server.replace(Some(info));
                    if let Some(window) = app.window() {
                        window.set_trouble(None);
                        window.composer().set_reachable(true, None);
                    }
                    app.refresh_status();
                }
                Err(error) => app.report_unreachable(&error.to_string()),
            }
        ));
    }

    fn report_unreachable(&self, detail: &str) {
        let Some(window) = self.window() else {
            return;
        };
        let url = self.imp().config.borrow().server_url.clone();
        window.set_trouble(Some(&format!("No llama-server at {url} — {detail}")));
        window
            .composer()
            .set_reachable(false, Some("No llama-server to send to"));
        self.refresh_status();
    }

    /// The bottom bar: which model, and how full its context is.
    fn refresh_status(&self) {
        let Some(window) = self.window() else {
            return;
        };
        let imp = self.imp();
        let server = imp.server.borrow();

        let model = server
            .as_ref()
            .and_then(|info| info.model.clone())
            .unwrap_or_else(|| "Not connected".to_string());

        // The prompt tokens of the last turn are what the model actually read,
        // which is a truer measure of the thread's weight than counting
        // characters.
        let window_size = server
            .as_ref()
            .and_then(|info| info.context_window)
            .unwrap_or(imp.settings.borrow().context_window);
        let used = imp
            .thread
            .borrow()
            .turns()
            .last()
            .and_then(|turn| turn.metrics)
            .map(|metrics| metrics.prompt_tokens + metrics.generated_tokens);

        match used.filter(|_| window_size > 0) {
            Some(used) => {
                let fraction = f64::from(used) / f64::from(window_size);
                window.set_status(&format!(
                    "{model} · {}% of context",
                    (fraction * 100.0).round()
                ));
                window.set_context_usage(Some(fraction));
            }
            None => {
                window.set_status(&model);
                window.set_context_usage(None);
            }
        }
    }

    // -- odds and ends --------------------------------------------------------

    fn remember_window(&self) {
        let Some(window) = self.window() else {
            return;
        };
        let imp = self.imp();
        {
            let mut settings = imp.settings.borrow_mut();
            // A maximised window records that it was maximised rather than its
            // maximised size, so unmaximising returns to the shape it had.
            settings.window_maximized = window.is_maximized();
            // Zero means the window is not mapped and has no shape to record;
            // keeping the previous one beats overwriting it with nothing.
            if !settings.window_maximized && window.width() > 0 && window.height() > 0 {
                settings.window_width = Some(window.width());
                settings.window_height = Some(window.height());
            }
        }
        let settings = imp.settings.borrow();
        let _ = settings.save(&imp.settings_path.borrow());
    }

    /// Every chat in every project that wakes on its own.
    ///
    /// Read from disk rather than from anything in memory: only the open chat
    /// is loaded, and a schedule set up last week on a chat in another project
    /// is exactly the one somebody comes here to find. The open chat is taken
    /// from memory instead, so a schedule just set is listed before it has been
    /// written out.
    fn scheduled(&self) -> Vec<dialogs::Scheduled> {
        let Some(store) = self.store() else {
            return Vec::new();
        };
        let now = chrono::Local::now();
        let open = self.imp().thread.borrow().id.clone();
        let open_slug = self.imp().project.borrow().slug.clone();

        let mut found = Vec::new();
        for project in self.imp().projects.borrow().iter() {
            let Ok(threads) = store.threads(&project.slug) else {
                continue;
            };
            for summary in threads {
                let thread = if summary.id == open && project.slug == open_slug {
                    self.imp().thread.borrow().clone()
                } else {
                    match store.load_thread(&project.slug, &summary.id) {
                        Ok(thread) => thread,
                        Err(_) => continue,
                    }
                };
                let Some(heartbeat) = thread.heartbeat.clone() else {
                    continue;
                };
                found.push(dialogs::Scheduled {
                    slug: project.slug.clone(),
                    project: project.name.clone(),
                    thread: thread.id.to_string(),
                    title: thread.display_title(),
                    schedule: heartbeat.schedule.describe(),
                    prompt: heartbeat.prompt.clone(),
                    enabled: heartbeat.enabled,
                    status: describe_status(&heartbeat, now),
                });
            }
        }
        found
    }

    /// Set or change when the open thread wakes.
    fn schedule_thread(&self) {
        let Some(window) = self.window() else { return };
        let existing = self
            .imp()
            .thread
            .borrow()
            .heartbeat
            .as_ref()
            .map(|heartbeat| (heartbeat.schedule, heartbeat.prompt.clone()));

        dialogs::edit_schedule(
            &window,
            existing,
            clone!(
                #[weak(rename_to = app)]
                self,
                move |chosen| {
                    let Some((schedule, prompt)) = chosen else {
                        return;
                    };
                    {
                        let mut thread = app.imp().thread.borrow_mut();
                        match thread.heartbeat.as_mut() {
                            // Editing keeps `last_run`, so changing the time
                            // does not make a run that already happened today
                            // happen again.
                            Some(heartbeat) => {
                                heartbeat.schedule = schedule;
                                heartbeat.prompt = prompt;
                                heartbeat.enabled = true;
                            }
                            None => {
                                let mut fresh =
                                    crate::model::thread::Heartbeat::new(schedule, &prompt);
                                // The clock starts now rather than at the
                                // epoch: a daily set up at 09:00 waits for
                                // tomorrow's 07:00 instead of firing at once
                                // for a 07:00 that has already gone.
                                fresh.last_run = Some(chrono::Utc::now());
                                thread.heartbeat = Some(fresh);
                            }
                        }
                    }
                    app.save_thread();
                    if let Some(window) = app.window() {
                        let next = app
                            .imp()
                            .thread
                            .borrow()
                            .heartbeat
                            .as_ref()
                            .and_then(|heartbeat| heartbeat.next_run(chrono::Local::now()))
                            .map(|when| when.format("%a %-d %B at %H:%M").to_string());
                        window.toast(&match next {
                            Some(next) => format!("Scheduled. Next run {next}."),
                            None => "Scheduled.".to_string(),
                        });
                    }
                }
            ),
        );
    }

    fn show_schedules(&self) {
        let Some(window) = self.window() else { return };
        dialogs::present_schedules(
            &window,
            &self.scheduled(),
            clone!(
                #[weak(rename_to = app)]
                self,
                move |change: dialogs::Change| match change {
                    dialogs::Change::Enabled { slug, thread, on } => {
                        app.edit_heartbeat(&slug, &thread, |heartbeat| heartbeat.enabled = on);
                    }
                    dialogs::Change::Deleted { slug, thread } => {
                        app.remove_heartbeat(&slug, &thread);
                    }
                    dialogs::Change::Opened { slug, thread } => app.open_thread(&slug, &thread),
                }
            ),
        );
    }

    /// Change a schedule wherever its thread lives, open or not.
    fn edit_heartbeat<F>(&self, slug: &str, thread: &str, change: F)
    where
        F: FnOnce(&mut crate::model::thread::Heartbeat),
    {
        let (Some(store), Some(id)) = (self.store(), ThreadId::from_stem(thread)) else {
            return;
        };
        // The open thread is edited in memory and saved, or a later write of
        // the open thread would overwrite what was just changed on disk.
        let is_open =
            self.imp().thread.borrow().id == id && self.imp().project.borrow().slug == slug;
        if is_open {
            if let Some(heartbeat) = self.imp().thread.borrow_mut().heartbeat.as_mut() {
                change(heartbeat);
            }
            self.save_thread();
            return;
        }
        let Ok(mut loaded) = store.load_thread(slug, &id) else {
            return;
        };
        if let Some(heartbeat) = loaded.heartbeat.as_mut() {
            change(heartbeat);
        }
        let _ = store.save_thread(slug, &loaded);
    }

    /// Stop a thread waking. The thread and everything it said stay.
    fn remove_heartbeat(&self, slug: &str, thread: &str) {
        let (Some(store), Some(id)) = (self.store(), ThreadId::from_stem(thread)) else {
            return;
        };
        let is_open =
            self.imp().thread.borrow().id == id && self.imp().project.borrow().slug == slug;
        if is_open {
            self.imp().thread.borrow_mut().heartbeat = None;
            self.save_thread();
        } else if let Ok(mut loaded) = store.load_thread(slug, &id) {
            loaded.heartbeat = None;
            let _ = store.save_thread(slug, &loaded);
        }
        if let Some(window) = self.window() {
            window.toast("Schedule removed. The chat is still here.");
        }
    }

    fn show_preferences(&self) {
        let Some(window) = self.window() else { return };
        let current = Preferences {
            config: self.imp().config.borrow().clone(),
            settings: self.imp().settings.borrow().clone(),
        };
        preferences::present(
            &window,
            &current,
            clone!(
                #[weak(rename_to = app)]
                self,
                move |edited: Preferences| {
                    let imp = app.imp();
                    imp.config.replace(edited.config.clone());
                    imp.settings.replace(edited.settings.clone());
                    // Written as they are changed: there is no Save button, so
                    // closing the dialog must not be able to lose a setting.
                    let _ = edited.settings.save(&imp.settings_path.borrow());
                    let _ = edited.config.save(&Config::default_path());
                    app.refresh_status();
                }
            ),
        );
    }

    fn show_about(&self) {
        let dialog = adw::AboutDialog::builder()
            .application_name("Familiar")
            .application_icon(APP_ID)
            .developer_name("Matthew Hagrelius")
            .version(env!("CARGO_PKG_VERSION"))
            .license_type(gtk::License::Gpl30)
            .comments("An assistant for GNOME, running on a model you host.")
            .build();
        dialog.present(self.window().as_ref());
    }
}

/// The shortcuts dialog.
///
/// Written by hand rather than derived from the accel map: the map knows which
/// keys are bound but not which are worth telling someone about, or what to
/// call them. Sections in the order you meet them.
/// Decode attachments for display. An image that will not decode is left out
/// rather than shown as a broken tile — it still goes to the model, which is
/// reading the bytes rather than a GdkTexture.
fn textures(attached: &[Attachment]) -> Vec<gtk::gdk::Texture> {
    attached
        .iter()
        .filter_map(|image| gtk::gdk::Texture::from_bytes(&glib::Bytes::from(&image.bytes)).ok())
        .collect()
}

fn show_shortcuts(window: &Window) {
    let dialog = adw::ShortcutsDialog::new();

    for (title, shortcuts) in [
        (
            "Conversation",
            &[
                ("Send", "Return"),
                ("New line", "<Control>Return"),
                ("Explain the selected text", "<Control><Shift>e"),
                ("Stop generating", "Escape"),
            ][..],
        ),
        (
            "Chats and Projects",
            &[("New chat", "<Control>n"), ("Toggle sidebar", "F9")][..],
        ),
        (
            "Application",
            &[
                ("Preferences", "<Control>comma"),
                ("Keyboard shortcuts", "<Control>question"),
                ("Quit", "<Control>q"),
            ][..],
        ),
    ] {
        let section = adw::ShortcutsSection::new(Some(title));
        for (name, accelerator) in shortcuts {
            section.add(adw::ShortcutsItem::new(name, accelerator));
        }
        dialog.add(section);
    }

    dialog.present(Some(window));
}

/// What a scheduled thread's row says under its switch.
///
/// Three states worth telling apart, because they call for different things
/// from the reader: paused is a decision they made, never-run is a schedule
/// waiting for its first occurrence, and a last run is the only evidence the
/// thing works at all.
fn describe_status(
    heartbeat: &crate::model::thread::Heartbeat,
    now: chrono::DateTime<chrono::Local>,
) -> String {
    if !heartbeat.enabled {
        return "Paused".to_string();
    }
    let next = heartbeat
        .next_run(now)
        .map(|when| when.format("%a %-d %B at %H:%M").to_string());
    match (heartbeat.last_run, next) {
        (Some(last), Some(next)) => format!(
            "Last ran {} · next {next}",
            ago(now - last.with_timezone(&chrono::Local))
        ),
        (Some(last), None) => format!("Last ran {}", ago(now - last.with_timezone(&chrono::Local))),
        (None, Some(next)) => format!("Has not run yet · next {next}"),
        (None, None) => "Has not run yet".to_string(),
    }
}

/// A typed name that can only ever be one entry in the folder it was typed in.
///
/// `..` and anything with a separator in it are refused rather than resolved:
/// the dialog asked for a name, and a name with a slash in it is somebody
/// asking for a path — which would land outside the folder the menu was opened
/// on.
fn one_name(typed: &str) -> Option<String> {
    let name = typed.trim();
    let sane = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0');
    sane.then(|| name.to_string())
}

/// A duration as a person says it. Deliberately coarse: "3 hours ago" is what
/// anyone wants from a status line, and "3 hours 14 minutes ago" is noise.
fn ago(elapsed: chrono::Duration) -> String {
    let minutes = elapsed.num_minutes();
    if minutes < 1 {
        return "just now".to_string();
    }
    if minutes < 60 {
        return format!("{minutes} minute{} ago", plural(minutes));
    }
    let hours = elapsed.num_hours();
    if hours < 24 {
        return format!("{hours} hour{} ago", plural(hours));
    }
    let days = elapsed.num_days();
    format!("{days} day{} ago", plural(days))
}

fn plural(count: i64) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}
