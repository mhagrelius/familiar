//! Talking to it.
//!
//! Everything about a spoken exchange that does not need a microphone, a model
//! or a display: when an utterance has ended, which chat it belongs to, how an
//! answer written for a screen is turned into something worth hearing, and the
//! sentence the model is given so it stops writing headings at somebody who is
//! listening.
//!
//! Three rules hold this together and the rest follows from them:
//!
//! 1. **Silence ends an utterance, not a key.** The shortcut is press-only —
//!    see `ui::voice::shortcut` for why — so something has to decide when the
//!    speaking stopped, and [`Endpointer`] is it.
//! 2. **A spoken question is an ordinary turn in an ordinary chat.** Nothing
//!    here writes a parallel store. [`continuation`] picks a chat and the
//!    application submits into it, so memory, folding, workflows and the
//!    sidebar all apply without knowing voice exists.
//! 3. **The prompt prefix is not touched.** The voice register rides on the
//!    question, the same way an attached document does, because a system
//!    prompt that changes between a typed turn and a spoken one throws away
//!    the KV prefix llama-server cached.

use chrono::{DateTime, Duration, Utc};

/// What the endpointer makes of the audio so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heard {
    /// Nothing yet. Still listening.
    Quiet,
    /// Somebody is talking.
    Speaking,
    /// They talked and then stopped. Transcribe it.
    Ended,
    /// They have been talking for a very long time. Take what there is.
    Overlong,
    /// Nobody said anything at all. Close, and do not send an empty question.
    NothingSaid,
}

/// The most audio one read of the microphone can return, in milliseconds.
///
/// **A read almost never returns this much.** Measured against `pw-record` on
/// this machine: 92 reads in three seconds, not one of them a full block, most
/// of them a little over half. Every caller therefore has to pass the length
/// of the audio it actually got — passing this constant instead ran the
/// endpointer's clock at nearly twice real time, which made its patience
/// expire while somebody was still talking and its hangover cut them off at
/// the first pause in a sentence. Both were reported as "it did not hear me".
pub const BLOCK_MS: u32 = 40;

/// Decides when an utterance has finished, from the loudness of each block.
///
/// The gate cannot be a constant: fixed, it is deaf on a quiet laptop
/// microphone and held open by a fan on a loud one. So it sits a margin above
/// the room — and **what "the room" means is the whole difficulty.** Three
/// answers have been tried and the first two are worth keeping written down,
/// because each one looks like a working design until it meets a microphone it
/// was not tuned on.
///
/// Measuring the room *once*, from the first quarter-second, measures whatever
/// the speaker said first — people press the key and start talking. The gate
/// lands above their own voice, no speech is ever detected, and the microphone
/// stays open until the two-minute cap.
///
/// Measuring it as a minimum over *all* time fixes that and breaks the other
/// end: the floor only ever falls, so one quiet instant pins it low for the
/// rest of the utterance. Move somewhere noisier and the ambient level now sits
/// permanently above a gate set during a lull — every block reads as speech,
/// silence never accumulates, and it never notices you stopped talking.
///
/// A **minimum over a rolling window** was the answer to both, and it is wrong
/// for a reason that took a measurement to see. Many microphones do not emit a
/// quiet room; they emit *silence* interrupted by noise. This desk's webcam
/// reads exactly 0.000 for a tenth of its blocks while its actual room level is
/// 0.124 with peaks to 0.385. A rolling minimum reads 0.017 there. So
/// `room + margin` fell below `floor` on every single block, `gate()` returned
/// the constant `floor`, and the adaptive machinery contributed nothing at all
/// — while the room it was supposed to be adapting to sat *above* that floor
/// 15% of the time. Room noise kept resetting the silence counter, `Ended`
/// never fired, and the microphone stayed open for two minutes.
///
/// So the room is the **25th percentile** of a rolling window: low enough to
/// sit under speech, which has gaps, and high enough that occasional true
/// silence cannot drag it to zero. Measured on this desk it reads 0.073–0.091
/// against a real room level of 0.124, and 0.017 in a genuinely quiet room —
/// which is what makes one `margin` serve both.
///
/// The cost is the first word or two, before any room has been seen. Nothing
/// depends on those: the whole buffer is transcribed regardless, and all the
/// gate decides is *when to stop listening*.
#[derive(Debug, Clone)]
pub struct Endpointer {
    /// Never open the gate below this, however quiet the room measured. A
    /// sanity minimum, not the working value — if this is what `gate()`
    /// returns, the room estimate has stopped contributing.
    pub floor: f64,
    /// Never hold it above this.
    ///
    /// **The ceiling is set by the gaps in speech, not by how loud a voice is.**
    /// Silence under the gate is what ends an utterance, so a gate above the
    /// quiet moments *between words* ends it mid-sentence. Measured on this
    /// desk: at a gate of 0.35 the longest gap inside ordinary speech is
    /// 640 ms, and at 0.40 it is 1320 ms — past `hangover_ms`, so the utterance
    /// is cut off while somebody is still talking.
    pub loud: f64,
    /// How far above the room speech has to be. This is what adapts, so it
    /// carries the working range rather than `floor`.
    pub margin: f64,
    /// Silence this long, after enough speech, ends the utterance.
    pub hangover_ms: u32,
    /// Less speech than this is a cough, not a question.
    pub min_speech_ms: u32,
    /// Stop listening after this whatever happens.
    pub max_ms: u32,
    /// Silence this long without enough speech gives up.
    pub patience_ms: u32,
    /// How much over-gate audio the rolling window needs before words the live
    /// model produced can be credited to the person rather than the room. See
    /// [`Self::heard_you`].
    pub attributable_ms: u32,

    /// How long a window of room-noise history is worth keeping. Long enough
    /// to span the gaps in ordinary speech, short enough that a room which
    /// changes is followed within a couple of seconds.
    window_ms: u32,
    /// One entry per block over the last `window_ms`, oldest first, each with
    /// the span of audio it covered — reads off the microphone are not all the
    /// same length, so the window is trimmed by time rather than by count. The
    /// percentile needs the samples themselves, not a running aggregate.
    recent: std::collections::VecDeque<(f64, u32)>,
    /// The spans in `recent`, summed.
    window_span_ms: u32,
    peak: f64,
    speech_ms: u32,
    silence_ms: u32,
    total_ms: u32,
}

impl Default for Endpointer {
    fn default() -> Self {
        // Every level here is on the curve `ui::voice::recorder::level`
        // produces, and all four came off one afternoon's measurement of this
        // desk: a silent room and seven seconds of ordinary talking, put
        // through that curve at the same 40 ms blocks the app uses.
        //
        //                   room        speech
        //   median          0.124        0.343
        //   p90             0.221        0.511
        //   p99             0.357        0.562
        //   window p25      0.073        0.225
        //
        // The room overlaps the *bottom* of speech, so no single threshold
        // separates them — which is why the gate is `room p25 + margin` and why
        // the numbers below are stated as the run lengths they produce rather
        // than as levels that sound right.
        Self {
            // A sanity minimum only. The room estimate is what normally sets
            // the gate; this stops a pathologically silent input opening it at
            // the margin alone.
            floor: 0.15,
            // Measured: at this gate the longest gap inside ordinary speech is
            // 640 ms, comfortably inside `hangover_ms`, while the longest run
            // of room noise above it is 120 ms. Above 0.38 the speech gaps
            // start to exceed the hangover and it cuts people off.
            loud: 0.34,
            // What turns a room estimate into a gate. 0.21 above a measured
            // p25 puts the gate at 0.23 in a quiet room and 0.30 in this one —
            // and 0.30 is where a silent room's longest quiet run reaches
            // 1720 ms against an 800 ms hangover, with 2.5x the margin the old
            // 0.20 gate had.
            margin: 0.21,
            // Long enough to survive the pause in the middle of a sentence,
            // short enough that it does not feel like waiting. Measured
            // against nothing but taste; it is a preference for a reason.
            hangover_ms: 800,
            min_speech_ms: 300,
            max_ms: 120_000,
            patience_ms: 8_000,
            // Between a silent room's worst 1.5 s (200 ms above the gate) and
            // speech's quietest (720 ms). Nothing measured lands in between.
            attributable_ms: 400,
            window_ms: 1_500,
            recent: std::collections::VecDeque::new(),
            window_span_ms: 0,
            peak: 0.0,
            speech_ms: 0,
            silence_ms: 0,
            total_ms: 0,
        }
    }
}

impl Endpointer {
    /// What the room sounds like: the 25th percentile of the rolling window.
    ///
    /// A quarter of recent blocks are quieter than this. Speech cannot hold it
    /// up, because more than a quarter of speech is the gaps between words; a
    /// microphone that emits occasional true silence cannot drag it to zero,
    /// because a tenth of the blocks reading 0.000 leaves the quartile where it
    /// was. That is the whole reason it is a percentile and not a minimum.
    ///
    /// `None` until anything has been heard at all.
    fn room(&self) -> Option<f64> {
        if self.recent.is_empty() {
            return None;
        }
        let mut levels: Vec<f64> = self.recent.iter().map(|(level, _)| *level).collect();
        levels.sort_by(|a, b| a.total_cmp(b));
        Some(levels[levels.len() / 4])
    }

    /// Where the gate sits, given what the room has been doing.
    pub fn gate(&self) -> f64 {
        match self.room() {
            None => self.floor,
            Some(room) => (room + self.margin).clamp(self.floor, self.loud),
        }
    }

    /// Whether enough speech has been heard to be worth transcribing.
    pub fn has_speech(&self) -> bool {
        self.speech_ms >= self.min_speech_ms
    }

    /// How much of the rolling window was above the gate.
    ///
    /// Unlike [`Self::has_speech`] this forgets: it asks what the microphone
    /// heard *just now*, not what it has heard since the shortcut was pressed.
    fn speech_in_window_ms(&self) -> u32 {
        let gate = self.gate();
        self.recent
            .iter()
            .filter(|(level, _)| *level >= gate)
            .map(|(_, span)| *span)
            .sum()
    }

    /// Whether the microphone can account for the person at it *right now*.
    ///
    /// This is the loudness half's opinion of whether words the live model
    /// produced belong to the speaker or to the room, and it exists because
    /// [`Self::has_speech`] is cumulative and never resets — given a few
    /// seconds of any real room it becomes true and stays true, after which
    /// every hallucinated word the streaming model emits on room noise resets
    /// [`Spoken`]'s clock and the microphone never closes. Measured, that held
    /// it open indefinitely: twenty of thirty-one chunks of a *silent* room
    /// were credited to the speaker.
    ///
    /// Sustained energy is what tells them apart, and the separation is wide.
    /// Over-gate audio per 1.5 s window, measured on this desk:
    ///
    /// | | silent room | speech |
    /// |---|---|---|
    /// | median | 160 ms | 960 ms |
    /// | worst case | 200 ms | 720 ms |
    ///
    /// `attributable_ms` sits in the middle of that gap, where neither side
    /// reaches it. A room cannot fake sustained speech; its bursts are 120 to
    /// 160 ms long and a word is not.
    pub fn heard_you(&self) -> bool {
        self.speech_in_window_ms() >= self.attributable_ms
    }

    /// How long has been listened to, in milliseconds.
    pub fn elapsed_ms(&self) -> u32 {
        self.total_ms
    }

    /// How much of that was speech, and how much trailing silence. What the
    /// decision is actually made of, for the trace — a single sampled level is
    /// not evidence, because one block a second lands in the gap between two
    /// syllables as often as not.
    pub fn tally(&self) -> (u32, u32) {
        (self.speech_ms, self.silence_ms)
    }

    /// The loudest block so far.
    pub fn peak(&self) -> f64 {
        self.peak
    }

    /// Take one block's loudness and say what to do about it.
    pub fn push(&mut self, level: f64, span_ms: u32) -> Heard {
        self.total_ms = self.total_ms.saturating_add(span_ms);
        self.peak = self.peak.max(level);

        // The room, over a window that moves. Blocks fall off the far end one
        // at a time, so the estimate slides rather than stepping — an earlier
        // version rotated two windows and briefly forgot half its history
        // every time it swapped them.
        self.recent.push_back((level, span_ms));
        self.window_span_ms = self.window_span_ms.saturating_add(span_ms);
        while self.window_span_ms > self.window_ms && self.recent.len() > 1 {
            if let Some((_, span)) = self.recent.pop_front() {
                self.window_span_ms = self.window_span_ms.saturating_sub(span);
            }
        }

        if level >= self.gate() {
            self.speech_ms = self.speech_ms.saturating_add(span_ms);
            self.silence_ms = 0;
        } else {
            self.silence_ms = self.silence_ms.saturating_add(span_ms);
        }

        let enough = self.speech_ms >= self.min_speech_ms;
        if self.total_ms >= self.max_ms {
            // Overlong only counts as an utterance if there was one.
            return if enough {
                Heard::Overlong
            } else {
                Heard::NothingSaid
            };
        }
        if enough && self.silence_ms >= self.hangover_ms {
            return Heard::Ended;
        }
        // Not enough to send, and quiet for a long time. This has to cover
        // *some* speech as well as none: a throat-clear that opened the gate
        // for two blocks used to satisfy neither this nor the rule above, and
        // the microphone stayed open until the cap with nothing happening —
        // which is what it looks like from the outside when it hangs.
        if !enough && self.silence_ms >= self.patience_ms {
            return Heard::NothingSaid;
        }
        if self.speech_ms > 0 {
            Heard::Speaking
        } else {
            Heard::Quiet
        }
    }
}

/// Watches for somebody talking over the answer.
///
/// Interrupting is the difference between a conversation and a walkie-talkie,
/// so the microphone stays open while it thinks and while it speaks, and this
/// decides when what it is hearing is a person rather than itself.
///
/// **There is no echo cancellation, and this is how it copes without one.** The
/// background is learned continuously: during thinking that is the room, and
/// during speaking it is the assistant's own voice coming back off the
/// speakers. A person talking arrives *on top* of whatever that is, so the
/// trigger is a margin above the running background rather than an absolute
/// level. Two consequences worth knowing: on headphones it is very sensitive,
/// because the background is just the room; and on loud speakers somebody has
/// to talk over their own assistant, which is what interrupting a person sounds
/// like anyway.
///
/// The settle period is what stops it hearing itself start. Speech beginning
/// takes the background from silence to echo in a few blocks, and without a
/// pause to learn in, the assistant's first syllable reads as an interruption
/// and it shuts itself up.
#[derive(Debug, Clone)]
pub struct Barge {
    /// Never trigger below this, however quiet the background is.
    ///
    /// Without it, a quiet room sets the bar at a level a keyboard, a mug or a
    /// chair clears — and the "interruption" throws away the turn the person
    /// is waiting on. A margin above near-silence is not a voice; this is what
    /// says what a voice is.
    pub voice: f64,
    /// How far above the background a voice has to be.
    pub margin: f64,
    /// And for how long, so a cough or a keyboard is not an interruption.
    pub trigger_ms: u32,
    /// Learn the background for this long before triggering on anything.
    pub settle_ms: u32,
    background: f64,
    over_ms: u32,
    heard_ms: u32,
}

impl Default for Barge {
    fn default() -> Self {
        Self {
            // Set from a measurement, and re-measured after the first one
            // turned out to have been taken through a webcam that was quietly
            // cancelling the room. **0.18 was chosen against a room reading
            // 0.01 that actually reads 0.124**, with peaks to 0.385 — a quarter
            // of its silent blocks clear 0.18, so the floor was inside the
            // noise it was supposed to sit above.
            //
            // 0.32 is picked for a structural reason rather than a comfortable
            // margin: measured on this desk, the longest unbroken run of room
            // noise above 0.32 is 120 ms, and `trigger_ms` is 160. So a silent
            // room cannot interrupt at all, however long it is listened to,
            // while ordinary speech spends 58% of its blocks above it and
            // reaches the trigger in four. Where a room *is* louder than this,
            // the margin above the measured background takes over and the
            // floor stops mattering.
            voice: 0.32,
            // Small on purpose. This scale is a fifth root of power, so 0.07
            // above a background of 0.40 is about four decibels — a person
            // talking over a speaker, not a person shouting at it.
            margin: 0.07,
            trigger_ms: 160,
            settle_ms: 600,
            background: 0.0,
            over_ms: 0,
            heard_ms: 0,
        }
    }
}

impl Barge {
    /// How long it has been watching, in milliseconds.
    pub fn elapsed_ms(&self) -> u32 {
        self.heard_ms
    }

    /// What a block has to beat to count as somebody talking.
    pub fn threshold(&self) -> f64 {
        (self.background + self.margin).max(self.voice)
    }

    /// How close it is to deciding, as a fraction of the trigger.
    pub fn nearly(&self) -> f64 {
        f64::from(self.over_ms) / f64::from(self.trigger_ms.max(1))
    }

    /// Take one block. True the moment it decides somebody is talking.
    pub fn push(&mut self, level: f64, span_ms: u32) -> bool {
        self.heard_ms = self.heard_ms.saturating_add(span_ms);

        if self.heard_ms <= self.settle_ms {
            // The loudest thing in the settle window is the thing to be heard
            // over: while it speaks that is its own voice at its peak, and
            // while it thinks it is the room.
            self.background = self.background.max(level);
            return false;
        }

        // Afterwards the background only ever comes *down*, and slowly. One
        // that chased the level upwards would climb to meet an interruption
        // and never fire — the first version did exactly that, and nothing
        // could be interrupted at all.
        if level < self.background {
            self.background += (level - self.background) * 0.02;
        }

        // Accumulated with a decay rather than reset on the first quiet
        // block. Speech is not continuously loud — between two syllables it
        // is nearly silent — so a counter that resets whenever it dips needs
        // one unnaturally sustained shout to reach its threshold, which is
        // what "it did not pick up at normal talking volume" is.
        if level > self.threshold() {
            self.over_ms = self.over_ms.saturating_add(span_ms);
        } else {
            self.over_ms = self.over_ms.saturating_sub(span_ms / 3);
        }
        self.over_ms >= self.trigger_ms
    }
}

/// Ends an utterance by *words* rather than by loudness.
///
/// The energy gate below it works, on the microphone it was tuned against, in
/// the room it was tuned in. Change either — a webcam whose noise suppression
/// ducks the room, that same webcam with the suppression switched off, a
/// quieter voice, a fan — and it needs tuning again, in a direction that is not
/// obvious from the outside. An evening of that is what produced this.
///
/// The live speech model is already running while somebody talks: it is what
/// puts the words on screen as they are said. If it is emitting words, that is
/// speech, and no threshold is involved in knowing it. So the rule becomes
/// "somebody said something, and has not said anything for a moment", which is
/// what a person means by an utterance ending and holds whatever the
/// microphone is doing to the levels.
///
/// The cost is latency: the model works in 560 ms chunks, so the end of speech
/// is known a chunk late. [`Self::quiet_ms`] is set well above that, because
/// being a little slow to answer is a much smaller failure than cutting
/// somebody off in the middle of a sentence — which is the one this replaces.
///
/// **Words alone are not enough either**, and the reason is more ordinary than
/// a broken model: it transcribes everything it can hear, and a microphone can
/// hear more than the person in front of it. Measured — ten seconds in a room
/// with a video playing across it, one millisecond of audio above the gate,
/// and a clean running transcript of the video the whole time. Left to decide
/// on its own the utterance never ends, because somebody else's speech keeps
/// resetting the clock, and one starts when the person has not said a word.
///
/// So the two signals do different jobs, and neither does the other's.
/// **Loudness says whether *you* made a sound; words say whether it is over.**
/// A video across the room reaches the microphone at a fraction of the level
/// of somebody talking into it — 0.045 against 0.6 on the scale here — so the
/// gate separates them cleanly even though both are real speech. Words that
/// arrive while the microphone has been quiet belong to the room, not to the
/// person, and are dropped — see [`Self::words_if_heard`].
///
/// What remains is robust to a microphone whose gain moves, because the
/// *ending* no longer depends on a threshold, and robust to a room with other
/// voices in it, because starting still does.
///
/// Loudness is also what draws the meter, and the whole rule when no live
/// model is installed.
#[derive(Debug, Clone)]
pub struct Spoken {
    /// No new words for this long ends the utterance. Must comfortably exceed
    /// one chunk of the model plus the time it takes to run.
    pub quiet_ms: u32,
    /// No words at all for this long gives up.
    pub patience_ms: u32,
    /// Stop listening after this whatever happens.
    pub max_ms: u32,

    heard_words: bool,
    since_words_ms: u32,
    total_ms: u32,
}

impl Default for Spoken {
    fn default() -> Self {
        Self {
            quiet_ms: 1_200,
            patience_ms: 8_000,
            max_ms: 120_000,
            heard_words: false,
            since_words_ms: 0,
            total_ms: 0,
        }
    }
}

impl Spoken {
    /// The live model produced something, and the microphone agrees there was
    /// something to produce it from.
    ///
    /// `heard_sound` is the loudness signal's opinion: it has heard enough
    /// over its gate this utterance, and not so long ago that these words
    /// cannot be about it. A television in the same room passes neither test,
    /// because it never gets near the level of a person at the microphone.
    pub fn words_if_heard(&mut self, heard_sound: bool) -> bool {
        if !heard_sound {
            return false;
        }
        self.heard_words = true;
        self.since_words_ms = 0;
        true
    }

    pub fn has_words(&self) -> bool {
        self.heard_words
    }

    /// How long since the last words arrived.
    pub fn quiet_for(&self) -> u32 {
        self.since_words_ms
    }

    /// Take a block of audio's worth of time and say what to do about it.
    pub fn push(&mut self, span_ms: u32) -> Heard {
        self.total_ms = self.total_ms.saturating_add(span_ms);
        self.since_words_ms = self.since_words_ms.saturating_add(span_ms);

        if self.total_ms >= self.max_ms {
            return if self.heard_words {
                Heard::Overlong
            } else {
                Heard::NothingSaid
            };
        }
        if self.heard_words {
            if self.since_words_ms >= self.quiet_ms {
                return Heard::Ended;
            }
            return Heard::Speaking;
        }
        if self.total_ms >= self.patience_ms {
            return Heard::NothingSaid;
        }
        Heard::Quiet
    }
}

/// A chat a spoken question could belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recent {
    pub id: String,
    pub title: String,
    /// When the last spoken exchange in it ended.
    pub spoke_at: DateTime<Utc>,
}

/// Which chat this question goes into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Going {
    /// Carry on the one that was just spoken in.
    On { id: String, title: String },
    /// Start a new one.
    Fresh,
}

/// Continue the last spoken chat, or start a new one.
///
/// A follow-up window and nothing cleverer. The alternative designs both lose:
/// one eternal voice chat grows without bound and drags every unrelated topic
/// into the same fold, and a model asked "does this belong with that?" costs a
/// round trip on the one path where latency is the whole product — and is wrong
/// silently, which is worse than being wrong visibly. What the window cannot
/// decide, the person can: the overlay names the chat it is continuing and the
/// same shortcut that started this can start a new one instead.
///
/// `minutes` of zero means every spoken question starts its own chat.
pub fn continuation(last: Option<&Recent>, now: DateTime<Utc>, minutes: i64) -> Going {
    let Some(last) = last else {
        return Going::Fresh;
    };
    if minutes <= 0 {
        return Going::Fresh;
    }
    // A clock that went backwards — a laptop waking with a corrected time —
    // must not be read as "no time has passed".
    let since = now.signed_duration_since(last.spoke_at);
    if since >= Duration::zero() && since <= Duration::minutes(minutes) {
        Going::On {
            id: last.id.clone(),
            title: last.title.clone(),
        }
    } else {
        Going::Fresh
    }
}

/// What a spoken question carries into the request, after the question itself.
///
/// On the question rather than in the system prompt on purpose: see the module
/// header. It is written as a fact about this turn rather than a standing
/// instruction, because that is what it is — the next turn in the same chat may
/// well be typed.
pub const REGISTER: &str = "\
(This question was spoken aloud and the answer will be read aloud by a speech \
synthesiser. Answer in plain spoken prose: no headings, no bullet lists, no \
tables, no code blocks, no markdown of any kind, and no URLs read out \
character by character. Lead with the answer in one or two sentences and stop \
there unless more was asked for. If the honest answer needs a table or code, \
say so in a sentence and put it in the chat rather than reading it out.)";

/// The question as it goes to the model.
pub fn asked_aloud(question: &str) -> String {
    format!("{question}\n\n{REGISTER}")
}

/// Turns a streamed answer into sentences worth speaking, as they arrive.
///
/// Speaking cannot wait for the whole answer — that is most of the latency
/// budget — and it cannot speak every fragment either, because a synthesiser
/// handed three words at a time has no prosody left. So: emit at sentence ends,
/// once there is enough to be worth a breath.
#[derive(Debug, Default)]
pub struct Reading {
    buffer: String,
    /// Inside a fenced code block, which is never spoken.
    fenced: bool,
    /// A block was passed over, so the overlay can say so.
    skipped_code: bool,
}

/// Below this, a sentence end is not worth breaking on — "Yes." followed half a
/// second later by the rest reads as a stutter.
const WORTH_SPEAKING: usize = 40;

impl Reading {
    /// Take a delta of the answer and return whatever is now sayable.
    pub fn push(&mut self, delta: &str) -> Vec<String> {
        self.buffer.push_str(delta);
        let mut out = Vec::new();
        while let Some(sentence) = self.take_sentence() {
            let spoken = spoken(&sentence);
            if !spoken.is_empty() {
                out.push(spoken);
            }
        }
        out
    }

    /// Everything left, at the end of the answer.
    pub fn flush(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.buffer);
        let (kept, fenced) = strip_fences(&rest, self.fenced);
        self.fenced = fenced;
        self.skipped_code |= kept.len() != rest.len();
        let spoken = spoken(&kept);
        (!spoken.is_empty()).then_some(spoken)
    }

    pub fn skipped_code(&self) -> bool {
        self.skipped_code
    }

    /// The next complete sentence, if the buffer holds one.
    fn take_sentence(&mut self) -> Option<String> {
        // A fence anywhere in the buffer is handled before sentences are
        // looked for, so a code block never becomes something to say.
        let (kept, fenced) = strip_fences(&self.buffer, self.fenced);
        if kept.len() != self.buffer.len() {
            self.skipped_code = true;
            self.buffer = kept;
            self.fenced = fenced;
        }
        if self.fenced {
            // Still inside a block. Nothing to say until it closes.
            return None;
        }

        let bytes = self.buffer.as_bytes();
        let mut end = None;
        for (index, byte) in bytes.iter().enumerate() {
            let terminator = matches!(byte, b'.' | b'!' | b'?' | b'\n' | b':' | b';');
            if !terminator {
                continue;
            }
            // A terminator has to be followed by whitespace to be one: the dot
            // in "1.5" and the one in "e.g." are not sentence ends.
            let next = bytes.get(index + 1);
            let breaks = match next {
                None => false, // more may yet arrive; wait for it
                Some(b) => b.is_ascii_whitespace(),
            };
            if (*byte == b'\n' || breaks) && index + 1 >= WORTH_SPEAKING {
                end = Some(index + 1);
                break;
            }
        }

        let end = end?;
        let sentence: String = self.buffer.drain(..end).collect();
        Some(sentence)
    }
}

/// Remove any fenced code from `text`, saying whether it ends inside a fence.
fn strip_fences(text: &str, mut fenced: bool) -> (String, bool) {
    if !fenced && !text.contains("```") {
        return (text.to_string(), false);
    }
    let mut kept = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if !fenced {
            kept.push_str(line);
        }
    }
    (kept, fenced)
}

/// An answer written for a screen, as something worth hearing.
///
/// Markdown read aloud is unbearable — "star star important star star" — and a
/// URL read aloud is worse. The model is asked not to write any (see
/// [`REGISTER`]), and this is what happens when it does anyway.
pub fn spoken(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let trimmed = line.trim();
        // A table row aloud is a list of pipes. The model was told; if it
        // writes one anyway, saying nothing is better than saying that.
        if trimmed.starts_with('|') {
            continue;
        }
        let mut line = trimmed.trim_start_matches('>').trim_start();
        // Headings and bullets are structure, not words.
        line = line.trim_start_matches('#').trim_start();
        if let Some(rest) = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("+ "))
        {
            line = rest;
        }
        // A horizontal rule is not a sentence.
        if !line.is_empty() && line.chars().all(|c| c == '-' || c == '*' || c == '_') {
            continue;
        }
        let line = inline(line);
        if line.trim().is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(line.trim());
    }
    out
}

/// Inline markdown, flattened: links become their text, emphasis and code
/// markers are dropped.
fn inline(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // `[text](url)` — the text is what a person would say.
            '[' => {
                let mut text = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == ']' {
                        closed = true;
                        break;
                    }
                    text.push(c);
                }
                out.push_str(&text);
                if closed && chars.peek() == Some(&'(') {
                    // Swallow the target. Nobody wants an address read out.
                    for c in chars.by_ref() {
                        if c == ')' {
                            break;
                        }
                    }
                }
            }
            '*' | '_' | '`' | '#' => {}
            _ => out.push(c),
        }
    }
    out
}

/// What a bare transcript needs before it is a question.
///
/// Speech recognition returns a stray fragment when somebody clears their
/// throat at the microphone, and sending "uh" as a turn wastes a round trip and
/// puts a nonsense exchange in the chat.
pub fn is_a_question(transcript: &str) -> bool {
    let trimmed = transcript.trim();
    if trimmed.chars().filter(|c| c.is_alphanumeric()).count() < 2 {
        return false;
    }
    // One filler word on its own is not a question. Two words is.
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    if words.len() == 1 {
        let word = words[0]
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        return !matches!(
            word.as_str(),
            "uh" | "um" | "hmm" | "mm" | "ah" | "er" | "eh" | "oh" | "hm" | "okay" | "ok"
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn silence(endpointer: &mut Endpointer, blocks: usize) -> Heard {
        let mut last = Heard::Quiet;
        for _ in 0..blocks {
            last = endpointer.push(0.05, BLOCK_MS);
        }
        last
    }

    fn speech(endpointer: &mut Endpointer, blocks: usize) -> Heard {
        let mut last = Heard::Quiet;
        for _ in 0..blocks {
            last = endpointer.push(0.45, BLOCK_MS);
        }
        last
    }

    #[test]
    fn a_quiet_room_keeps_the_gate_under_ordinary_speech() {
        // The gate has to end up somewhere between the room and a voice. Both
        // ends matter: too low and room noise reads as speech and the utterance
        // never ends, too high and the speaker is never heard at all.
        let mut endpointer = Endpointer::default();
        silence(&mut endpointer, 10);
        assert!(
            endpointer.gate() > 0.05,
            "the room itself must not read as speech, gate was {}",
            endpointer.gate()
        );
        assert!(
            endpointer.gate() < 0.34,
            "ordinary speech has to clear it, gate was {}",
            endpointer.gate()
        );
    }

    #[test]
    fn a_microphone_that_emits_true_silence_does_not_collapse_the_room() {
        // Measured on this desk's webcam: a tenth of the blocks in a *silent*
        // room read exactly 0.000, while the room's actual level is 0.124 with
        // peaks to 0.385. A rolling minimum reads 0.017 there — so the gate
        // sat at its floor of 0.20 while 15% of silent-room blocks were above
        // it, silence never accumulated to the hangover, and the microphone
        // stayed open for two minutes. This is that room, and the estimate has
        // to land near what it actually sounds like.
        let mut endpointer = Endpointer::default();
        for block in 0..75 {
            let level = match block % 10 {
                0 => 0.000, // the gate closing entirely
                9 => 0.385, // a peak
                _ => 0.124, // the room
            };
            endpointer.push(level, BLOCK_MS);
        }
        let room = endpointer.room().expect("blocks were pushed");
        assert!(
            room > 0.05,
            "one block in ten at zero must not read as a silent room, got {room}"
        );
        assert!(
            endpointer.gate() > 0.221,
            "the gate has to clear this room's p90 of 0.221, was {}",
            endpointer.gate()
        );
    }

    #[test]
    fn the_room_is_never_credited_with_words_the_model_produced() {
        // The measured failure: `has_speech()` is cumulative, so a few seconds
        // of any real room made it true forever, and every word the streaming
        // model hallucinated on room noise then reset the utterance clock.
        // Twenty of thirty-one chunks of a *silent* room were credited to the
        // speaker and the microphone never closed.
        let mut endpointer = Endpointer::default();
        for block in 0..150 {
            endpointer.push(if block % 25 == 0 { 0.385 } else { 0.124 }, BLOCK_MS);
            assert!(
                !endpointer.heard_you(),
                "block {block} of a silent room was credited to the speaker \
                 ({} ms over a gate of {:.2})",
                endpointer.speech_in_window_ms(),
                endpointer.gate()
            );
        }
        // And the person talking is credited immediately.
        speech(&mut endpointer, 12);
        assert!(
            endpointer.heard_you(),
            "480 ms of speech has to count, {} ms over a gate of {:.2}",
            endpointer.speech_in_window_ms(),
            endpointer.gate()
        );
    }

    #[test]
    fn an_utterance_in_that_room_still_ends() {
        // The failure this whole change is about, end to end: a room noisy
        // enough that the old gate spent 15% of its silence above it. Ending
        // has to survive the noise bursts in the gaps.
        // The duty cycle is the measurement, and getting it wrong is how this
        // test first passed a broken gate and then failed a working one: at a
        // gate of 0.32 the real room is above it for 4% of its blocks, in runs
        // of at most three. One peak per 25 blocks is that. A fixture with a
        // third of its blocks above the gate is not a noisy room, it is
        // somebody talking, and no endpointer should call it silence.
        // Stops at the first decision, because that is what the application
        // does — it calls `stop_listening` and pushes nothing more. A helper
        // that reports only the last block hides an `Ended` that fired in the
        // middle and then had its silence counter reset by the next noise
        // burst, which is a pass reported as a failure.
        let room = |endpointer: &mut Endpointer, blocks: usize| {
            let mut last = Heard::Quiet;
            for block in 0..blocks {
                let level = if block % 25 == 0 { 0.385 } else { 0.124 };
                last = endpointer.push(level, BLOCK_MS);
                if !matches!(last, Heard::Quiet | Heard::Speaking) {
                    break;
                }
            }
            last
        };
        let mut endpointer = Endpointer::default();
        room(&mut endpointer, 40);
        speech(&mut endpointer, 25);
        assert_eq!(
            room(&mut endpointer, 30),
            Heard::Ended,
            "gate was {}",
            endpointer.gate()
        );
    }

    #[test]
    fn a_noisy_room_lifts_the_gate_above_the_noise() {
        let mut endpointer = Endpointer::default();
        for _ in 0..10 {
            endpointer.push(0.30, BLOCK_MS);
        }
        assert!(
            endpointer.gate() > 0.30,
            "a fan at 0.30 must not read as speech, gate was {}",
            endpointer.gate()
        );
    }

    #[test]
    fn speech_then_silence_ends_the_utterance() {
        let mut endpointer = Endpointer::default();
        silence(&mut endpointer, 6);
        assert_eq!(speech(&mut endpointer, 25), Heard::Speaking);
        // A short gap mid-sentence is not the end.
        assert_eq!(silence(&mut endpointer, 10), Heard::Speaking);
        assert_eq!(speech(&mut endpointer, 10), Heard::Speaking);
        // 800 ms is twenty blocks.
        assert_eq!(silence(&mut endpointer, 20), Heard::Ended);
    }

    #[test]
    fn talking_from_the_very_first_block_still_ends() {
        // What people actually do: press the key and start talking. There is
        // no quiet moment to measure the room from, and a gate calibrated
        // from the opening word sits above the whole utterance — no speech is
        // ever detected, nothing ever ends, and the microphone stays open
        // until the cap. That was the first thing anybody hit.
        let mut endpointer = Endpointer::default();
        speech(&mut endpointer, 50);
        assert!(endpointer.has_speech(), "gate was {}", endpointer.gate());
        assert_eq!(silence(&mut endpointer, 20), Heard::Ended);
    }

    #[test]
    fn a_room_that_gets_louder_is_still_silence() {
        // Switch a webcam's noise suppression off, or move somewhere noisier,
        // and the ambient level rises. A floor that only ever falls is then
        // pinned below the room by whatever the quietest moment was, every
        // block afterwards counts as speech, and it never notices anybody
        // stopped talking. This is that, and it has to recover on its own.
        let mut endpointer = Endpointer::default();
        silence(&mut endpointer, 10); // a quiet start pins the old floor low
        let quiet = endpointer.gate();

        // The room is now noisier than the gate that quiet moment set.
        for _ in 0..(3_000 / BLOCK_MS as usize) {
            endpointer.push(0.28, BLOCK_MS);
        }
        assert!(
            endpointer.gate() > 0.28,
            "the new room has to read as silence, gate was {}",
            endpointer.gate()
        );
        assert!(quiet < endpointer.gate(), "and it has to have moved");

        // And an utterance in that room still ends.
        speech(&mut endpointer, 25);
        assert_eq!(
            {
                let mut last = Heard::Quiet;
                for _ in 0..30 {
                    last = endpointer.push(0.28, BLOCK_MS);
                }
                last
            },
            Heard::Ended
        );
    }

    #[test]
    fn a_room_that_gets_quieter_takes_the_gate_down_with_it() {
        // The room is only ever revised downwards, so a burst of noise at the
        // start cannot deafen it for the rest of the utterance.
        let mut endpointer = Endpointer::default();
        for _ in 0..5 {
            endpointer.push(0.8, BLOCK_MS);
        }
        let loud = endpointer.gate();
        silence(&mut endpointer, 15);
        assert!(
            endpointer.gate() < loud,
            "gate was {} and is now {}",
            loud,
            endpointer.gate()
        );
    }

    #[test]
    fn a_cough_is_not_an_utterance() {
        let mut endpointer = Endpointer::default();
        silence(&mut endpointer, 6);
        speech(&mut endpointer, 3); // 120 ms, under the minimum
        assert_ne!(silence(&mut endpointer, 25), Heard::Ended);
    }

    #[test]
    fn a_cough_and_then_nothing_gives_up_rather_than_hanging() {
        // The gap between the two rules that used to exist: too little speech
        // to send, but not *no* speech, so neither Ended nor NothingSaid ever
        // fired and the microphone stayed open for two minutes.
        let mut endpointer = Endpointer::default();
        silence(&mut endpointer, 6);
        speech(&mut endpointer, 3);
        let outcome = silence(&mut endpointer, 8_000 / BLOCK_MS as usize + 1);
        assert_eq!(outcome, Heard::NothingSaid);
    }

    #[test]
    fn saying_nothing_gives_up_rather_than_listening_forever() {
        let mut endpointer = Endpointer::default();
        let outcome = silence(&mut endpointer, 8_000 / BLOCK_MS as usize + 1);
        assert_eq!(outcome, Heard::NothingSaid);
    }

    #[test]
    fn talking_forever_is_cut_off_with_what_there_is() {
        let mut endpointer = Endpointer {
            max_ms: 1_000,
            ..Endpointer::default()
        };
        silence(&mut endpointer, 6);
        assert_eq!(speech(&mut endpointer, 40), Heard::Overlong);
    }

    #[test]
    fn a_long_stretch_of_nothing_is_not_reported_as_an_utterance() {
        // The cut-off and "nobody said anything" are different outcomes and
        // the caller does different things with them.
        let mut endpointer = Endpointer {
            max_ms: 1_000,
            patience_ms: 10_000,
            ..Endpointer::default()
        };
        assert_eq!(silence(&mut endpointer, 40), Heard::NothingSaid);
    }

    fn feed(barge: &mut Barge, level: f64, blocks: usize) -> bool {
        let mut fired = false;
        for _ in 0..blocks {
            fired |= barge.push(level, BLOCK_MS);
        }
        fired
    }

    #[test]
    fn its_own_voice_is_not_an_interruption() {
        // The answer starting is the background going from a quiet room to
        // speech coming off the speakers. Heard as an interruption, the
        // assistant shuts itself up on its own first syllable.
        let mut barge = Barge::default();
        assert!(!feed(&mut barge, 0.05, 5), "the room before it speaks");
        assert!(!feed(&mut barge, 0.40, 50), "two seconds of its own voice");
    }

    #[test]
    fn talking_over_it_is_an_interruption() {
        let mut barge = Barge::default();
        feed(&mut barge, 0.40, 50); // it is speaking, echo learned
        assert!(
            feed(&mut barge, 0.62, 10),
            "somebody louder than the echo, for 400 ms"
        );
    }

    #[test]
    fn a_quiet_room_does_not_make_everything_an_interruption() {
        // While it is thinking there is no echo to be heard over, so the
        // background is near silence — and a margin above near silence is
        // cleared by anything at all. Firing here abandons the turn the
        // person is waiting for, which is the most expensive false positive
        // there is, so an absolute floor sits under the margin.
        //
        // Where that floor sits is a real trade. Low enough that ordinary
        // talking clears it, which is the whole point of the feature, and
        // therefore not so high that a keyboard against the microphone never
        // could. Being interrupted by your own typing is recoverable; not
        // being able to interrupt at all is the thing this exists to fix.
        let mut barge = Barge::default();
        feed(&mut barge, 0.04, 20);
        assert!(
            !feed(&mut barge, 0.18, 20),
            "a mug on a desk is not a voice"
        );
        assert!(feed(&mut barge, 0.40, 10), "an ordinary voice is");
    }

    #[test]
    fn ordinary_speech_interrupts_without_being_shouted() {
        // Speech is loud about half the time and nearly silent between
        // syllables. Requiring an unbroken run of loud blocks means only a
        // shout gets through, which is what happened the first time.
        let mut barge = Barge::default();
        feed(&mut barge, 0.35, 40); // it is speaking; the echo is learned
        let mut fired = false;
        for block in 0..30 {
            // Alternating loud and quiet, as a voice actually arrives.
            let level = if block % 2 == 0 { 0.46 } else { 0.20 };
            fired |= barge.push(level, BLOCK_MS);
        }
        assert!(fired, "a normal voice has to be able to interrupt");
    }

    #[test]
    fn a_single_loud_block_is_not_an_interruption() {
        // A door, a cough, a key press. Wanting 200 ms of it is what tells
        // those apart from a word.
        let mut barge = Barge::default();
        feed(&mut barge, 0.40, 50);
        assert!(!feed(&mut barge, 0.9, 2), "80 ms is not a word");
    }

    #[test]
    fn talking_while_it_thinks_interrupts_immediately_after_settling() {
        // Nothing is playing, so the background is the room and a voice is
        // far above it.
        let mut barge = Barge::default();
        feed(&mut barge, 0.06, 20);
        assert!(feed(&mut barge, 0.45, 6));
    }

    #[test]
    fn the_settle_period_covers_the_start_of_speech() {
        // Nothing can trigger before the background has been learned at all.
        let mut barge = Barge::default();
        assert!(!feed(&mut barge, 0.95, 600 / BLOCK_MS as usize - 1));
    }

    fn wait(spoken: &mut Spoken, ms: u32) -> Heard {
        let mut last = Heard::Quiet;
        for _ in 0..(ms / BLOCK_MS) {
            last = spoken.push(BLOCK_MS);
        }
        last
    }

    #[test]
    fn words_then_a_pause_ends_the_utterance() {
        let mut spoken = Spoken::default();
        assert_eq!(wait(&mut spoken, 400), Heard::Quiet);
        assert!(spoken.words_if_heard(true));
        assert_eq!(wait(&mut spoken, 400), Heard::Speaking);
        assert!(spoken.words_if_heard(true));
        assert_eq!(wait(&mut spoken, 1_200), Heard::Ended);
    }

    #[test]
    fn somebody_elses_speech_is_not_yours() {
        // A video playing across the room transcribes perfectly well and is
        // nothing to do with the person at the microphone. Taken at face
        // value it never stops resetting the clock, so the utterance never
        // ends and the microphone stays open until the cap.
        let mut spoken = Spoken::default();
        for _ in 0..20 {
            assert!(!spoken.words_if_heard(false));
            wait(&mut spoken, 400);
        }
        assert!(!spoken.has_words(), "a silent room said nothing");
        assert_eq!(wait(&mut spoken, 8_040), Heard::NothingSaid);
    }

    #[test]
    fn the_room_cannot_hold_a_finished_utterance_open() {
        // The dangerous shape: somebody asks a question, stops, and the video
        // they left playing carries on being transcribed.
        let mut spoken = Spoken::default();
        assert!(spoken.words_if_heard(true));
        wait(&mut spoken, 400);
        for _ in 0..10 {
            assert!(!spoken.words_if_heard(false));
            wait(&mut spoken, 200);
        }
        assert!(
            matches!(spoken.push(BLOCK_MS), Heard::Ended),
            "quiet for {} ms and still going",
            spoken.quiet_for()
        );
    }

    #[test]
    fn a_gap_between_two_chunks_is_not_the_end() {
        // The live model works in 560 ms chunks, so words arrive in bursts
        // with most of a second between them even while somebody is talking
        // without pause. Ending on that gap cuts people off mid-sentence,
        // which is what the loudness gate kept doing.
        let mut spoken = Spoken::default();
        spoken.words_if_heard(true);
        assert_eq!(wait(&mut spoken, 700), Heard::Speaking);
        spoken.words_if_heard(true);
        assert_eq!(wait(&mut spoken, 700), Heard::Speaking);
        assert!(
            spoken.quiet_ms > 560 + 200,
            "the threshold has to clear a chunk and the time to run it"
        );
    }

    #[test]
    fn nothing_said_is_words_never_arriving() {
        // And nothing else. However loud the room is, however quiet the
        // speaker: if the model produced no words, nobody spoke.
        let mut spoken = Spoken::default();
        assert_eq!(wait(&mut spoken, 8_040), Heard::NothingSaid);
    }

    #[test]
    fn a_long_silence_after_words_is_still_an_utterance() {
        // The old rule counted silence against the speaker and could decide
        // nothing had been said while their words were on screen.
        let mut spoken = Spoken::default();
        spoken.words_if_heard(true);
        assert_eq!(wait(&mut spoken, 20_000), Heard::Ended);
        assert!(spoken.has_words());
    }

    #[test]
    fn talking_forever_is_cut_off_with_what_there_is_by_words_too() {
        let mut spoken = Spoken {
            max_ms: 1_000,
            ..Spoken::default()
        };
        spoken.words_if_heard(true);
        assert_eq!(wait(&mut spoken, 1_040), Heard::Overlong);
    }

    #[test]
    fn a_recent_chat_is_carried_on() {
        let now = Utc::now();
        let last = Recent {
            id: "7".into(),
            title: "The deploy".into(),
            spoke_at: now - Duration::minutes(3),
        };
        assert_eq!(
            continuation(Some(&last), now, 8),
            Going::On {
                id: "7".into(),
                title: "The deploy".into()
            }
        );
    }

    #[test]
    fn an_old_chat_is_left_alone() {
        let now = Utc::now();
        let last = Recent {
            id: "7".into(),
            title: "The deploy".into(),
            spoke_at: now - Duration::minutes(40),
        };
        assert_eq!(continuation(Some(&last), now, 8), Going::Fresh);
    }

    #[test]
    fn a_window_of_zero_always_starts_a_new_chat() {
        let now = Utc::now();
        let last = Recent {
            id: "7".into(),
            title: "The deploy".into(),
            spoke_at: now,
        };
        assert_eq!(continuation(Some(&last), now, 0), Going::Fresh);
    }

    #[test]
    fn a_clock_that_went_backwards_does_not_continue_forever() {
        let now = Utc::now();
        let last = Recent {
            id: "7".into(),
            title: "The deploy".into(),
            // Stamped in the future: the machine's clock was corrected.
            spoke_at: now + Duration::hours(2),
        };
        assert_eq!(continuation(Some(&last), now, 8), Going::Fresh);
    }

    #[test]
    fn the_register_rides_on_the_question_not_the_prompt() {
        let asked = asked_aloud("what did I say about the deploy");
        assert!(asked.starts_with("what did I say about the deploy"));
        assert!(asked.contains("spoken aloud"));
    }

    #[test]
    fn markdown_is_flattened_for_the_synthesiser() {
        assert_eq!(spoken("## The answer"), "The answer");
        assert_eq!(spoken("- one\n- two"), "one two");
        assert_eq!(spoken("**very** `important`"), "very important");
        assert_eq!(
            spoken("see [the design doc](https://example.com/a/b) for it"),
            "see the design doc for it"
        );
        assert_eq!(spoken("| a | b |\n| - | - |"), "");
        assert_eq!(spoken("> quoted"), "quoted");
        assert_eq!(spoken("---"), "");
    }

    #[test]
    fn a_sentence_is_spoken_as_soon_as_it_is_whole() {
        let mut reading = Reading::default();
        assert!(reading.push("The deploy finished at four").is_empty());
        let said = reading.push(" and nothing failed. The next one is Friday");
        assert_eq!(
            said,
            vec!["The deploy finished at four and nothing failed."]
        );
    }

    #[test]
    fn a_short_fragment_waits_for_the_rest() {
        // "Yes." on its own, then the reason, is a stutter.
        let mut reading = Reading::default();
        assert!(reading.push("Yes. ").is_empty());
        let said = reading.push("The deploy went out at four this afternoon. ");
        assert_eq!(
            said,
            vec!["Yes. The deploy went out at four this afternoon."]
        );
    }

    #[test]
    fn a_decimal_point_is_not_a_sentence_end() {
        let mut reading = Reading::default();
        assert!(reading
            .push("The build took 1.5 hours which is under the budget")
            .is_empty());
    }

    #[test]
    fn a_code_block_is_passed_over_rather_than_read_out() {
        let mut reading = Reading::default();
        reading.push("Here is the fix, roughly speaking.\n");
        reading.push("```rust\nfn main() { println!(\"no\"); }\n```\n");
        let tail = reading.push("It goes in the loader and nowhere else.\n");
        let all = tail.join(" ");
        assert!(!all.contains("println"), "got {all}");
        assert!(reading.skipped_code());
    }

    #[test]
    fn an_unterminated_code_block_does_not_leak_at_the_end() {
        let mut reading = Reading::default();
        reading.push("Try this:\n```\ncargo test\n");
        let tail = reading.flush().unwrap_or_default();
        assert!(!tail.contains("cargo test"), "got {tail}");
    }

    #[test]
    fn the_tail_of_an_answer_is_still_spoken() {
        let mut reading = Reading::default();
        reading.push("It is done");
        assert_eq!(reading.flush().as_deref(), Some("It is done"));
        assert_eq!(reading.flush(), None);
    }

    #[test]
    fn a_throat_clearing_is_not_a_question() {
        assert!(!is_a_question(""));
        assert!(!is_a_question("  "));
        assert!(!is_a_question("uh"));
        assert!(!is_a_question("Um."));
        assert!(!is_a_question("a"));
        assert!(is_a_question("why"));
        assert!(is_a_question("um what did I say"));
    }
}
