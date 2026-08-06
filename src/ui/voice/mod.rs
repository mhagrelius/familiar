//! Talking to it: the microphone, the speech model, the voice and the window.
//!
//! One capability in one place. `model::voice` holds everything about a spoken
//! exchange that needs no hardware — when an utterance ended, which chat it
//! belongs to, what an answer sounds like — and this is the four boundaries it
//! is wired between:
//!
//! | | |
//! |---|---|
//! | [`recorder`] | `pw-record` on a pipe |
//! | [`speech`] | two Parakeet models on one worker thread |
//! | [`tts`] | speech-dispatcher, or an OpenAI-shaped speech endpoint |
//! | [`shortcut`] | a gnome-settings-daemon custom keybinding |
//! | [`window`] | the window the shortcut opens |
//!
//! The orchestration is in `ui::application`, with the rest of what a turn
//! does, because a spoken question is an ordinary turn: it goes through
//! `submit_turn`, into a real chat, under the project's own tools and
//! instructions, and everything downstream — folding, the passive reader, the
//! sidebar — happens to it without knowing where the words came from.
//!
//! **The microphone stays open for the whole exchange** — while it
//! transcribes, while it thinks, while it speaks — because an assistant you
//! have to wait for is a walkie-talkie. `model::voice::Barge` is what watches
//! it during those states, and it works without echo cancellation by learning
//! what it is already hearing and triggering on a margin above that: the room
//! while it thinks, its own voice off the speakers while it talks. On
//! headphones that makes it very sensitive; on loud speakers it means talking
//! over your own assistant, which is what interrupting anybody sounds like.
//!
//! The microphone closes when the exchange does, so the panel's indicator is
//! on for a conversation rather than for a session.

pub mod recorder;
pub mod shortcut;
pub mod speech;
pub mod tts;
mod window;

pub use recorder::Recorder;
pub use speech::Speech;
pub use tts::{Speaker, Voice};
pub use window::VoiceWindow;

use crate::model::voice::{Barge, Endpointer, Reading, Recent, Spoken};

/// Trace what the microphone is doing, when `FAMILIAR_VOICE_LOG` is set.
///
/// Voice is the one part of this application with no visible trace of its own:
/// a question that goes nowhere leaves an empty window and no file, and the
/// difference between "the gate never opened", "the model returned nothing"
/// and "the turn was dropped" is invisible from the outside. Every one of
/// those has happened; this is what told them apart.
pub fn log(what: std::fmt::Arguments<'_>) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("FAMILIAR_VOICE_LOG").is_some()) {
        eprintln!("voice: {what}");
    }
}

/// `voice::log!("heard {n} blocks")`.
#[macro_export]
macro_rules! voice_log {
    ($($arg:tt)*) => {
        $crate::ui::voice::log(format_args!($($arg)*))
    };
}

/// Where an exchange is up to.
///
/// Shared by the window, which names its buttons from it, and the application,
/// which refuses the things that make no sense in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Nothing is happening. The window may be open or closed.
    Idle,
    /// The microphone is on.
    Listening,
    /// The utterance ended and the accurate pass is running.
    Transcribing,
    /// The model has the question.
    Thinking,
    /// The answer is being read out.
    Speaking,
}

/// A spoken exchange in progress, and what it needs to remember between them.
///
/// Held by the application for the life of the process. The window inside it is
/// hidden rather than dropped between exchanges — see `VoiceWindow` — so this
/// is created once, on the first press of the shortcut.
pub struct Talk {
    pub window: VoiceWindow,
    /// Running while the microphone is open.
    pub recorder: Option<Recorder>,
    /// What ends an utterance when a live model is installed: words stopping.
    pub spoken: Spoken,
    /// And what ends it when one is not: loudness dropping. Also what draws
    /// the meter, either way.
    pub endpointer: Endpointer,
    /// Watches for somebody talking over it, while it thinks and while it
    /// speaks. Reset whenever what it would be hearing changes.
    pub barge: Barge,
    /// Samples not yet handed to the live model, kept until there is a whole
    /// chunk of them: the streaming encoder takes one size and nothing else.
    pub pending: Vec<f32>,
    /// What the live model has said so far this utterance. Feedback only —
    /// the accurate pass is what gets sent.
    pub live: String,
    /// The answer as it streams, both to show and to speak.
    pub answer: String,
    pub reading: Reading,
    /// The chat this exchange is going into, once one has been picked.
    pub chat: Option<Recent>,
    /// The last chat spoken in, which is what the follow-up window is measured
    /// against.
    pub last: Option<Recent>,
    /// The user asked for a new chat, so the follow-up window does not apply to
    /// the next question.
    pub fresh: bool,
    /// How long the window has been showing a state that something else is
    /// supposed to take it out of. The microphone is open through all of them,
    /// so this counts in blocks and needs no timer.
    pub waiting_ms: u32,
    /// Blocks of audio seen, for the trace.
    pub blocks: u64,
    /// This listen was opened by the application after an answer, not by
    /// somebody pressing the shortcut. It is quieter about hearing nothing: a
    /// conversation ending is not a failure to report.
    pub carrying_on: bool,
}

impl Talk {
    pub fn new(window: VoiceWindow) -> Self {
        Self {
            window,
            recorder: None,
            spoken: Spoken::default(),
            endpointer: Endpointer::default(),
            barge: Barge::default(),
            pending: Vec::new(),
            live: String::new(),
            answer: String::new(),
            reading: Reading::default(),
            chat: None,
            last: None,
            fresh: false,
            waiting_ms: 0,
            blocks: 0,
            carrying_on: false,
        }
    }

    /// Forget everything about the exchange that just happened, keeping what
    /// the next one needs.
    pub fn clear(&mut self) {
        self.recorder = None;
        self.spoken = Spoken::default();
        self.endpointer = Endpointer::default();
        self.barge = Barge::default();
        self.pending.clear();
        self.live.clear();
        self.answer.clear();
        self.reading = Reading::default();
    }
}
