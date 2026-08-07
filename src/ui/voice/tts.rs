//! Saying it back.
//!
//! One sentence at a time, spoken as it arrives, because waiting for the whole
//! answer would put the length of the answer into the length of the silence.
//! `model::voice::Reading` decides what a sentence is; this plays it.
//!
//! Two voices, and the difference is what is installed rather than what is
//! better. **speech-dispatcher** is the default because it is already on every
//! GNOME desktop — it is what Orca speaks through — so the feature works with
//! nothing fetched and nothing running. It sounds like 2005. An **endpoint** is
//! any OpenAI-shaped `/v1/audio/speech` — Kokoro through `kokoro-fastapi` is
//! the one this was written against — which sounds like a person and costs a
//! server to keep running. Neither is a dependency: with no voice configured
//! and no speech-dispatcher installed the answer is read on screen and nothing
//! fails.
//!
//! Everything here is cancellable at a sentence boundary and mid-sentence, so
//! pressing the shortcut while it is talking stops it talking. **That is the
//! whole of barge-in this app has**, and the measurement is why: the assistant's
//! own voice off the speakers reaches this desk's microphone at a peak of 0.577,
//! against 0.578 for the person in front of it. Nothing about a level separates
//! those, so watching the microphone for somebody talking over the top was a
//! coin toss between interrupting itself and not being interruptible at all —
//! both of which happened. Audio arriving while it talks is discarded.

use gio::prelude::*;
use gtk::glib;
use soup::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

/// How the answer is spoken.
#[derive(Debug, Clone, PartialEq)]
pub enum Voice {
    /// Not aloud. The answer is still on screen.
    Silent,
    /// Through speech-dispatcher, the desktop's own synthesiser.
    Desktop,
    /// Through an OpenAI-shaped speech endpoint.
    Endpoint {
        /// Base URL, e.g. `http://127.0.0.1:8880`.
        url: String,
        /// The voice's name, as that server spells it.
        name: String,
        /// What sample rate its raw PCM comes back at. Kokoro's is 24 kHz.
        rate: u32,
    },
}

impl Voice {
    /// Whether this voice can actually be used on this machine right now.
    pub fn is_available(&self) -> bool {
        match self {
            Voice::Silent => true,
            Voice::Desktop => glib::find_program_in_path("spd-say").is_some(),
            Voice::Endpoint { url, .. } => {
                !url.trim().is_empty() && glib::find_program_in_path("pw-cat").is_some()
            }
        }
    }

    /// Why it cannot, in a sentence somebody can act on.
    pub fn unavailable(&self) -> Option<String> {
        if self.is_available() {
            return None;
        }
        Some(match self {
            Voice::Silent => unreachable!("silence is always available"),
            Voice::Desktop => "speech-dispatcher is not installed, so there is nothing to \
                 speak with. Install speech-dispatcher, or set a speech endpoint in \
                 Preferences."
                .into(),
            Voice::Endpoint { .. } => {
                "pw-cat is not installed, so there is nothing to play the audio through. \
                 It comes with PipeWire."
                    .into()
            }
        })
    }
}

/// The request an OpenAI-shaped speech endpoint takes.
///
/// `pcm` rather than a container: this goes straight into `pw-cat`, and a WAV
/// header in the middle of a stream of samples is a click.
fn speech_request(text: &str, name: &str) -> Vec<u8> {
    let body = serde_json::json!({
        "model": "tts-1",
        "input": text,
        "voice": name,
        "response_format": "pcm",
    });
    serde_json::to_vec(&body).unwrap_or_default()
}

/// The command that plays raw mono PCM at `rate`.
///
/// `--raw` is not optional and its absence is not obvious: without it `pw-cat`
/// hands the pipe to libsndfile, which looks for a container it recognises,
/// finds a stream of samples, and fails with "Format not recognised". The
/// stream options are then applied to a file that never opened, so the whole
/// thing is silent with one line on a stderr nobody is reading.
fn playback_argv(rate: u32) -> Vec<String> {
    vec![
        "pw-cat".into(),
        "--playback".into(),
        "--raw".into(),
        "--format".into(),
        "s16".into(),
        "--rate".into(),
        rate.to_string(),
        "--channels".into(),
        "1".into(),
        "-".into(),
    ]
}

/// The command that speaks `text` through the desktop synthesiser.
///
/// `-w` waits for the speech to finish, which is what makes the process's exit
/// the signal to start the next sentence. `--` protects a sentence that begins
/// with a dash from being read as a flag.
fn dispatcher_argv(text: &str) -> Vec<String> {
    vec!["spd-say".into(), "-w".into(), "--".into(), text.into()]
}

/// Told when it starts and stops making noise.
type OnChange = Box<dyn Fn(bool)>;

/// Speaks sentences, one after another, and stops when told.
pub struct Speaker {
    voice: RefCell<Voice>,
    queue: RefCell<VecDeque<String>>,
    /// The process currently making noise, if any.
    playing: RefCell<Option<gio::Subprocess>>,
    /// The fetch of the next sentence's audio, if one is outstanding.
    fetching: RefCell<Option<gio::Cancellable>>,
    session: soup::Session,
    speaking: Cell<bool>,
    /// Set by [`Self::hush`] and cleared by [`Self::allow`]: refuse sentences
    /// until somebody says this exchange may be spoken again.
    ///
    /// **A stop has to be a latch, not a moment.** Clearing the queue only
    /// silences what is already in it, and the answer is still arriving — every
    /// delta of a streaming turn queues another sentence. So a stop went quiet
    /// for a fraction of a second and then carried on reading, which is what
    /// "cancel and stop do not work" was. Worse after an interruption: the turn
    /// is abandoned and out of the in-flight slot, so the cancel path can no
    /// longer find anything to stop, while the dead turn's buffered deltas keep
    /// feeding this queue.
    muted: Cell<bool>,
    /// Told whenever it starts or stops making noise, so the overlay can say so.
    on_change: RefCell<Option<OnChange>>,
}

impl Default for Speaker {
    fn default() -> Self {
        Self::new()
    }
}

impl Speaker {
    pub fn new() -> Self {
        Self {
            voice: RefCell::new(Voice::Silent),
            queue: RefCell::new(VecDeque::new()),
            playing: RefCell::new(None),
            fetching: RefCell::new(None),
            session: soup::Session::new(),
            speaking: Cell::new(false),
            muted: Cell::new(false),
            on_change: RefCell::new(None),
        }
    }

    /// Allow this exchange to be read out.
    ///
    /// Called where a new answer is about to begin, which is the one place that
    /// knows a stop is over. Every path that stops the reading mutes, and
    /// nothing un-mutes on its own — so a stop stays stopped however much of
    /// the old answer is still on its way.
    pub fn allow(&self) {
        self.muted.set(false);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.get()
    }

    pub fn set_voice(&self, voice: Voice) {
        self.voice.replace(voice);
    }

    pub fn voice(&self) -> Voice {
        self.voice.borrow().clone()
    }

    pub fn connect_changed(&self, handler: impl Fn(bool) + 'static) {
        self.on_change.replace(Some(Box::new(handler)));
    }

    pub fn is_speaking(&self) -> bool {
        self.speaking.get()
    }

    /// Whether anything is queued, being fetched, or playing.
    ///
    /// Not the same question as [`Self::is_speaking`], which is about whether
    /// noise is coming out *right now*. Between two sentences there is a
    /// moment where the first has finished and the second is still being
    /// fetched, and a caller that asks the other question there concludes the
    /// answer is over — starts listening, and records the rest of the answer
    /// as if it were the next thing said.
    pub fn is_busy(&self) -> bool {
        self.playing.borrow().is_some()
            || self.fetching.borrow().is_some()
            || !self.queue.borrow().is_empty()
    }

    /// Queue a sentence and start speaking if nothing else is.
    pub fn say(self: &Rc<Self>, sentence: &str) {
        let sentence = sentence.trim();
        if self.muted.get() {
            crate::voice_log!(
                "not saying {:?}: stopped",
                sentence.chars().take(30).collect::<String>()
            );
            return;
        }
        if sentence.is_empty() || matches!(*self.voice.borrow(), Voice::Silent) {
            crate::voice_log!(
                "not saying {:?}: voice is {:?}",
                sentence.chars().take(30).collect::<String>(),
                self.voice.borrow()
            );
            return;
        }
        crate::voice_log!("saying {:?}", sentence.chars().take(40).collect::<String>());
        self.queue.borrow_mut().push_back(sentence.to_string());
        self.pump();
    }

    /// Stop, now, forget what was queued, and refuse any more of it.
    ///
    /// The dispatcher needs telling twice: killing the client that is waiting
    /// does not stop the server that is speaking, and `-C` is what does.
    ///
    /// Stays stopped until [`Self::allow`]. See `muted` for why clearing the
    /// queue is not enough on its own.
    pub fn hush(&self) {
        self.muted.set(true);
        self.queue.borrow_mut().clear();
        if let Some(cancellable) = self.fetching.take() {
            cancellable.cancel();
        }
        if let Some(process) = self.playing.take() {
            process.force_exit();
        }
        if matches!(*self.voice.borrow(), Voice::Desktop)
            && glib::find_program_in_path("spd-say").is_some()
        {
            let _ = gio::Subprocess::newv(
                &[std::ffi::OsStr::new("spd-say"), std::ffi::OsStr::new("-C")],
                gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_SILENCE,
            );
        }
        // Quiet, but *not announced* as quiet. The change handler means "the
        // answer finished", which is what makes the application start listening
        // for a follow-up — so announcing a deliberate stop had it open a fresh
        // exchange on its way out of being cancelled, halfway through the cancel
        // that was tearing the old one down. Every caller of `hush` sets the
        // state it wants next itself.
        self.speaking.set(false);
    }

    fn announce(&self, speaking: bool) {
        if self.speaking.get() == speaking {
            return;
        }
        self.speaking.set(speaking);
        crate::voice_log!("speaker is {}", if speaking { "talking" } else { "quiet" });
        if let Some(handler) = self.on_change.borrow().as_ref() {
            handler(speaking);
        }
    }

    /// Start the next sentence, if there is one and nothing is playing.
    fn pump(self: &Rc<Self>) {
        if self.playing.borrow().is_some() || self.fetching.borrow().is_some() {
            return;
        }
        let Some(sentence) = self.queue.borrow_mut().pop_front() else {
            self.announce(false);
            return;
        };
        self.announce(true);
        match self.voice.borrow().clone() {
            Voice::Silent => {}
            Voice::Desktop => self.speak_here(&dispatcher_argv(&sentence), None),
            Voice::Endpoint { url, name, rate } => {
                self.fetch_then_play(&url, &name, rate, sentence)
            }
        }
    }

    /// Run a command that makes the noise, and pump again when it exits.
    fn speak_here(self: &Rc<Self>, argv: &[String], feed: Option<glib::Bytes>) {
        let words: Vec<&std::ffi::OsStr> = argv.iter().map(std::ffi::OsStr::new).collect();
        let flags = if feed.is_some() {
            gio::SubprocessFlags::STDIN_PIPE | gio::SubprocessFlags::STDERR_SILENCE
        } else {
            gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_SILENCE
        };
        let Ok(process) = gio::Subprocess::newv(&words, flags) else {
            // Nothing to speak with. Silence is the failure mode, and the
            // answer is on screen either way.
            self.queue.borrow_mut().clear();
            self.announce(false);
            return;
        };
        self.playing.replace(Some(process.clone()));

        glib::spawn_future_local(glib::clone!(
            #[strong(rename_to = speaker)]
            self,
            async move {
                if let (Some(bytes), Some(stdin)) = (feed, process.stdin_pipe()) {
                    // Written and closed: `pw-cat` plays until its input ends,
                    // so leaving the pipe open leaves it waiting in silence.
                    let _ = stdin.write_all_future(bytes, glib::Priority::DEFAULT).await;
                    let _ = stdin.close_future(glib::Priority::DEFAULT).await;
                }
                let _ = process.wait_future().await;
                // Only clear the slot if it is still ours: `hush` may have
                // replaced it, and clearing it then would let a stopped
                // speaker start the next sentence.
                let mine = speaker
                    .playing
                    .borrow()
                    .as_ref()
                    .is_some_and(|current| current == &process);
                if mine {
                    speaker.playing.replace(None);
                    speaker.pump();
                }
            }
        ));
    }

    /// Ask the endpoint for the audio, then play it.
    fn fetch_then_play(self: &Rc<Self>, url: &str, name: &str, rate: u32, sentence: String) {
        let endpoint = format!("{}/v1/audio/speech", url.trim_end_matches('/'));
        let Ok(message) = soup::Message::new("POST", &endpoint) else {
            self.queue.borrow_mut().clear();
            self.announce(false);
            return;
        };
        message.set_request_body_from_bytes(
            Some("application/json"),
            Some(&glib::Bytes::from_owned(speech_request(&sentence, name))),
        );

        let cancellable = gio::Cancellable::new();
        self.fetching.replace(Some(cancellable.clone()));
        let sent = message.clone();
        self.session.send_and_read_async(
            &message,
            glib::Priority::DEFAULT,
            Some(&cancellable),
            glib::clone!(
                #[strong(rename_to = speaker)]
                self,
                move |result| {
                    speaker.fetching.replace(None);
                    let status = sent.status_code() as u16;
                    match result {
                        Ok(bytes) if (200..300).contains(&status) && !bytes.is_empty() => {
                            speaker.speak_here(&playback_argv(rate), Some(bytes));
                        }
                        // A voice that will not answer is not worth queueing
                        // behind. Drop what is left rather than stuttering
                        // through a failure per sentence.
                        other => {
                            crate::voice_log!(
                                "the speech endpoint refused: status {status}, {}",
                                match &other {
                                    Ok(bytes) => format!("{} bytes", bytes.len()),
                                    Err(error) => error.to_string(),
                                }
                            );
                            speaker.queue.borrow_mut().clear();
                            speaker.announce(false);
                        }
                    }
                }
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stop_stays_stopped() {
        // Clearing the queue silences what is in it; it does not stop the answer
        // still arriving. Every delta of a streaming turn queues another
        // sentence, so a stop went quiet for a fraction of a second and then
        // carried on reading — and after an interruption the turn is already out
        // of the in-flight slot, so the cancel path can no longer find anything
        // to stop while the dead turn's deltas keep arriving.
        let speaker = Rc::new(Speaker::new());
        speaker.set_voice(Voice::Desktop);
        speaker.hush();
        assert!(speaker.is_muted());
        speaker.say("the rest of an answer that was cancelled");
        assert!(
            !speaker.is_busy(),
            "a stopped speaker must not take another sentence"
        );
    }

    #[test]
    fn a_new_question_lets_it_speak_again() {
        // Nothing un-mutes on its own, or the latch would not hold. Asking
        // something new is the one thing that means the stop is over.
        let speaker = Rc::new(Speaker::new());
        speaker.hush();
        assert!(speaker.is_muted());
        speaker.allow();
        assert!(!speaker.is_muted());
    }

    #[test]
    fn the_endpoint_is_asked_for_raw_samples() {
        let body = speech_request("hello", "af_heart");
        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(parsed["response_format"], "pcm");
        assert_eq!(parsed["input"], "hello");
        assert_eq!(parsed["voice"], "af_heart");
    }

    #[test]
    fn a_sentence_with_quotes_survives_being_a_request() {
        let body = speech_request("she said \"no\" and left", "af_heart");
        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(parsed["input"], "she said \"no\" and left");
    }

    #[test]
    fn playback_matches_what_the_endpoint_returns() {
        let argv = playback_argv(24_000);
        assert!(
            argv.contains(&"--raw".to_string()),
            "without it pw-cat looks for a container in a stream of samples and plays nothing"
        );
        assert!(argv.contains(&"24000".to_string()));
        assert!(argv.contains(&"s16".to_string()));
        // Mono, or a stereo player would halve the pitch of a mono stream.
        let channels = argv.iter().position(|word| word == "--channels").unwrap();
        assert_eq!(argv[channels + 1], "1");
        assert_eq!(argv.last().unwrap(), "-", "it reads from the pipe");
    }

    #[test]
    fn a_sentence_starting_with_a_dash_is_not_read_as_a_flag() {
        let argv = dispatcher_argv("-- delete that");
        let separator = argv.iter().position(|word| word == "--").unwrap();
        assert_eq!(argv[separator + 1], "-- delete that");
    }

    #[test]
    fn the_dispatcher_waits_so_sentences_do_not_overlap() {
        assert!(dispatcher_argv("anything").contains(&"-w".to_string()));
    }

    #[test]
    fn silence_is_always_available() {
        assert!(Voice::Silent.is_available());
        assert!(Voice::Silent.unavailable().is_none());
    }

    #[test]
    fn an_endpoint_with_no_url_is_not_available() {
        let voice = Voice::Endpoint {
            url: "  ".into(),
            name: "af_heart".into(),
            rate: 24_000,
        };
        assert!(!voice.is_available());
    }
}
