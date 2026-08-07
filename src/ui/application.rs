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
/// The spoken half's pure logic. Aliased because `ui::voice` is its other half
/// and both are wanted in this file.
use crate::model::voice as spoken;
use crate::model::web::Budget;
use crate::model::wire::{ChatRequest, Message, ToolInvocation};
use crate::ui::approval::{self, Decision};
use crate::ui::client::{Client, ClientError};
use crate::ui::embedder::Embeddings;
use crate::ui::preferences::{self, Preferences};
use crate::ui::runner::Runner;
use crate::ui::voice::{self, Recorder, Speaker, Speech, State, Talk, VoiceWindow};
use crate::ui::{dialogs, Chip, TurnView, Window, WorkflowBar};
use crate::APP_ID;

/// How long to wait after a turn before reading it for anything durable.
///
/// Long enough that a follow-up typed straight away goes first, short enough
/// that closing the window seldom loses the read.
const HARVEST_DELAY: u32 = 5;

/// How long a spoken question may sit being transcribed before something is
/// assumed to have gone wrong. The pass itself is a quarter of a second.
const TRANSCRIBE_PATIENCE: u32 = 20_000;

/// How long to listen with no audio arriving at all before deciding the
/// microphone is not going to produce any. Reads come every 40 ms, so this is
/// three orders of magnitude of grace.
const DEAF_MICROPHONE: std::time::Duration = std::time::Duration::from_secs(3);

/// And how long it may sit being answered. Long, because a turn that calls
/// four tools legitimately takes minutes — this is a backstop against a turn
/// that is not running at all, not a limit on how long an answer may take.
const THINK_PATIENCE: u32 = 300_000;

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
/// Which chat a running turn belongs to.
///
/// The open chat's state stays in the application's slot, where the tools that
/// mutate it mid-turn — `schedule` writes `heartbeat`, `workflow` writes
/// `workflow` — already write, and where the sidebar reads it. Only a run
/// against a chat that is *not* open needs the turn to carry its own, and that
/// is the whole difference between the two arms.
pub enum Chat {
    Open,
    /// Boxed because a `Thread` is large and the open case carries none.
    Background(Box<Thread>),
}

/// The chat a turn is running against, and the widget showing it if one is.
///
/// A scheduled run happens against a thread that is usually not open and may
/// have no window at all, so the turn carries what it needs rather than
/// reading the application's slot. That is what lets a run happen in the
/// background without either freezing navigation for its duration or writing
/// the answer into whatever chat the user has since clicked on.
///
/// The project rides along because [`Application::build_request`] needs it —
/// tools, instructions and workspace are the project's, and a job's project is
/// not necessarily the open one either.
pub struct Session {
    pub slug: String,
    pub project: Project,
    pub chat: Chat,
    /// `None` for a background run: there is no turn widget, and there may be
    /// no window at all.
    pub view: Option<Rc<TurnView>>,
}

pub struct InFlight {
    pub question: String,
    /// Documents whose text was extracted, framed and ready to go into the
    /// question. Held for the turn, so a tool round still sees them.
    pub documents: Vec<String>,
    /// Images the question was asked with, sent on every round of the turn so
    /// the model can still see them when it reads its tool results.
    pub images: Vec<Attachment>,
    pub stream: TurnStream,
    /// The chat this turn belongs to. Not `imp.thread`, because a scheduled
    /// run's chat is usually not the open one.
    pub session: Session,
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
    /// The job this turn is a run of, if it is one. What its outcome is
    /// recorded against — the job outlives the chat, so "what happened last
    /// time" belongs on the job rather than on whichever thread it wrote into.
    pub job: Option<String>,
    /// The question was spoken and the answer will be read out.
    ///
    /// Two things follow from it and nothing else does: the question carries
    /// `voice::REGISTER`, and the answer is fed to the voice window and the
    /// synthesiser as it streams. The turn is otherwise an ordinary turn.
    pub spoken: bool,
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
        /// Where the job interface is exported, once it has been. `None` when
        /// there is no session bus, which is an ordinary state — every surface
        /// is optional by construction.
        pub bus_path: RefCell<Option<String>>,
        /// Holds the process open with no window while it exists; dropping it
        /// lets the app end when its last window closes. Kept rather than
        /// leaked so background running can be switched off again without a
        /// restart.
        pub held: RefCell<Option<gio::ApplicationHoldGuard>>,
        /// Everything that runs on its own. Loaded once at startup, migrated
        /// from the heartbeats threads used to carry if there is no file yet,
        /// and written whenever one changes.
        pub jobs: RefCell<crate::model::jobs::Jobs>,
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
        /// The spoken exchange and its window, once the shortcut has been
        /// pressed once. `None` until then: building it loads nothing and
        /// costs nothing, but the window is one more thing to keep in step and
        /// most sessions never talk.
        pub talk: RefCell<Option<Talk>>,
        /// The speech worker. Created with the first utterance, not at startup:
        /// it loads most of a gigabyte of ONNX and a session that never speaks
        /// should never pay for it.
        pub speech: RefCell<Option<Rc<Speech>>>,
        /// What reads the answer out.
        pub speaker: RefCell<Option<Rc<Speaker>>>,
        /// Holds the process open while the voice window is on screen, and
        /// only then.
        ///
        /// The window is *not* the application's, deliberately. It is hidden
        /// rather than destroyed between exchanges — rebuilding it would lose
        /// its size and flicker — and a hidden window that belongs to the
        /// application still counts as a window, so `GtkApplication` never
        /// reaches zero and the app cannot quit. Closing the main window then
        /// leaves a process nobody can see and nobody can end. A hold guard
        /// says the same thing exactly and only while it is true.
        pub voice_hold: RefCell<Option<gio::ApplicationHoldGuard>>,
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
            // Before the tick starts, or the first one runs against an empty
            // list and a schedule that was due while the machine was off would
            // be missed by the very pass meant to recover it.
            obj.load_jobs();
            obj.export_jobs();
            obj.apply_background();
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
            let arguments: Vec<String> = command_line
                .arguments()
                .iter()
                .map(|argument| argument.to_string_lossy().to_string())
                .collect();

            // The global shortcut, arriving as a second launch. It deliberately
            // does *not* activate: the point of talking to it is that the main
            // window need not be open, and raising it over whatever the user is
            // working in would make the shortcut something to think twice about
            // pressing. The voice window is the only thing that appears.
            if arguments.iter().any(|argument| argument == "--voice") {
                obj.toggle_voice();
                return glib::ExitCode::SUCCESS;
            }

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
            // Before anything else: speech-dispatcher keeps speaking after the
            // process that asked it to is gone, so quitting mid-sentence would
            // leave a voice talking to an empty desktop.
            if let Some(speaker) = self.speaker.borrow().as_ref() {
                speaker.hush();
            }
            // The voice window belongs to no application, so nothing else will
            // take it down.
            if let Some(talk) = self.talk.borrow().as_ref() {
                talk.window.destroy();
            }
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

        // The same thing the global shortcut does, for the menu and for a
        // desktop where no shortcut could be registered.
        let voice = gio::SimpleAction::new("voice", None);
        voice.connect_activate(clone!(
            #[weak(rename_to = app)]
            self,
            move |_, _| app.toggle_voice()
        ));
        self.add_action(&voice);

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
        // Consolidation keeps the behaviour it has always had: if the machine
        // was off through three o'clock, the night is let go rather than run
        // over breakfast against the same GPU the conversation wants.
        if schedule
            .due(last, now, crate::model::heartbeat::Recovery::OnTime)
            .is_none()
        {
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
        // Any job landing here goes with it. A job pointing at a chat that is
        // gone would run forever against nothing, writing turns nobody can
        // reach — and its prompt was in the file being deleted anyway.
        let orphaned = self
            .imp()
            .jobs
            .borrow_mut()
            .forget_chat(slug, &id.to_string());
        if orphaned > 0 {
            self.save_jobs();
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
                    // Every job belonging to it goes too, for the same reason a
                    // deleted chat takes its jobs: the destination is gone.
                    if app.imp().jobs.borrow_mut().forget_project(&slug) > 0 {
                        app.save_jobs();
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
        let Some(store) = self.store() else {
            return;
        };
        let slug = self.imp().project.borrow().slug.clone();
        let thread = self.imp().thread.borrow();
        if let Err(error) = store.save_thread(&slug, &thread) {
            // Losing what was said is worth a banner, not a toast: a toast is
            // missed while typing and the cost of missing it is the
            // conversation. Only if there is a window to put one in — the save
            // itself must not depend on that, or a background run would write
            // nothing at all and report no error either.
            if let Some(window) = self.window() {
                window.set_trouble(Some(&format!("This chat is not being saved: {error}")));
            }
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

    /// The thread the running turn belongs to.
    ///
    /// Everything in the turn path goes through this rather than reaching for
    /// `imp.thread` directly, and so does every tool that writes to the thread
    /// — `schedule` and `workflow` both do. Without it a scheduled run against
    /// a chat that is not open would set its schedule, or append its answer, to
    /// whichever chat happened to be on screen.
    ///
    /// Falls through to the open chat when no turn is running, or when the one
    /// that is belongs to the open chat anyway, which is the ordinary case.
    fn with_turn_thread<R>(&self, f: impl FnOnce(&mut Thread) -> R) -> R {
        {
            let mut in_flight = self.imp().in_flight.borrow_mut();
            if let Some(Chat::Background(thread)) =
                in_flight.as_mut().map(|turn| &mut turn.session.chat)
            {
                return f(thread);
            }
        }
        f(&mut self.imp().thread.borrow_mut())
    }

    /// Write out the running turn's chat, whichever chat that is.
    ///
    /// The counterpart to [`Self::with_turn_thread`], and needed for the same
    /// reason: a tool that changes the thread has to persist the one it
    /// changed. `save_thread` only ever writes the slot.
    fn save_turn_thread(&self) {
        let background = {
            let mut in_flight = self.imp().in_flight.borrow_mut();
            match in_flight.as_mut().map(|turn| &mut turn.session) {
                Some(Session {
                    slug,
                    chat: Chat::Background(thread),
                    ..
                }) => Some((slug.clone(), thread.clone())),
                _ => None,
            }
        };
        match background {
            Some((slug, thread)) => {
                if let Some(store) = self.store() {
                    let _ = store.save_thread(&slug, &thread);
                }
            }
            None => self.save_thread(),
        }
    }

    /// Which chat the running turn belongs to, as a job's destination names it.
    fn turn_chat(&self) -> (String, String) {
        {
            let in_flight = self.imp().in_flight.borrow();
            if let Some(turn) = in_flight.as_ref() {
                if let Chat::Background(thread) = &turn.session.chat {
                    return (turn.session.slug.clone(), thread.id.to_string());
                }
            }
        }
        (
            self.imp().project.borrow().slug.clone(),
            self.imp().thread.borrow().id.to_string(),
        )
    }

    /// Which chat a settling turn belonged to, as a name that outlives it.
    ///
    /// The session is gone by the time an asynchronous fold comes back, so what
    /// travels is the pair that identifies the chat rather than the chat.
    fn session_chat(&self, session: &mut Session) -> (String, ThreadId) {
        let id = self.with_session_thread(session, |thread| thread.id.clone());
        (session.slug.clone(), id)
    }

    /// A background run has finished for the chat that happens to be on screen.
    ///
    /// The run itself was no different — that is the point of not special-casing
    /// the open chat — but the slot and the widgets have to catch up. Without
    /// this the application's copy is missing the run, and the next thing the
    /// user sends writes that stale copy back over the file, losing it.
    fn adopt_background_turn(&self, session: &Session) {
        let Chat::Background(thread) = &session.chat else {
            return;
        };
        let mine = self.imp().project.borrow().slug == session.slug
            && self.imp().thread.borrow().id == thread.id;
        if !mine {
            return;
        }
        self.imp().thread.replace((**thread).clone());

        // Appended rather than redrawn: `show_thread` also navigates, and a run
        // finishing must not yank somebody off the project page they are
        // reading.
        let Some(window) = self.window() else {
            return;
        };
        if window.showing_project() {
            return;
        }
        let show_thinking = self.imp().settings.borrow().show_thinking;
        let last = self.imp().thread.borrow().turns().last().cloned();
        if let Some(turn) = last {
            let view = TurnView::replayed_with(&turn, show_thinking, &self.images_of(&turn));
            window.conversation().append(view.widget());
        }
    }

    /// The same, for a turn being settled — by then the turn has been taken out
    /// of its cell, so the session is passed in rather than looked up.
    fn with_session_thread<R>(&self, session: &mut Session, f: impl FnOnce(&mut Thread) -> R) -> R {
        match &mut session.chat {
            Chat::Background(thread) => f(thread),
            Chat::Open => f(&mut self.imp().thread.borrow_mut()),
        }
    }

    /// Write the session's chat out, whichever chat that is.
    fn save_session(&self, session: &mut Session) {
        match &mut session.chat {
            Chat::Open => self.save_thread(),
            Chat::Background(thread) => {
                let Some(store) = self.store() else {
                    eprintln!("familiar: no store, so a finished turn was not written");
                    return;
                };
                // Said rather than swallowed. This is the last step of a turn
                // that has already been paid for, and losing it silently is
                // the worst way to lose it.
                if let Err(error) = store.save_thread(&session.slug, thread) {
                    eprintln!("familiar: a finished chat could not be written: {error}");
                }
            }
        }
    }

    /// Load the job list, importing the old per-thread heartbeats once.
    ///
    /// The import happens only when there is no jobs file, or every start would
    /// resurrect schedules the user has since deleted. `last_run` carries over
    /// with everything else — a migration that reset the clock would make every
    /// schedule on the machine come due at once.
    fn load_jobs(&self) {
        let Some(store) = self.store() else {
            return;
        };
        if store.has_jobs_file() {
            self.imp().jobs.replace(store.load_jobs());
            return;
        }
        let slugs: Vec<String> = self
            .imp()
            .projects
            .borrow()
            .iter()
            .map(|project| project.slug.clone())
            .collect();
        let migrated = crate::model::jobs::Jobs::migrated(store.heartbeats(&slugs));
        let _ = store.save_jobs(&migrated);
        self.imp().jobs.replace(migrated);
    }

    /// Write the job list out, and tell every watcher.
    ///
    /// The announcement rides with the save rather than being a separate call
    /// at each site, because "the list changed" and "the list was written" are
    /// the same moment — and a surface that missed one would be showing a
    /// schedule the file no longer agrees with.
    fn save_jobs(&self) {
        if let Some(store) = self.store() {
            let _ = store.save_jobs(&self.imp().jobs.borrow());
        }
        self.announce_jobs();
    }

    /// Keep the process alive with no window, if that was asked for.
    ///
    /// `gio::Application` ends when its last window closes, which for a
    /// scheduled assistant means the schedules only run while somebody is
    /// looking at it — the thing the background run was built to stop being
    /// true. The hold guard is the counterweight; dropping it gives the
    /// ordinary lifetime back.
    ///
    /// **Conditional, and it has to be.** `test.sh` drives the real application
    /// through integration tests; an unconditional hold means they never
    /// terminate. `FAMILIAR_NO_BACKGROUND` is the same kind of guard as
    /// `GTK_A11Y=none` and `GSETTINGS_BACKEND=memory` — a test must never
    /// depend on, or be trapped by, real user state.
    fn apply_background(&self) {
        let wanted = self.imp().settings.borrow().background
            && std::env::var("FAMILIAR_NO_BACKGROUND").is_err();
        let held = self.imp().held.borrow().is_some();
        if wanted == held {
            return;
        }
        if wanted {
            let guard = gio::prelude::ApplicationExtManual::hold(self);
            self.imp().held.replace(Some(guard));
        } else {
            self.imp().held.replace(None);
        }
    }

    /// Put the job list on the bus the application already owns.
    ///
    /// Costs a vtable rather than a subsystem: `GApplication` has a connection
    /// and a bus name by the time this runs. A surface — a shell extension, a
    /// tray, a script — reads it here and never reaches into the app.
    fn export_jobs(&self) {
        let Some(connection) = self.dbus_connection() else {
            // No session bus. Every surface is optional by construction, so
            // this is a missing convenience rather than a failure.
            return;
        };
        let path = format!("{}/Jobs", self.dbus_object_path().unwrap_or_default());
        let registered = crate::ui::jobs_bus::export(
            &connection,
            &path,
            clone!(
                #[weak(rename_to = app)]
                self,
                #[upgrade_or]
                None,
                move |ask| {
                    use crate::ui::jobs_bus::Ask;
                    let now = chrono::Local::now();
                    match ask {
                        Ask::List => Some(crate::ui::jobs_bus::describe_all(
                            &app.imp().jobs.borrow(),
                            now,
                        )),
                        Ask::SetEnabled { id, on } => {
                            let found = app.imp().jobs.borrow().get(&id).is_some();
                            if found {
                                app.edit_job(&id, move |job| job.enabled = on);
                                app.announce_jobs();
                            }
                            Some(found.to_variant())
                        }
                        // Bringing a run forward is setting its clock back far
                        // enough that the next tick finds it owed — rather than
                        // a second path into the turn pipeline, which would be
                        // a way to start two runs at once.
                        Ask::RunNow { id } => {
                            let known = app.imp().jobs.borrow().get(&id).is_some();
                            if known {
                                app.edit_job(&id, |job| {
                                    job.last_run =
                                        Some(chrono::Utc::now() - chrono::Duration::days(400));
                                    job.recovery = crate::model::heartbeat::Recovery::Whenever;
                                });
                                app.announce_jobs();
                            }
                            Some(known.to_variant())
                        }
                    }
                }
            ),
        );
        if registered.is_ok() {
            self.imp().bus_path.replace(Some(path));
        }
    }

    /// Tell every watcher the list changed.
    ///
    /// The push half of push-never-poll. Called wherever a job is added,
    /// edited, paused, deleted or finishes a run, so a panel never needs a
    /// timer of its own.
    fn announce_jobs(&self) {
        let (Some(connection), Some(path)) =
            (self.dbus_connection(), self.imp().bus_path.borrow().clone())
        else {
            return;
        };
        let now = chrono::Local::now();
        let jobs = self.imp().jobs.borrow();
        crate::ui::jobs_bus::changed(
            &connection,
            &path,
            &crate::ui::jobs_bus::describe_all(&jobs, now),
            crate::ui::jobs_bus::overdue(&jobs, now),
        );
    }

    /// One tick: is anything due, and can it run right now?
    ///
    /// Every job is considered the same way, whatever is front and center. A
    /// scheduled run is a background run — routing the open chat through
    /// `submit_turn` instead took the composer's staging area with it, so a
    /// schedule coming due would swallow a file you had attached and not sent.
    fn tick(&self) {
        // Never mid-answer. A scheduled run fires *between* turns, and one model
        // call at a time is the policy against a single local server.
        if self.imp().in_flight.borrow().is_some() {
            return;
        }
        // Nothing is in flight, which is also the only time consolidation may
        // start: it is many requests against the same server the conversation
        // uses, and a person waiting on an answer must not queue behind it.
        self.dream_if_due();
        self.look_out_if_due();
        // The server being down is a reason to wait for the next occurrence,
        // not to submit a turn that will fail.
        if self.imp().client.borrow().is_none() {
            return;
        }

        // One per tick, most overdue first: the next is a minute away, and
        // starting two would put them both on the same GPU.
        let now = chrono::Local::now();
        let owed = self
            .imp()
            .jobs
            .borrow()
            .next_due(now)
            .map(|(job, due, scheduled)| (job.clone(), due, scheduled));
        let Some((job, due, scheduled)) = owed else {
            return;
        };
        let Some((slug, project, thread)) = self.chat_for(&job) else {
            return;
        };

        // Recorded before the turn rather than after: if the answer never
        // arrives the schedule must still have moved on, or every tick inside
        // the window tries again.
        if let Some(held) = self.imp().jobs.borrow_mut().get_mut(&job.id) {
            held.last_run = Some(chrono::Utc::now());
        }
        self.save_jobs();

        let preamble = crate::model::heartbeat::preamble(due, scheduled);
        self.run_in_background(
            slug,
            project,
            thread,
            format!("{preamble}\n\n{}", job.prompt),
            Some(job.id.clone()),
        );
    }

    /// The chat a job writes into, loading or creating it as its destination
    /// says.
    ///
    /// `None` when the job points at a project that is gone — the list is
    /// pruned when a project or chat is deleted, but a store edited by hand or
    /// synced from elsewhere can still say otherwise, and running a job against
    /// nothing would write turns nobody can reach.
    fn chat_for(&self, job: &crate::model::jobs::Job) -> Option<(String, Project, Thread)> {
        use crate::model::jobs::Destination;
        let store = self.store()?;
        let slug = job.destination.slug()?.to_string();
        let project = self
            .imp()
            .projects
            .borrow()
            .iter()
            .find(|project| project.slug == slug)
            .cloned()?;
        match &job.destination {
            Destination::Chat { thread, .. } => {
                let id = crate::model::thread::ThreadId::from_stem(thread)?;
                // The open chat's in-memory copy wins: the file may be a turn
                // behind whatever the user has just said.
                let open =
                    self.imp().project.borrow().slug == slug && self.imp().thread.borrow().id == id;
                let thread = if open {
                    self.imp().thread.borrow().clone()
                } else {
                    store.load_thread(&slug, &id).ok()?
                };
                Some((slug, project, thread))
            }
            // A run of its own each time. The chat is created here rather than
            // at settle, so the turn has somewhere to go from the first round.
            Destination::FreshChat { .. } => {
                let thread = store.new_thread(&slug).ok()?;
                Some((slug, project, thread))
            }
            Destination::Nothing => None,
        }
    }

    fn run_in_background(
        &self,
        slug: String,
        project: Project,
        thread: Thread,
        question: String,
        job: Option<String>,
    ) {
        if self.imp().in_flight.borrow().is_some() {
            return;
        }
        self.imp().in_flight.replace(Some(InFlight {
            question,
            documents: Vec::new(),
            images: Vec::new(),
            stream: TurnStream::new(),
            session: Session {
                slug,
                project,
                chat: Chat::Background(Box::new(thread)),
                view: None,
            },
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
            scheduled: true,
            job,
            spoken: false,
        }));
        self.ask(None);
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
        let (enabled, schedule, last) = {
            let settings = self.imp().settings.borrow();
            (
                settings.lookout,
                settings.lookout_schedule(),
                settings.last_lookout,
            )
        };
        if !enabled {
            return;
        }
        let now = chrono::Utc::now();
        // Through the same arithmetic as everything else that runs on its own,
        // rather than a raw hours comparison — that was a third notion of
        // "due", and the one that had never been asked what to do about a
        // machine that was asleep. `Whenever`: a check that says nothing five
        // times in nine is worth doing late, and there is nothing stale about
        // "look over the day".
        let local = now.with_timezone(&chrono::Local);
        match last {
            Some(last) => {
                if schedule
                    .due(
                        Some(last.with_timezone(&chrono::Local)),
                        local,
                        crate::model::heartbeat::Recovery::Whenever,
                    )
                    .is_none()
                {
                    return;
                }
            }
            // Never run: start the clock rather than firing immediately, the
            // same rule a scheduled chat follows.
            None => {
                let imp = self.imp();
                let mut settings = imp.settings.borrow_mut();
                settings.last_lookout = Some(now);
                let _ = settings.save(&imp.settings_path.borrow());
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
    fn announce_run(&self, id: &str, title: &str, summary: &str) {
        let notification = gio::Notification::new(title);
        notification.set_body(Some(summary));
        notification.set_priority(gio::NotificationPriority::Normal);

        // The chat that ran, which is not necessarily the one on screen. Read
        // from the slot, this opened whatever the user was looking at when a
        // background run finished.
        notification.set_default_action_and_target_value("app.show-thread", Some(&id.to_variant()));
        self.send_notification(Some(&format!("heartbeat:{id}")), &notification);
    }

    // -- talking to it --------------------------------------------------------

    /// The global shortcut, `familiar --voice`, or the menu item.
    ///
    /// One gesture, and what it means is the state it finds. Pressing it while
    /// it is talking stops it talking and starts listening, which is what
    /// interrupting somebody is; pressing it while it is thinking stops the
    /// turn. There is no second key to learn because there is no second key to
    /// register — see `voice::shortcut`.
    pub fn toggle_voice(&self) {
        // Whether the window was already on screen decides what a press means.
        // A hidden window's state is not something anybody can see, so the
        // press cannot sensibly mean "stop that" — it means start. Without
        // this, a state left over from the last exchange eats the first press
        // and the window opens sitting there doing nothing.
        let showing = self
            .voice_window()
            .is_some_and(|window| window.is_visible());
        if !self.open_talk() {
            return;
        }
        if !showing {
            self.cancel_voice();
            self.start_listening();
            return;
        }
        match self.voice_state() {
            State::Idle => self.start_listening(),
            State::Listening => self.stop_listening(true),
            // The accurate pass takes a fifth of a second. Pressing through it
            // means "no, forget it" rather than "hurry up".
            State::Transcribing => self.cancel_voice(),
            State::Thinking => {
                self.stop();
                self.abandon();
                self.go_idle();
            }
            // Interrupting, by hand instead of by talking over it. The same
            // gesture, so the same path: hushing and listening was not enough on
            // its own, because the turn went on streaming and every delta queued
            // another sentence — the button went quiet for a moment and then the
            // answer carried on.
            State::Speaking => self.interrupted(),
        }
    }

    /// Make sure the window exists and is on screen. False if voice cannot run.
    fn open_talk(&self) -> bool {
        if self.imp().talk.borrow().is_some() {
            let talk = self.imp().talk.borrow();
            let window = talk.as_ref().expect("just checked").window.clone();
            drop(talk);
            self.hold_for_voice();
            window.present();
            return true;
        }

        let window = VoiceWindow::new();
        window.connect_local(
            "act",
            false,
            clone!(
                #[weak(rename_to = app)]
                self,
                #[upgrade_or]
                None,
                move |_| {
                    app.toggle_voice();
                    None
                }
            ),
        );
        window.connect_local(
            "cancel",
            false,
            clone!(
                #[weak(rename_to = app)]
                self,
                #[upgrade_or]
                None,
                move |_| {
                    app.cancel_voice();
                    None
                }
            ),
        );
        window.connect_local(
            "fresh",
            false,
            clone!(
                #[weak(rename_to = app)]
                self,
                #[upgrade_or]
                None,
                move |_| {
                    app.fresh_voice_chat();
                    None
                }
            ),
        );
        window.connect_local(
            "open",
            false,
            clone!(
                #[weak(rename_to = app)]
                self,
                #[upgrade_or]
                None,
                move |_| {
                    app.show_voice_chat();
                    None
                }
            ),
        );
        // Closing it is cancelling it, and on a machine where nothing else is
        // holding the process open — no main window, no background running —
        // it is also quitting. Without that, a cold start from the shortcut
        // would leave a process with no visible window behind every time.
        window.connect_close_request(clone!(
            #[weak(rename_to = app)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_| {
                app.cancel_voice();
                // Letting go is the whole of it. With no main window and
                // nothing else holding the process, the application ends by
                // itself; with one, it carries on. No special case either way.
                app.imp().voice_hold.replace(None);
                glib::Propagation::Proceed
            }
        ));

        crate::voice_log!("this build: {}", crate::built_at());
        let talk = Talk::new(window.clone());
        self.imp().talk.replace(Some(talk));
        window.set_state(State::Idle);
        window.set_chat(None);
        self.hold_for_voice();
        window.present();
        true
    }

    /// Keep the process alive while the voice window is up.
    fn hold_for_voice(&self) {
        if self.imp().voice_hold.borrow().is_none() {
            self.imp().voice_hold.replace(Some(self.hold()));
        }
    }

    /// The speech worker, started on first use.
    fn speech(&self) -> Rc<Speech> {
        let existing = self.imp().speech.borrow().clone();
        existing.unwrap_or_else(|| {
            let speech = Rc::new(Speech::new());
            self.imp().speech.replace(Some(speech.clone()));
            speech
        })
    }

    /// What reads the answer out.
    ///
    /// Which voice it uses is settled once per exchange by [`Self::start_listening`]
    /// and not looked at again: this is called for every delta of the answer,
    /// and deciding the voice involves searching the `PATH` for a synthesiser.
    /// Preferences changed mid-answer apply to the next question.
    fn speaker(&self) -> Rc<Speaker> {
        let existing = self.imp().speaker.borrow().clone();
        existing.unwrap_or_else(|| {
            let speaker = Rc::new(Speaker::new());
            speaker.connect_changed(clone!(
                #[weak(rename_to = app)]
                self,
                move |speaking| app.voice_is_speaking(speaking)
            ));
            self.imp().speaker.replace(Some(speaker.clone()));
            speaker
        })
    }

    /// The voice the preferences ask for, or silence if it cannot be used.
    fn chosen_voice(&self) -> voice::Voice {
        let settings = self.imp().settings.borrow();
        let wanted = match settings.voice_reply.as_str() {
            "off" => voice::Voice::Silent,
            "endpoint" => voice::Voice::Endpoint {
                url: settings.voice_endpoint.clone(),
                name: settings.voice_name.clone(),
                rate: settings.voice_rate,
            },
            _ => voice::Voice::Desktop,
        };
        if wanted.is_available() {
            wanted
        } else {
            // The answer is on screen either way. Silence beats a stream of
            // failures, one per sentence.
            voice::Voice::Silent
        }
    }

    fn voice_state(&self) -> State {
        self.imp()
            .talk
            .borrow()
            .as_ref()
            .map(|talk| talk.window.state())
            .unwrap_or(State::Idle)
    }

    fn voice_window(&self) -> Option<VoiceWindow> {
        self.imp()
            .talk
            .borrow()
            .as_ref()
            .map(|talk| talk.window.clone())
    }

    /// Open the microphone because somebody asked for it.
    fn start_listening(&self) {
        self.listen(false);
    }

    /// Open it again after an answer, so the conversation can carry on without
    /// a key being pressed between every question.
    ///
    /// This is what makes it a conversation rather than a series of
    /// dictations, and it costs nothing to leave: say nothing and the
    /// endpointer gives up after its patience runs out and the window goes
    /// quiet. The microphone is only ever open *after* the speaking has
    /// finished, so there is still nothing for it to hear itself say.
    fn carry_on_listening(&self) {
        if !self.imp().settings.borrow().voice_converse {
            return;
        }
        // Not over a window somebody has closed, and not over a turn that has
        // somehow started since.
        let showing = self
            .voice_window()
            .is_some_and(|window| window.is_visible());
        // And only from a standstill. Listening again while already listening
        // would empty the buffer somebody is halfway through talking into.
        if !showing || self.imp().in_flight.borrow().is_some() || self.voice_state() != State::Idle
        {
            return;
        }
        self.listen(true);
    }

    fn listen(&self, carrying_on: bool) {
        let Some(window) = self.voice_window() else {
            return;
        };
        let speaker = self.speaker();
        speaker.hush();
        // Once per exchange, which is where a preference change takes effect.
        let voice = self.chosen_voice();
        crate::voice_log!("voice: {voice:?}");
        speaker.set_voice(voice);

        if !voice::speech::is_installed() {
            window.set_state(State::Idle);
            window.set_trouble(voice::speech::MISSING);
            return;
        }

        // Which chat this is going into, decided before a word is said so the
        // window can show it and the user can change it.
        let (last, fresh) = {
            let talk = self.imp().talk.borrow();
            let talk = talk.as_ref().expect("the window exists");
            (talk.last.clone(), talk.fresh)
        };
        let minutes = self.imp().settings.borrow().voice_follow_up;
        let going = if fresh {
            spoken::Going::Fresh
        } else {
            spoken::continuation(last.as_ref(), chrono::Utc::now(), minutes)
        };

        let source = self.imp().settings.borrow().voice_source.clone();
        // The microphone may already be open: it stays open for the whole
        // exchange so it can be interrupted, and carrying a conversation on
        // means picking the same stream back up rather than spawning a second
        // `pw-record` beside the first.
        let already_open = self
            .imp()
            .talk
            .borrow()
            .as_ref()
            .is_some_and(|talk| talk.recorder.is_some());
        let started = {
            let mut talk = self.imp().talk.borrow_mut();
            let talk = talk.as_mut().expect("the window exists");
            if already_open {
                if let Some(recorder) = talk.recorder.as_ref() {
                    let _ = recorder.take();
                }
                talk.spoken = crate::model::voice::Spoken::default();
                talk.endpointer = crate::model::voice::Endpointer::default();
                talk.barge = crate::model::voice::Barge::default();
                talk.pending.clear();
                talk.live.clear();
                talk.answer.clear();
                talk.reading = crate::model::voice::Reading::default();
            } else {
                talk.clear();
            }
            talk.carrying_on = carrying_on;
            talk.waiting_ms = 0;
            talk.chat = match &going {
                spoken::Going::On { id, title } => Some(spoken::Recent {
                    id: id.clone(),
                    title: title.clone(),
                    spoke_at: chrono::Utc::now(),
                }),
                spoken::Going::Fresh => None,
            };
            if already_open {
                None
            } else {
                Some(Recorder::start(
                    &source,
                    clone!(
                        #[weak(rename_to = app)]
                        self,
                        move |block: &[f32]| app.heard_block(block)
                    ),
                    clone!(
                        #[weak(rename_to = app)]
                        self,
                        move |reason: String| {
                            crate::voice_log!("the microphone stopped: {reason}");
                            app.recover_voice(&reason);
                        }
                    ),
                ))
            }
        };

        window.reset();
        window.set_chat(match &going {
            spoken::Going::On { title, .. } => Some(title.as_str()),
            spoken::Going::Fresh => None,
        });

        crate::voice_log!(
            "listening on {} (carrying on: {carrying_on}, microphone already open: {already_open})",
            if source.is_empty() {
                "the system default microphone"
            } else {
                source.as_str()
            }
        );
        match started {
            None => {
                self.speech().reset();
                window.set_state(State::Listening);
                self.guard_against_a_deaf_microphone();
            }
            Some(Ok(recorder)) => {
                self.speech().reset();
                if let Some(talk) = self.imp().talk.borrow_mut().as_mut() {
                    talk.recorder = Some(recorder);
                }
                window.set_state(State::Listening);
                self.guard_against_a_deaf_microphone();
            }
            Some(Err(error)) => {
                self.go_idle();
                window.set_trouble(&error.to_string());
            }
        }
    }

    /// Give up if no audio arrives at all.
    ///
    /// `Recorder` reports a pipe that closes, but a `pw-record` that stays
    /// alive and delivers nothing — a muted device, a source that exists but
    /// produces no frames — closes nothing and reports nothing. Every way out
    /// of `Listening` is driven by a block of audio arriving, including the
    /// endpointer's own patience, so with no blocks there is no way out at all
    /// and the window listens until the user notices.
    fn guard_against_a_deaf_microphone(&self) {
        glib::timeout_add_local_once(
            DEAF_MICROPHONE,
            clone!(
                #[weak(rename_to = app)]
                self,
                move || {
                    if app.voice_state() != State::Listening {
                        return;
                    }
                    let heard_anything = app
                        .imp()
                        .talk
                        .borrow()
                        .as_ref()
                        .is_some_and(|talk| talk.endpointer.elapsed_ms() > 0);
                    if heard_anything {
                        return;
                    }
                    crate::voice_log!(
                        "no audio at all after {} ms — giving up",
                        DEAF_MICROPHONE.as_millis()
                    );
                    app.recover_voice(
                        "No audio arrived from the microphone. Check the input device in \
                         Preferences.",
                    );
                }
            ),
        );
    }

    /// End the exchange: nothing running, nothing listening, microphone shut.
    ///
    /// The one place the microphone is closed. It is open from the first press
    /// until here, so the panel's indicator is lit for a conversation rather
    /// than for a whole session — and so there is always something listening
    /// while there is something to interrupt.
    fn go_idle(&self) {
        if let Some(talk) = self.imp().talk.borrow_mut().as_mut() {
            if let Some(recorder) = talk.recorder.take() {
                crate::voice_log!("closing the microphone");
                let _ = recorder.finish();
            }
        }
        if let Some(window) = self.voice_window() {
            window.set_state(State::Idle);
        }
    }

    /// One block of audio, forty milliseconds of it.
    ///
    /// The microphone is open for the whole exchange, so what a block means
    /// depends on what is happening: while listening it is the question, and
    /// while thinking or speaking it is only ever watched for somebody
    /// starting to talk over the top.
    fn heard_block(&self, block: &[f32]) {
        let level = voice::recorder::level(block);
        // How much audio this actually is, rather than how much was asked for.
        // A pipe read returns what it has, which is often less than a whole
        // block — and counting every read as a full one runs the endpointer's
        // clock at nearly twice real time. Its patience then expires while
        // somebody is still talking, and what they get is "I did not hear
        // anything" said over the top of them.
        let span_ms = (block.len() as u32 * 1_000)
            .div_euclid(voice::recorder::SAMPLE_RATE)
            .max(1);
        {
            let mut talk = self.imp().talk.borrow_mut();
            if let Some(talk) = talk.as_mut() {
                talk.blocks = talk.blocks.wrapping_add(1);
                if talk.blocks % 25 == 0 {
                    let state = talk.window.state();
                    crate::voice_log!("block {} in {state:?}", talk.blocks);
                }
            }
        }
        match self.voice_state() {
            State::Listening => self.heard_while_listening(block, level, span_ms),
            // Not while transcribing. That quarter-second sits directly after
            // somebody stopped talking, which is exactly where their own
            // trailing breath is, and interrupting a pass that has not
            // produced a question yet gains nothing but loses the question.
            State::Transcribing => self.still_waiting(TRANSCRIBE_PATIENCE, span_ms),
            State::Thinking => {
                // Nothing is running, and nothing is going to take this out of
                // Thinking. Every hole that has caused it — a turn dropped for
                // want of a window, a server that was never configured, a
                // speech worker that answered nobody — has been closed one at
                // a time, and each of them looked identical from here: heard,
                // and then ignored. This is the backstop, and it costs one
                // comparison per block.
                if self.imp().in_flight.borrow().is_none() {
                    self.recover_voice("That question did not get through. Try it again.");
                    return;
                }
                self.still_waiting(THINK_PATIENCE, span_ms);
                if self.listen_for_interruption(level, span_ms, "thinking") {
                    self.interrupted();
                }
            }
            State::Speaking => {
                // The same backstop as thinking: nothing but the speaker
                // falling silent takes the window out of this state, and a
                // missed announcement would strand it with the microphone
                // open and no way back.
                self.still_waiting(THINK_PATIENCE, span_ms);
                if self.listen_for_interruption(level, span_ms, "speaking") {
                    self.interrupted();
                }
            }
            State::Idle => {}
        }
    }

    /// A block while the question is being asked.
    fn heard_while_listening(&self, block: &[f32], level: f64, span_ms: u32) {
        // Whether there is a model that can say "those were words". Without
        // one there is nothing but loudness to go on.
        let listening_for_words = voice::speech::model_dir(voice::speech::Model::Live).is_some();
        let (heard, chunks) = {
            let mut talk = self.imp().talk.borrow_mut();
            let Some(talk) = talk.as_mut() else {
                return;
            };
            if talk.recorder.is_none() {
                return;
            }
            talk.window.hear(level);
            talk.pending.extend_from_slice(block);
            let mut chunks: Vec<Vec<f32>> = Vec::new();
            while talk.pending.len() >= voice::speech::STREAM_CHUNK {
                chunks.push(
                    talk.pending
                        .drain(..voice::speech::STREAM_CHUNK)
                        .collect::<Vec<f32>>(),
                );
            }
            let before = talk.endpointer.elapsed_ms();
            // Both are always run: the loudness one draws the meter and is the
            // fallback, and the word one is the decision when there is a live
            // model to make it with.
            let by_loudness = talk.endpointer.push(level, span_ms);
            let by_words = talk.spoken.push(span_ms);
            let heard = if listening_for_words {
                by_words
            } else {
                by_loudness
            };
            if talk.endpointer.elapsed_ms() / 1_000 != before / 1_000 {
                let (speech, silence) = talk.endpointer.tally();
                crate::voice_log!(
                    "{} ms: peak {:.2} against gate {:.2} — {speech} ms over it, {silence} ms \
                     under; words {}, quiet for {} ms — {heard:?}",
                    talk.endpointer.elapsed_ms(),
                    talk.endpointer.peak(),
                    talk.endpointer.gate(),
                    talk.spoken.has_words(),
                    talk.spoken.quiet_for()
                );
            }
            (heard, chunks)
        };

        // The live model, for the words on screen. Only when there is a live
        // model: with only the accurate one installed there is no preview, and
        // that is a missing nicety rather than a broken feature.
        if voice::speech::model_dir(voice::speech::Model::Live).is_some() {
            for chunk in chunks {
                self.speech().feed(
                    chunk,
                    clone!(
                        #[weak(rename_to = app)]
                        self,
                        move |result| {
                            if let Ok(text) = result {
                                app.heard_words(&text);
                            }
                        }
                    ),
                );
            }
        }

        match heard {
            spoken::Heard::Ended | spoken::Heard::Overlong => self.stop_listening(true),
            spoken::Heard::NothingSaid => self.stop_listening(false),
            spoken::Heard::Quiet | spoken::Heard::Speaking => {}
        }
    }

    /// Watch one block for somebody talking over the top.
    ///
    /// The trace is what settles an argument about the threshold. Whether a
    /// voice cleared the bar is not a thing anybody can tell by listening —
    /// the only answer to "it did not pick me up" that is worth having is the
    /// level, the bar, and how close it came.
    fn listen_for_interruption(&self, level: f64, span_ms: u32, during: &str) -> bool {
        let mut talk = self.imp().talk.borrow_mut();
        let Some(talk) = talk.as_mut() else {
            return false;
        };
        let before = talk.barge.elapsed_ms();
        let interrupted = talk.barge.push(level, span_ms);
        // Off the barge's own clock, not the window's: only some of these
        // states advance the window's, so rate-limiting on it would log every
        // block in one state and none in another.
        if talk.barge.elapsed_ms() / 1_000 != before / 1_000 || interrupted {
            crate::voice_log!(
                "{during}: level {level:.2} against {:.2}, {:.0}% of the way to interrupting{}",
                talk.barge.threshold(),
                talk.barge.nearly() * 100.0,
                if interrupted { " — interrupted" } else { "" }
            );
        }
        interrupted
    }

    /// Count a block spent waiting, and give up if it has been far too long.
    fn still_waiting(&self, patience_ms: u32, span_ms: u32) {
        let over = {
            let mut talk = self.imp().talk.borrow_mut();
            let Some(talk) = talk.as_mut() else {
                return;
            };
            talk.waiting_ms = talk.waiting_ms.saturating_add(span_ms);
            talk.waiting_ms > patience_ms
        };
        if over {
            self.recover_voice("That took too long to come back. Try it again.");
        }
    }

    /// Put the window back somewhere a person can use it.
    fn recover_voice(&self, trouble: &str) {
        self.speaker().hush();
        self.stop();
        self.abandon();
        self.go_idle();
        if let Some(window) = self.voice_window() {
            window.set_trouble(trouble);
        }
    }

    /// Somebody started talking over it.
    ///
    /// Stop the answer, throw away the turn if one is still running, and take
    /// what they are saying — from a moment before the interruption was
    /// certain, so their first word is not the price of interrupting.
    fn interrupted(&self) {
        self.speaker().hush();
        let running = self
            .imp()
            .in_flight
            .borrow()
            .as_ref()
            .is_some_and(|turn| turn.spoken);
        if running {
            self.stop();
            self.abandon();
        }

        let Some(window) = self.voice_window() else {
            return;
        };
        {
            let mut talk = self.imp().talk.borrow_mut();
            let Some(talk) = talk.as_mut() else {
                return;
            };
            // Everything before this was the room, or the assistant's own
            // voice coming back off the speakers. Half a second is about the
            // length of the trigger plus the word that caused it.
            if let Some(recorder) = talk.recorder.as_ref() {
                recorder.keep_last(voice::recorder::SAMPLE_RATE as usize / 2);
            }
            talk.spoken = crate::model::voice::Spoken::default();
            talk.endpointer = crate::model::voice::Endpointer::default();
            talk.barge = crate::model::voice::Barge::default();
            talk.pending.clear();
            talk.live.clear();
            talk.answer.clear();
            talk.reading = crate::model::voice::Reading::default();
            talk.carrying_on = true;
            talk.waiting_ms = 0;
        }
        window.reset();
        // Talking over it arrives *through* an open microphone, so there is one
        // to carry on with. A press of Stop does not, and setting the window to
        // Listening with nothing capturing would leave it listening at a level of
        // zero for ever — nothing advances the state but a block of audio.
        let open = self
            .imp()
            .talk
            .borrow()
            .as_ref()
            .is_some_and(|talk| talk.recorder.is_some());
        if !open {
            self.start_listening();
            return;
        }
        window.set_state(State::Listening);
        self.speech().reset();
    }

    /// What the live model has made of the audio so far. Feedback, not the
    /// question: the accurate pass is what gets asked.
    fn heard_words(&self, words: &str) {
        let mut talk = self.imp().talk.borrow_mut();
        let Some(talk) = talk.as_mut() else {
            return;
        };
        // Only while still listening. A late chunk arriving after the utterance
        // was transcribed would overwrite the accurate text with the rough one.
        if talk.recorder.is_none() {
            return;
        }
        // Words end an utterance, but only words the microphone can account
        // for. The streaming model transcribes whatever it can hear — a video
        // playing across the room, or nothing at all — and anything credited to
        // the speaker resets the clock, so crediting the room means the
        // microphone never closes. See `voice::Endpointer::heard_you`, which is
        // measured against this room rather than assumed about it.
        if !words.trim().is_empty() && !talk.spoken.words_if_heard(talk.endpointer.heard_you()) {
            let (_, silence_ms) = talk.endpointer.tally();
            crate::voice_log!(
                "ignoring {:?}: too quiet to be you ({silence_ms} ms since anything was, \
                 gate {:.2})",
                words.trim().chars().take(24).collect::<String>(),
                talk.endpointer.gate()
            );
            return;
        }
        // Appended exactly as it came. The model puts its own spaces at the
        // front of the words it starts, so trimming each chunk and joining
        // with a space of our own is what turns "testing" into "test ing"
        // whenever a word happens to straddle two 560 ms chunks.
        talk.live.push_str(words);
        let live = talk.live.trim().to_string();
        talk.window.set_heard(&live, false);
    }

    /// Close the microphone. `keep` is false when nothing was said.
    fn stop_listening(&self, keep: bool) {
        let taken = {
            let mut talk = self.imp().talk.borrow_mut();
            let Some(talk) = talk.as_mut() else {
                return;
            };
            talk.recorder
                .as_ref()
                .map(|recorder| (recorder.take(), talk.carrying_on))
        };
        let Some((samples, carrying_on)) = taken else {
            return;
        };
        let Some(window) = self.voice_window() else {
            return;
        };

        // Half a second of audio is the only thing asked of an explicit send.
        // What the endpointer made of it is deliberately not consulted here:
        // pressing Send is a person saying they spoke, and a gate that
        // overrules them is a gate that loses the question. Its opinion
        // decides when to stop listening, which is the only thing it is good
        // at. `voice::is_a_question` catches what is left.
        crate::voice_log!(
            "stopped listening: keep {keep}, {} samples ({:.1}s)",
            samples.len(),
            samples.len() as f64 / f64::from(voice::recorder::SAMPLE_RATE)
        );
        let enough = samples.len() >= voice::recorder::SAMPLE_RATE as usize / 2;
        if !keep || !enough {
            self.go_idle();
            if !carrying_on {
                window.set_heard("", false);
                window.set_trouble("I did not hear anything. Press the shortcut and speak.");
            }
            return;
        }

        if let Some(talk) = self.imp().talk.borrow_mut().as_mut() {
            talk.waiting_ms = 0;
        }
        self.flush_live_words();
        window.set_state(State::Transcribing);
        self.speech().transcribe(
            samples,
            clone!(
                #[weak(rename_to = app)]
                self,
                move |result| app.transcribed(result)
            ),
        );
    }

    /// Put the last part-chunk of audio through the live model.
    ///
    /// The streaming encoder decodes whole 560 ms chunks and emits nothing at
    /// all until it has one, so when somebody stops talking there is up to half
    /// a second of speech in `pending` that has never been through it — and the
    /// last thing anybody said is exactly what is in there. Padding the
    /// remainder out to a whole chunk and sending it is what completes the
    /// sentence on screen.
    ///
    /// The words are feedback only; the accurate pass over the whole buffer is
    /// what gets asked. But the live text is also the fallback when that pass
    /// fails, and a transcript that visibly stops a word early reads as not
    /// having been heard. Scribe had the same hole, where it took the end off
    /// every dictation rather than off a preview.
    fn flush_live_words(&self) {
        if voice::speech::model_dir(voice::speech::Model::Live).is_none() {
            return;
        }
        let remainder = {
            let mut talk = self.imp().talk.borrow_mut();
            let Some(talk) = talk.as_mut() else {
                return;
            };
            let mut remainder = std::mem::take(&mut talk.pending);
            if remainder.is_empty() {
                return;
            }
            // The model returns nothing from a part-chunk, so silence makes up
            // the difference rather than the words being dropped.
            remainder.resize(voice::speech::STREAM_CHUNK, 0.0);
            remainder
        };
        self.speech().feed(
            remainder,
            clone!(
                #[weak(rename_to = app)]
                self,
                move |result| {
                    let Ok(text) = result else {
                        return;
                    };
                    if text.trim().is_empty() {
                        return;
                    }
                    let mut talk = app.imp().talk.borrow_mut();
                    let Some(talk) = talk.as_mut() else {
                        return;
                    };
                    talk.live.push_str(&text);
                    let live = talk.live.trim().to_string();
                    talk.window.set_heard(&live, false);
                }
            ),
        );
    }

    /// The accurate pass came back.
    fn transcribed(&self, result: Result<String, voice::speech::SpeechError>) {
        let Some(window) = self.voice_window() else {
            return;
        };
        // Cancelled while it was running. The words are no longer wanted.
        if window.state() != State::Transcribing {
            return;
        }
        // What the live model made of it, as a fallback. The accurate pass is
        // better and is what is used when it has anything to say — but words
        // on screen and "I did not catch that" underneath them is the
        // application calling the person a liar about something they just
        // watched happen.
        let live = self
            .imp()
            .talk
            .borrow()
            .as_ref()
            .map(|talk| talk.live.trim().to_string())
            .unwrap_or_default();
        crate::voice_log!("transcribed: {result:?} (live text: {live:?})");

        let question = match result {
            Ok(transcript) if spoken::is_a_question(&transcript) => transcript.trim().to_string(),
            Ok(_) if spoken::is_a_question(&live) => live,
            Err(error) if spoken::is_a_question(&live) => {
                // Worth a line in the log rather than the window: the fallback
                // is good enough that a person need not be told the better one
                // failed.
                eprintln!("familiar: the accurate pass failed, using the live text: {error}");
                live
            }
            Ok(_) => {
                self.go_idle();
                window.set_heard("", false);
                window.set_trouble("I did not catch that.");
                return;
            }
            Err(error) => {
                self.go_idle();
                window.set_trouble(&voice::speech::trouble(&error));
                return;
            }
        };

        window.set_heard(&question, true);
        self.ask_aloud(&question);
    }

    /// Put a spoken question to the model.
    ///
    /// It runs the background path — no turn widget, no composer, a chat that
    /// is usually not the open one — because the shortcut works with the main
    /// window closed and a turn that needs a window would either fail there or
    /// raise one over whatever the user is doing. Everything else about it is
    /// an ordinary turn, `scheduled` included: a person did ask, so the answer
    /// is not announced by notification and the passive reader does read it.
    fn ask_aloud(&self, question: &str) {
        let Some(window) = self.voice_window() else {
            return;
        };
        if self.imp().in_flight.borrow().is_some() {
            self.go_idle();
            window.set_trouble("Wait for the answer that is already running to finish.");
            return;
        }
        let Some((slug, project, thread)) = self.voice_chat() else {
            self.go_idle();
            window.set_trouble("There is nowhere to put this — no project could be opened.");
            return;
        };

        // Remembered now rather than at settle: a turn that fails still
        // happened in this chat, and the follow-up should carry on with it.
        let recent = spoken::Recent {
            id: thread.id.to_string(),
            title: thread.display_title(),
            spoke_at: chrono::Utc::now(),
        };
        // A chat with no turns in it yet is titled "New Chat", and "Carrying on
        // “New Chat”" is a sentence about nothing. Until it has a name of its
        // own the header says it is a new one, which is what it is.
        let named = !thread.is_empty();
        if let Some(talk) = self.imp().talk.borrow_mut().as_mut() {
            talk.chat = Some(recent.clone());
            talk.last = Some(recent.clone());
            talk.fresh = false;
        }
        window.set_chat(named.then_some(recent.title.as_str()));
        if let Some(talk) = self.imp().talk.borrow_mut().as_mut() {
            talk.waiting_ms = 0;
        }
        window.set_state(State::Thinking);

        // This answer may be read out. Every path that stops the reading mutes
        // the speaker and nothing un-mutes on its own, so a new question is the
        // one place that lifts it — which is what keeps a stop stopped while the
        // last answer is still arriving.
        self.speaker().allow();

        crate::voice_log!("asking: {question:?}");
        self.imp().in_flight.replace(Some(InFlight {
            question: question.to_string(),
            documents: Vec::new(),
            images: Vec::new(),
            stream: TurnStream::new(),
            session: Session {
                slug,
                project,
                chat: Chat::Background(Box::new(thread)),
                view: None,
            },
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
            scheduled: false,
            job: None,
            spoken: true,
        }));
        self.ask(None);
    }

    /// The chat a spoken question goes into: the one being carried on, or a new
    /// one in whichever project is open.
    fn voice_chat(&self) -> Option<(String, Project, Thread)> {
        let store = self.store()?;
        let project = self.imp().project.borrow().clone();
        let slug = project.slug.clone();
        let carrying = self
            .imp()
            .talk
            .borrow()
            .as_ref()
            .and_then(|talk| talk.chat.clone());

        if let Some(recent) = carrying {
            if let Some(id) = ThreadId::from_stem(&recent.id) {
                // The open chat's in-memory copy wins, exactly as a scheduled
                // run's does: the file may be a turn behind what is on screen.
                let open = self.imp().thread.borrow().id == id;
                let thread = if open {
                    Some(self.imp().thread.borrow().clone())
                } else {
                    store.load_thread(&slug, &id).ok()
                };
                if let Some(thread) = thread {
                    return Some((slug, project, thread));
                }
            }
        }
        let thread = store.new_thread(&slug).ok()?;
        crate::voice_log!("new chat {} in {slug}", thread.id);
        Some((slug, project, thread))
    }

    /// A piece of the answer arrived.
    fn voice_delta(&self, delta: &str) {
        let sentences = {
            let mut talk = self.imp().talk.borrow_mut();
            let Some(talk) = talk.as_mut() else {
                return;
            };
            talk.answer.push_str(delta);
            let answer = talk.answer.clone();
            talk.window.set_answer(&answer);
            talk.reading.push(delta)
        };
        let speaker = self.speaker();
        for sentence in sentences {
            speaker.say(&sentence);
        }
    }

    /// The turn is over. Say the tail of it and go quiet.
    fn voice_settled(&self, answer: &str) {
        let Some(window) = self.voice_window() else {
            return;
        };
        let (tail, skipped) = {
            let mut talk = self.imp().talk.borrow_mut();
            let Some(talk) = talk.as_mut() else {
                return;
            };
            // The stream is the truth while it runs, but a turn can settle with
            // an answer the deltas never carried — a recovered tool call, a
            // failure message — so the settled text wins at the end.
            if talk.answer.trim().is_empty() && !answer.trim().is_empty() {
                talk.answer = answer.to_string();
                talk.window.set_answer(answer);
                talk.reading.push(answer);
            }
            (talk.reading.flush(), talk.reading.skipped_code())
        };

        let speaker = self.speaker();
        if let Some(tail) = tail {
            speaker.say(&tail);
        }
        if skipped {
            // Said rather than read out: code aloud is noise, and silently
            // dropping it would leave somebody waiting for an answer that
            // already went past.
            speaker.say("The rest is code — it is in the chat.");
        }
        // `is_busy`, not `is_speaking`: a sentence being fetched is an answer
        // that is not over, and treating it as over means listening through
        // the rest of it and taking the assistant's own voice for a question.
        if speaker.is_busy() {
            // Say so explicitly rather than waiting to be told. The
            // announcement that noise has started arrives a fetch later, and
            // until it does this is the state the exchange is actually in —
            // which is what decides whether the microphone is watched for an
            // interruption or read as a question.
            window.set_state(State::Speaking);
            self.resettle_barge();
            crate::voice_log!("speaking the answer");
            return;
        }
        window.set_state(State::Idle);
        self.carry_on_listening();
        // Nothing carried on, so the exchange is over and the microphone has
        // no reason to stay open.
        if self.voice_state() == State::Idle {
            self.go_idle();
        }
    }

    /// The synthesiser started or stopped.
    fn voice_is_speaking(&self, speaking: bool) {
        let Some(window) = self.voice_window() else {
            return;
        };
        match (speaking, window.state()) {
            (true, State::Thinking | State::Speaking) => {
                let starting = window.state() != State::Speaking;
                window.set_state(State::Speaking);
                if starting {
                    self.resettle_barge();
                }
            }
            (false, State::Speaking) => {
                // Only when there is nothing left to say. The speaker falls
                // quiet between two sentences, and going back to listening
                // there would cut the answer in half — `listen` hushes it.
                if self.speaker().is_busy() {
                    return;
                }
                crate::voice_log!("finished speaking");
                window.set_state(State::Idle);
                self.carry_on_listening();
                if self.voice_state() == State::Idle {
                    self.go_idle();
                }
            }
            _ => {}
        }
    }

    /// Learn the background again, now that what it is hearing has changed.
    ///
    /// `Barge` measures what is already there during a settle window and then
    /// only ever revises it *down*. That is right within one phase and wrong
    /// across two: the window it settled in was the room while the model was
    /// thinking, and what arrives next is the assistant's own voice off the
    /// speakers. Without this the bar stayed where a quiet room put it — and on
    /// this desk a silent room peaks at 0.385, so it stayed above the median of
    /// ordinary speech and interrupting took a raised voice and several seconds.
    ///
    /// The cost is that the first `settle_ms` of an answer cannot be interrupted,
    /// which is not much of a cost: there is nothing to interrupt yet.
    fn resettle_barge(&self) {
        if let Some(talk) = self.imp().talk.borrow_mut().as_mut() {
            talk.barge = crate::model::voice::Barge::default();
        }
    }

    /// Throw this exchange away: stop listening, stop thinking, stop talking.
    fn cancel_voice(&self) {
        self.speaker().hush();
        let running = self
            .imp()
            .in_flight
            .borrow()
            .as_ref()
            .is_some_and(|turn| turn.spoken);
        if running {
            // Cancel first, for the ordinary case where a request is in
            // flight and the transport reports the cancellation. Then abandon,
            // because a turn can be stranded with nothing in flight to cancel
            // — and one left in the slot refuses every question after it with
            // "wait for the answer that is already running". `abandon` is a
            // no-op if `stop` has already settled the turn.
            self.stop();
            self.abandon();
        }
        if let Some(talk) = self.imp().talk.borrow_mut().as_mut() {
            talk.clear();
        }
        self.go_idle();
        if let Some(window) = self.voice_window() {
            window.reset();
        }
    }

    /// Put the next question in a chat of its own.
    fn fresh_voice_chat(&self) {
        if let Some(talk) = self.imp().talk.borrow_mut().as_mut() {
            talk.fresh = true;
            talk.chat = None;
        }
        if let Some(window) = self.voice_window() {
            window.set_chat(None);
        }
        // Already listening: the decision was made when the microphone opened,
        // so make it again rather than leaving the header saying one thing and
        // the exchange doing another.
        if self.voice_state() == State::Listening {
            self.stop_listening(false);
            self.start_listening();
        }
    }

    /// Show the chat this exchange went into, in the main window.
    fn show_voice_chat(&self) {
        let carrying = self
            .imp()
            .talk
            .borrow()
            .as_ref()
            .and_then(|talk| talk.chat.clone());
        let Some(recent) = carrying else { return };
        let slug = self.imp().project.borrow().slug.clone();
        self.activate();
        self.open_thread(&slug, &recent.id);
        if let Some(window) = self.window() {
            window.present();
        }
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
            session: Session {
                slug: self.imp().project.borrow().slug.clone(),
                project: self.imp().project.borrow().clone(),
                chat: Chat::Open,
                view: Some(view),
            },
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
            // A person typing is not a run of anything.
            job: None,
            spoken: false,
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
            // No server to ask. This used to return with the turn still in its
            // slot, which stops the next question being sent at all — and on
            // the spoken path there is no composer to look busy, so what it
            // looks like is being heard and then ignored.
            self.abandon();
            if let Some(window) = self.voice_window() {
                if window.state() != State::Idle {
                    self.go_idle();
                    window.set_trouble(
                        "There is no model server to ask. Familiar looks for one at the address \
                         in Preferences.",
                    );
                }
            }
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
        let mut in_flight = self.imp().in_flight.borrow_mut();
        let Some(turn) = in_flight.as_mut() else {
            return;
        };
        // Pushed before anything is drawn, and whether or not there is
        // anything to draw on. The stream is the turn's own state rather than
        // a display detail, and a background run has neither a view nor a
        // window — this used to return early without one, which would have
        // stalled the turn rather than run it silently.
        let events = turn.stream.push(text);
        let view = turn.session.view.clone();
        let calls = turn.stream.state().tool_calls.clone();
        let settled = turn.calls.clone();
        let spoken = turn.spoken;
        // The borrow is dropped before the widgets are touched: a handler that
        // re-enters the application would otherwise find the RefCell held.
        drop(in_flight);

        // Before the view guard, not after: a spoken turn has no view, and this
        // is the only thing watching it.
        if spoken {
            for event in &events {
                if let Event::Answer(delta) = event {
                    self.voice_delta(delta);
                }
            }
        }

        let Some(view) = view else {
            return;
        };
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
            if let Some(window) = self.window() {
                window.conversation().follow();
            }
        }
    }

    fn on_finished(&self, outcome: Result<(), ClientError>) {
        // No window guard. A background run finishes with nothing on screen,
        // and returning early here would leave the turn taken out of its cell
        // and never settled — the answer lost and the schedule marked as
        // having run. Every use of the window below is conditional instead.
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
                if let Some(view) = &turn.session.view {
                    view.set_failure(Some(&error.to_string()));
                }
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
            if let Some(view) = &turn.session.view {
                view.set_failure(Some("Stopped after too many tool calls in one turn."));
            }
        }

        self.settle_turn(turn, state, self.window().as_ref());
    }

    /// Run each call the model asked for, pausing at the gate when one needs
    /// approval, then hand the results back and continue the turn.
    fn run_tools(&self, calls: Vec<ToolCall>, state: TurnState) {
        let gated = calls
            .iter()
            .find(|call| tools::gate_for(&call.name, &call.arguments) == tools::Gate::Always)
            .cloned();

        let Some(gated) = gated else {
            // Nothing to ask about, so nothing needs a window. This used to
            // return early when the main window was closed, which stranded the
            // turn: `in_flight` was never settled, and a spoken question that
            // reached for the weather sat at "Thinking" until the app was
            // quit. Every other window use in the turn path is conditional —
            // this one was not.
            self.run_all(calls, state);
            return;
        };

        // Somewhere to put the dialog. The main window if there is one, and
        // the voice window if the question was spoken with the main window
        // closed — which is the ordinary way a spoken question arrives.
        let parent: Option<gtk::Widget> = self
            .window()
            .map(|window| window.upcast())
            .or_else(|| self.voice_window().map(|window| window.upcast()));
        let Some(parent) = parent else {
            // No window at all: a scheduled run in the background. A gate
            // cannot be answered by nobody, so it is refused rather than
            // silently run, and the turn carries on with the refusal.
            let settled = calls
                .into_iter()
                .map(|mut call| {
                    if call.id == gated.id {
                        call.outcome = Some(ToolOutcome::Denied);
                    }
                    call
                })
                .collect();
            self.run_all(settled, state);
            return;
        };

        // One dialog at a time: the rest of the round waits behind it.
        approval::ask(
            &parent,
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

    fn abandon(&self) {
        let Some(mut turn) = self.imp().in_flight.borrow_mut().take() else {
            return;
        };
        let state = TurnState {
            thinking: turn.thinking.clone(),
            answer: turn.answer.clone(),
            tool_calls: turn.calls.clone(),
            finish: Some(crate::model::turn::Finish::Cancelled),
            ..Default::default()
        };
        if let Some(view) = &turn.session.view {
            view.settle(&state);
        }
        // What was said still has to be kept whether or not there is a window
        // to unbusy — the record is the point, the composer is decoration.
        if let Some(window) = self.window() {
            window.composer().set_busy(false);
        }
        self.record(&mut turn.session, &turn.question, &state, &turn.images);
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
    fn settle_turn(&self, mut turn: InFlight, state: TurnState, window: Option<&Window>) {
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
            if let Some(view) = &turn.session.view {
                view.set_failure(Some(
                    "The model finished without answering — it reasoned, but its reply did not \
                     survive the server's parsing. Asking again usually works.",
                ));
            }
        }
        if let Some(view) = &turn.session.view {
            view.settle(&settled);
        }
        if turn.spoken {
            self.voice_settled(&settled.answer);
        }
        self.draw_chips(&turn.calls, &[]);
        if let Some(window) = window {
            window.composer().set_busy(false);
            window.conversation().follow();
        }
        crate::voice_log!(
            "settling: answer {} chars, empty {}",
            settled.answer.len(),
            settled.is_empty()
        );
        self.record(&mut turn.session, &turn.question, &settled, &turn.images);
        // For whichever chat this turn belonged to. A scheduled chat grows a
        // turn a day and is exactly the kind that needs folding, so skipping it
        // would have meant the threads most in need of compaction were the only
        // ones that never got it.
        let (slug, id) = self.session_chat(&mut turn.session);
        self.fold_if_needed(&slug, &id);
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
            // Through the session rather than the slot: a scheduled run's
            // outcome belongs to the chat that was scheduled, which is usually
            // not the one on screen.
            let (id, title) = self.with_session_thread(&mut turn.session, |thread| {
                (thread.id.to_string(), thread.display_title())
            });
            // Against the job rather than the chat: the job outlives every
            // conversation it writes into, and two jobs may share a chat, so
            // "what happened last time" is only answerable per job.
            if let Some(name) = &turn.job {
                if let Some(job) = self.imp().jobs.borrow_mut().get_mut(name) {
                    job.last_outcome = Some(summary.clone());
                }
                self.save_jobs();
            }
            self.save_session(&mut turn.session);
            self.announce_run(&id, &title, &summary);
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

    fn set_schedule(
        &self,
        action: &str,
        when: &str,
        prompt: &str,
        title: &str,
    ) -> Result<String, String> {
        use crate::model::heartbeat;
        use crate::model::jobs::{Destination, Job, Source};

        let (slug, thread) = self.turn_chat();

        match action {
            "show" | "list" => {
                let jobs = self.imp().jobs.borrow();
                let mine: Vec<String> = jobs
                    .for_chat(&slug, &thread)
                    .map(|job| {
                        format!(
                            "{} — runs {} and asks: {:?}{}",
                            job.title(),
                            job.schedule.describe().to_lowercase(),
                            job.prompt,
                            if job.enabled { "" } else { " (paused)" }
                        )
                    })
                    .collect();
                return Ok(if mine.is_empty() {
                    "This chat has no schedule.".to_string()
                } else {
                    // Plural, because a chat may now carry several and saying
                    // "this chat runs …" would be a half-truth the model would
                    // repeat to the user.
                    format!(
                        "This chat runs {} scheduled job(s):\n{}",
                        mine.len(),
                        mine.join("\n")
                    )
                });
            }
            "clear" | "stop" | "remove" | "cancel" => {
                let dropped = self.imp().jobs.borrow_mut().forget_chat(&slug, &thread);
                self.save_jobs();
                self.refresh_threads();
                return Ok(match dropped {
                    0 => "This chat had no schedule, so nothing changed.".to_string(),
                    1 => "Stopped. This chat no longer runs on its own.".to_string(),
                    many => format!("Stopped all {many} of this chat's schedules."),
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

        let mut job = Job::new(
            "",
            schedule,
            prompt,
            Destination::Chat {
                slug: slug.clone(),
                thread: thread.clone(),
            },
        );
        // Provenance, not ownership: the user may still edit it. Worth keeping
        // because "why is this running?" is a real question weeks later.
        job.source = Source::Agent;
        // The clock starts now rather than at the epoch, or the first tick
        // would fire every occurrence since 1970.
        job.last_run = Some(chrono::Utc::now());
        let named = crate::model::thread::tidy_title(title);
        if let Some(named) = &named {
            job.name = named.clone();
        }
        let next = schedule.first_after(chrono::Local::now());
        self.imp().jobs.borrow_mut().add(job, chrono::Utc::now());
        self.save_jobs();

        // The chat is named too when nothing has named it, because a scheduled
        // chat is the one kind you go looking for weeks later in a list, and
        // its name would otherwise be the first line of however the
        // conversation happened to open. Only when nothing has named it: a
        // title the user typed under Rename Chat is theirs.
        self.with_turn_thread(|thread| {
            let unnamed = thread
                .title
                .as_ref()
                .map_or(true, |had| had.trim().is_empty());
            if unnamed {
                thread.title.clone_from(&named);
            }
        });
        // An empty chat is not written, and a schedule pointing at an unwritten
        // chat is a schedule with nowhere to land.
        self.save_turn_thread();
        self.refresh_threads();
        if let Some(window) = self.window() {
            window.toast(&format!(
                "Scheduled — {}",
                schedule.describe().to_lowercase()
            ));
        }
        Ok(format!(
            "Set: this chat will run {} on its own, starting {}{}. Tell the user it is set up, \
             what it will do, and that they can pause or remove it under Scheduled Chats. A chat \
             may carry more than one schedule, so this did not replace any it already had.",
            schedule.describe().to_lowercase(),
            next.format("%A %-d %B at %H:%M"),
            match named {
                Some(named) => format!(", and this job is called {named:?}"),
                None => ", and this job has no name of its own — pass `title` next time so it is \
                     findable under Scheduled Chats"
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
                self.with_turn_thread(|thread| {
                    if let Some(open) = thread.workflow.as_mut() {
                        open.saved_as = Some(crate::model::project::slugify(&flow.goal));
                    }
                });
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
                self.with_turn_thread(|thread| thread.workflow = Some(found));
                said
            }
            _ => {
                // Taken out and put back rather than held across the call: the
                // borrow would still be live when `refresh_workflow` redraws,
                // and a handler that re-entered the application would find the
                // RefCell held. Every other borrow here keeps the same rule.
                let mut open = self.with_turn_thread(|thread| thread.workflow.take());
                let outcome = workflow::apply(&mut open, &action);
                self.with_turn_thread(|thread| thread.workflow = open);
                outcome?
            }
        };

        self.save_turn_thread();
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
            .and_then(|t| t.session.view.clone())
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

    fn record(
        &self,
        session: &mut Session,
        question: &str,
        state: &TurnState,
        attached: &[Attachment],
    ) {
        if state.is_empty() {
            return;
        }
        let mut stored = StoredTurn::new(question, state);
        stored.images = self.keep_images(attached);
        let title = self.with_session_thread(session, |thread| {
            thread.push_turn(stored);
            thread.display_title()
        });
        self.save_session(session);
        self.adopt_background_turn(session);
        self.refresh_threads();
        // Only when the turn belongs to the chat on screen. A background run
        // must not retitle the header over somebody else's conversation.
        if let (Chat::Open, Some(window)) = (&session.chat, self.window()) {
            window.set_thread_title(&title);
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
        // The running turn's project rather than the open one. A scheduled run
        // belongs to the project its chat is in, and that decides which tools
        // it is offered and which instructions it runs under — reading the
        // slot would give a background job whatever the user is looking at.
        let project = imp
            .in_flight
            .borrow()
            .as_ref()
            .map(|turn| turn.session.project.clone())
            .unwrap_or_else(|| imp.project.borrow().clone());
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
        // The running turn's chat, not the open one — the same rule as the
        // project above, and for the same reason. A run against a chat that is
        // not open would otherwise be sent the history of whichever chat
        // happens to be on screen, which for a spoken question is somebody
        // else's conversation entirely.
        let mut history = self
            .with_turn_thread(|thread| thread.messages_with_reasoning(settings.carry_reasoning));

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

        // A spoken question says so, on the question rather than in the system
        // prompt. In the prompt it would change the cached prefix every time a
        // chat mixed typing and talking, which is most of them; here it costs
        // the tokens of one paragraph on the turns that are actually spoken,
        // and the thread still records what the person said and nothing else.
        let asked = if imp
            .in_flight
            .borrow()
            .as_ref()
            .is_some_and(|turn| turn.spoken)
        {
            spoken::asked_aloud(&asked)
        } else {
            asked
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

    fn fold_if_needed(&self, slug: &str, id: &ThreadId) {
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
        // Whichever chat just finished a turn, which for a scheduled run is
        // usually not the one on screen. Read once, here, so the summarizer is
        // given that chat's history rather than the slot's.
        let Some(thread) = self.thread_named(slug, id) else {
            return;
        };

        let used = thread
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

        let fold = thread.fold.clone();
        let history = thread.messages_with_reasoning(carry_reasoning);
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
        let slug = slug.to_string();
        let id = id.clone();
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
                    // The chat is named rather than carried: a fold takes a
                    // model call to arrive and the user may have opened
                    // something else by then. Naming it means the summary lands
                    // where it belongs whatever happened in between.
                    app.install_fold(&slug, &id, summary, &chunk, more);
                }
            ),
        );
    }

    /// One chat by name, from the slot if that is where it is and from disk
    /// otherwise.
    ///
    /// The slot's copy wins when it matches, because it may be a turn ahead of
    /// the file.
    fn thread_named(&self, slug: &str, id: &ThreadId) -> Option<Thread> {
        let open = self.imp().project.borrow().slug == slug && &self.imp().thread.borrow().id == id;
        if open {
            return Some(self.imp().thread.borrow().clone());
        }
        self.store()?.load_thread(slug, id).ok()
    }

    /// Store what the summarizer produced, and say so in the thread.
    ///
    /// A summary the server would not produce falls back to [`Headings`], which
    /// needs nothing and cannot fail. Folding is what keeps a long thread
    /// sendable, so the one outcome that must not happen is not folding at all.
    ///
    /// Installed into the chat that was folded, named rather than assumed. It
    /// used to write straight into the slot, which meant a background chat's
    /// summary landed on whatever the user had open — a fold is a lossy
    /// rewrite of what gets *sent*, so putting one on the wrong conversation
    /// silently shortens it.
    fn install_fold(
        &self,
        slug: &str,
        id: &ThreadId,
        summary: Option<String>,
        chunk: &[Message],
        more: usize,
    ) {
        let imp = self.imp();
        imp.folding.set(false);

        let Some(mut thread) = self.thread_named(slug, id) else {
            return;
        };
        let previous = thread.fold.clone();
        let fold = match summary {
            Some(summary) => compaction::Fold {
                summary,
                covers: previous.as_ref().map_or(0, |fold| fold.covers) + more,
            },
            None => compaction::extend(previous.as_ref(), chunk, more, &Headings),
        };
        thread.fold = Some(fold);

        let open = imp.project.borrow().slug == slug && &imp.thread.borrow().id == id;
        if open {
            // Only the fold, not the whole thread: the slot may have gained a
            // note or a title since this fold was asked for, and writing a copy
            // taken before the model call would undo it.
            imp.thread.borrow_mut().fold = thread.fold.clone();
            self.save_thread();
            self.announce(Compacted::Folded { turns: more });
            self.refresh_status();
        } else if let Some(store) = self.store() {
            let _ = store.save_thread(slug, &thread);
        }
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

    fn scheduled(&self) -> Vec<dialogs::Scheduled> {
        let now = chrono::Local::now();
        let projects = self.imp().projects.borrow().clone();
        let store = self.store();
        self.imp()
            .jobs
            .borrow()
            .jobs
            .iter()
            // The system's own upkeep has its cadence in Preferences; listing
            // it here would offer the same setting in two places.
            .filter(|job| job.source.editable())
            .map(|job| {
                let slug = job.destination.slug().unwrap_or_default().to_string();
                let project = projects
                    .iter()
                    .find(|project| project.slug == slug)
                    .map(|project| project.name.clone())
                    .unwrap_or_else(|| slug.clone());
                // The chat's own name, so a row says where its answers land
                // rather than only what it asks — several jobs may share one.
                let chat = job
                    .destination
                    .thread()
                    .and_then(crate::model::thread::ThreadId::from_stem)
                    .zip(store.as_ref())
                    .and_then(|(id, store)| store.load_thread(&slug, &id).ok())
                    .map(|thread| thread.display_title());
                dialogs::Scheduled {
                    id: job.id.clone(),
                    slug,
                    project,
                    chat,
                    thread: job.destination.thread().map(str::to_string),
                    title: job.title(),
                    schedule: job.schedule.describe(),
                    prompt: job.prompt.clone(),
                    enabled: job.enabled,
                    recovery: job.recovery,
                    status: describe_run(job, now),
                    current: Some((job.schedule, job.prompt.clone(), job.recovery)),
                }
            })
            .collect()
    }

    /// Set or change when the open chat wakes, from the main menu.
    ///
    /// Edits the chat's *first* job if it has one and adds a job otherwise.
    /// "The schedule for this chat" is only well defined when there is one, and
    /// a chat with several is managed under Scheduled Chats where each has a
    /// row of its own — this entry point stays for the common case of a chat
    /// with none.
    fn schedule_thread(&self) {
        use crate::model::jobs::{Destination, Job};

        let Some(window) = self.window() else { return };
        let (slug, thread) = self.turn_chat();
        let existing = self
            .imp()
            .jobs
            .borrow()
            .for_chat(&slug, &thread)
            .next()
            .map(|job| {
                (
                    job.id.clone(),
                    job.schedule,
                    job.prompt.clone(),
                    job.recovery,
                )
            });
        let opened = existing
            .as_ref()
            .map(|(_, schedule, prompt, recovery)| (*schedule, prompt.clone(), *recovery));
        let held = existing.map(|(id, _, _, _)| id);

        let chat = self.imp().thread.borrow().display_title();
        dialogs::edit_schedule(
            &window,
            &chat,
            opened,
            clone!(
                #[weak(rename_to = app)]
                self,
                move |chosen| {
                    let Some((schedule, prompt, recovery)) = chosen else {
                        return;
                    };
                    match &held {
                        Some(id) => app.edit_job(id, move |job| {
                            job.schedule = schedule;
                            job.prompt = prompt;
                            job.recovery = recovery;
                        }),
                        None => {
                            let mut job = Job::new(
                                "",
                                schedule,
                                &prompt,
                                Destination::Chat {
                                    slug: slug.clone(),
                                    thread: thread.clone(),
                                },
                            );
                            job.recovery = recovery;
                            // The clock starts now, or the first tick would
                            // fire every occurrence since the epoch.
                            job.last_run = Some(chrono::Utc::now());
                            app.imp().jobs.borrow_mut().add(job, chrono::Utc::now());
                            app.save_jobs();
                        }
                    }
                    // A schedule pointing at an unwritten chat has nowhere to
                    // land, so make sure the chat exists.
                    app.save_thread();
                    app.refresh_threads();
                    let next = schedule.first_after(chrono::Local::now());
                    if let Some(window) = app.window() {
                        window.toast(&format!(
                            "Scheduled — {}, next {}",
                            schedule.describe().to_lowercase(),
                            next.format("%A at %H:%M")
                        ));
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
                    dialogs::Change::Enabled { id, on } => {
                        app.edit_job(&id, |job| job.enabled = on);
                    }
                    dialogs::Change::Deleted { id } => {
                        app.imp().jobs.borrow_mut().remove(&id);
                        app.save_jobs();
                        app.refresh_threads();
                    }
                    dialogs::Change::Opened { slug, thread } => app.open_thread(&slug, &thread),
                    // By job id, so editing one schedule leaves the others in
                    // the same chat alone. `last_run` is deliberately untouched:
                    // moving a daily briefing from 07:00 to 08:00 must not make
                    // this morning's run happen a second time.
                    dialogs::Change::Edited {
                        id,
                        schedule,
                        prompt,
                        recovery,
                    } => {
                        app.edit_job(&id, move |job| {
                            job.schedule = schedule;
                            job.prompt = prompt;
                            job.recovery = recovery;
                        });
                        app.refresh_threads();
                    }
                }
            ),
        );
    }

    /// Change one job, wherever its chat lives and whether or not it is open.
    ///
    /// By id rather than by chat, which is the change the job list bought: a
    /// chat may be the destination of several, and keying by chat would pause
    /// or retime all of them together.
    fn edit_job<F>(&self, id: &str, change: F)
    where
        F: FnOnce(&mut crate::model::jobs::Job),
    {
        let found = {
            let mut jobs = self.imp().jobs.borrow_mut();
            match jobs.get_mut(id) {
                Some(job) => {
                    change(job);
                    true
                }
                None => false,
            }
        };
        if found {
            self.save_jobs();
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
                    // Takes effect now rather than at the next launch: somebody
                    // switching this on has just decided they want tonight's
                    // schedule to run.
                    app.apply_background();
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
fn describe_run(
    heartbeat: &crate::model::jobs::Job,
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
