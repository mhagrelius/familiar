//! The fold: a stream of frames becomes one turn.
//!
//! A turn arrives as fragments and is folded into structured state exactly
//! once, here. Above this module nothing knows what a chunk is; it sees
//! [`TurnState`] — thinking, answer, tool calls, time to first token, and the
//! numbers llama.cpp reported. The reverse fold, `TurnState` into a widget,
//! lives in `ui::turn_view`, and both the live stream and a thread loaded from
//! disk go through it, so a reopened conversation cannot look different from
//! the one you just had.
//!
//! Failure is an [`Event`], not an exception. A stream can deliver three good
//! frames and then a bad one, and the caller needs the three — so `push`
//! returns what happened in order and lets the caller decide when to stop.

use std::cell::Cell;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::wire::{self, Sse, SseDecoder, Timings, Usage, WireError};

/// Monotonic time, injected so the fold is testable without sleeping.
pub trait Clock {
    /// Elapsed time from an arbitrary origin. Only differences are meaningful.
    fn now(&self) -> Duration;
}

/// The real one.
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// A clock that only moves when told to. Public because the integration tests
/// drive whole turns through it.
#[derive(Debug, Default)]
pub struct ManualClock {
    elapsed: Cell<Duration>,
}

impl ManualClock {
    pub fn advance(&self, by: Duration) {
        self.elapsed.set(self.elapsed.get() + by);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        self.elapsed.get()
    }
}

/// Why the model stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Finish {
    Stop,
    /// Ran into `max_tokens` — the answer is cut off, and the UI says so.
    Length,
    ToolCalls,
    /// Escape, or the stop button.
    Cancelled,
    Other(String),
}

impl Finish {
    fn from_reason(reason: &str) -> Self {
        match reason {
            "stop" => Self::Stop,
            "length" => Self::Length,
            "tool_calls" | "function_call" => Self::ToolCalls,
            other => Self::Other(other.to_string()),
        }
    }
}

/// What a tool call did. Kept beside the call, because a chip that says "done"
/// while the tool returned an error is worse than no chip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    Ok(String),
    Failed(String),
    /// Refused at the approval dialog.
    Denied,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// A JSON object as a string, accumulated fragment by fragment. It is not
    /// valid JSON until the call is complete.
    pub arguments: String,
    /// The arguments stopped arriving, so the call can be run.
    #[serde(default)]
    pub complete: bool,
    #[serde(default)]
    pub outcome: Option<ToolOutcome>,
}

impl ToolCall {
    /// The one argument worth putting on a chip: the query, the path, the URL.
    /// Whichever it has, in that order, or nothing while the JSON is still
    /// arriving.
    pub fn primary_argument(&self) -> Option<String> {
        let parsed: serde_json::Value = serde_json::from_str(&self.arguments).ok()?;
        for key in ["query", "url", "path", "subject", "name", "names"] {
            match parsed.get(key) {
                Some(serde_json::Value::String(value)) => return Some(value.clone()),
                // `use_tools` takes a list, and a chip reading only `use_tools`
                // hides the one thing anybody would want to know about it.
                Some(serde_json::Value::Array(values))
                    if values.iter().all(serde_json::Value::is_string) =>
                {
                    let joined = values
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ");
                    if !joined.is_empty() {
                        return Some(joined);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// The arguments as something a person can read, one field at a time.
    ///
    /// The chip carries one value because there is room for one; this is what
    /// is behind it. JSON is what the model sent and it is not what anybody
    /// wants to look at, so the object is unpacked: keys become labels, an
    /// argv becomes a command line, and anything genuinely nested falls back to
    /// indented JSON rather than being flattened into nonsense.
    ///
    /// Arguments that never finished arriving are not an error here. A call
    /// still streaming has half an object in it, and the honest answer is the
    /// raw text under one field rather than an empty list that reads as "no
    /// arguments".
    pub fn fields(&self) -> Vec<Field> {
        let text = self.arguments.trim();
        if text.is_empty() {
            return Vec::new();
        }
        let Ok(serde_json::Value::Object(parsed)) = serde_json::from_str(text) else {
            return vec![Field::new("Arguments", text)];
        };
        parsed
            .iter()
            .map(|(key, value)| Field::new(label_for(key), render(value)))
            .collect()
    }
}

/// One argument, ready to be drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// The key, as a person would write it: `related_to` becomes "Related to".
    pub key: String,
    pub value: String,
}

impl Field {
    fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }

    /// Whether this wants a block of its own rather than a row.
    ///
    /// A script, a file's contents and a page of Markdown are all arguments,
    /// and all three are unreadable squeezed into a subtitle.
    pub fn is_block(&self) -> bool {
        self.value.contains('\n') || self.value.chars().count() > 80
    }
}

/// A JSON key as a label: underscores out, first letter up.
fn label_for(key: &str) -> String {
    let spaced = key.replace('_', " ");
    let mut characters = spaced.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => spaced,
    }
}

/// One JSON value as text.
fn render(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => "none".to_string(),
        // An argv reads as the command line it is about to become — which is
        // also how the user will recognise it from a terminal.
        serde_json::Value::Array(items) if items.iter().all(serde_json::Value::is_string) => items
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
        }
        other => other.to_string(),
    }
}

/// What the model is told as the last round of tools comes back.
///
/// Without it, hitting the ceiling settles the turn with whatever answer text
/// happened to have accumulated — which for a model mid-chain is usually none,
/// so the user gets a row of chips and no reply. Telling it to conclude turns a
/// truncation into an ending.
///
/// Here rather than beside the ceiling in `ui::application` because the eval
/// harness has to send the same sentence at the same moment. It did not, and
/// the difference was not small: a spiral that the application ends with an
/// honest "I could not confirm the version" was scored as a turn that answered
/// nothing at all.
pub const LAST_ROUND: &str =
    "That was the last round of tools available for this turn. Answer now with what you have, \
     and say plainly what you could not finish.";

/// How many calls a turn may make before it is asked to start wrapping up.
///
/// Taken from the longest legitimate chain in the suite: reading a couple of
/// files, reading a skill and writing a document is five or six. Past that the
/// traces stop looking like work and start looking like a hunt.
pub const WRAP_UP_AFTER: usize = 6;

/// The nudge sent once a turn passes [`WRAP_UP_AFTER`] calls without answering.
///
/// This fills the gap between the tool budgets, which are per capability, and
/// the round ceiling, which only speaks at the very end. `web::Budget` stops a
/// search spiral and has nothing to say about seven `gh` calls; the ceiling
/// catches those, but only once the turn is already over. "Thrashed on one
/// tool" was the second commonest antipattern across the whole suite, spread
/// across families with nothing else in common — which is what a missing
/// general rule looks like.
///
/// Deliberately a nudge rather than a refusal. A long chain is sometimes
/// exactly right, and a turn genuinely finishing a document on its seventh call
/// should be allowed to.
pub const WRAP_UP: &str =
    "You have made several tool calls this turn and have not answered yet. Unless the next one \
     actually finishes the job, stop looking and write the answer with what you already have — \
     including, plainly, whatever you could not find out. Another angle on something two calls \
     have already missed is not going to be the one that works.";

/// A turn, folded. Live state, not the persisted shape: `thread::StoredTurn`
/// is what reaches the disk.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnState {
    pub thinking: String,
    pub answer: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish: Option<Finish>,
    pub usage: Option<Usage>,
    pub timings: Option<Timings>,
    /// Time to the first token of anything — thinking counts, because that is
    /// when the model started answering you.
    pub time_to_first_token: Option<Duration>,
    /// How long the model spent thinking before it started answering, if it
    /// thought at all. What the disclosure reports: "Thought for 4s".
    pub thinking_elapsed: Option<Duration>,
    pub elapsed: Duration,
    /// How many of `tool_calls` were rescued from the thinking because the
    /// server did not parse them. Counted so the eval can score the upstream
    /// bug separately from the model's own behaviour — a turn that only worked
    /// because of the rescue is not the same as one that never needed it.
    pub recovered_calls: usize,
}

impl TurnState {
    /// Nothing came back at all. A thread is not created for one of these.
    pub fn is_empty(&self) -> bool {
        self.thinking.is_empty() && self.answer.is_empty() && self.tool_calls.is_empty()
    }

    pub fn metrics(&self) -> TurnMetrics {
        TurnMetrics::of(self)
    }
}

/// What `push` did with what it was given.
///
/// Text events carry the fragment, not the accumulated string, so the view
/// appends rather than re-rendering a growing answer on every token.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Thinking(String),
    Answer(String),
    /// The call at this index started or grew.
    ToolCall(usize),
    /// Usage or timings landed; the metrics line can be drawn.
    Measured,
    Finished(Finish),
    Failed(WireError),
}

/// The only thing that knows llama-server's stream shape.
pub struct TurnStream {
    decoder: SseDecoder,
    state: TurnState,
    clock: Box<dyn Clock>,
    started: Duration,
    /// When the first reasoning token arrived, so the thinking span can be
    /// closed by the first answer token or by the end of the turn.
    thinking_started: Option<Duration>,
    settled: bool,
}

impl TurnStream {
    pub fn new() -> Self {
        Self::with_clock(Box::new(SystemClock::default()))
    }

    pub fn with_clock(clock: Box<dyn Clock>) -> Self {
        let started = clock.now();
        Self {
            decoder: SseDecoder::new(),
            state: TurnState::default(),
            clock,
            started,
            thinking_started: None,
            settled: false,
        }
    }

    pub fn state(&self) -> &TurnState {
        &self.state
    }

    /// Feed whatever came off the socket.
    pub fn push(&mut self, text: &str) -> Vec<Event> {
        let mut events = Vec::new();
        for sse in self.decoder.push(text) {
            self.absorb(sse, &mut events);
        }
        events
    }

    /// The connection closed. Flushes a final unterminated line and settles the
    /// turn whether or not `[DONE]` ever arrived — a stream that dies mid-answer
    /// still leaves a turn worth keeping.
    pub fn end(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        for sse in self.decoder.finish() {
            self.absorb(sse, &mut events);
        }
        if !self.settled {
            self.settle(None, &mut events);
        }
        events
    }

    /// Escape, or the stop button.
    pub fn cancel(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        if !self.settled {
            self.settle(Some(Finish::Cancelled), &mut events);
        }
        events
    }

    /// The settled turn. Tool noise is stripped here rather than per token,
    /// because a half-written tag is indistinguishable from prose until it
    /// closes.
    pub fn finish(mut self) -> TurnState {
        if !self.settled {
            self.settle(None, &mut Vec::new());
        }
        let said = std::mem::take(&mut self.state.answer);
        self.state.answer = strip_tool_noise(&said);

        // A turn that produced nothing at all is the one failure a user cannot
        // read anything into, so before giving up, look for a call the server
        // did not parse. See `recover_tool_calls` for why one is there.
        //
        // Both channels, because the leak lands in both. The thinking is the
        // documented shape; the answer is what happens when a call is long
        // enough that the model gives up on emitting it as JSON and writes it
        // out as text instead — and stripping without recovering turns that
        // into a turn that says nothing and did nothing. The guard is what
        // makes reading the answer safe: there is nothing left to strip it
        // from, so the model was not talking *about* a call, it was making one.
        if self.state.tool_calls.is_empty() && self.state.answer.trim().is_empty() {
            let mut recovered = recover_tool_calls(&self.state.thinking);
            if recovered.is_empty() {
                recovered = recover_tool_calls(&said);
            }
            self.state.recovered_calls = recovered.len();
            self.state.tool_calls = recovered;
        }
        self.state
    }

    fn absorb(&mut self, sse: Sse, events: &mut Vec<Event>) {
        let payload = match sse {
            Sse::Done => {
                if !self.settled {
                    self.settle(None, events);
                }
                return;
            }
            Sse::Data(payload) => payload,
        };

        let chunk = match wire::parse_chunk(&payload) {
            Ok(chunk) => chunk,
            Err(error) => {
                events.push(Event::Failed(error));
                return;
            }
        };

        let mut measured = false;
        if let Some(usage) = chunk.usage {
            self.state.usage = Some(usage);
            measured = true;
        }
        if let Some(timings) = chunk.timings {
            self.state.timings = Some(timings);
            measured = true;
        }

        let mut finish = None;
        for choice in chunk.choices {
            if let Some(thinking) = choice.delta.reasoning_content.filter(|t| !t.is_empty()) {
                self.mark_first_token();
                if self.thinking_started.is_none() {
                    self.thinking_started = Some(self.clock.now());
                }
                self.state.thinking.push_str(&thinking);
                events.push(Event::Thinking(thinking));
            }
            if let Some(answer) = choice.delta.content.filter(|a| !a.is_empty()) {
                self.mark_first_token();
                // The first answer token closes the thinking span. A model that
                // goes back to thinking afterwards keeps the first span, which
                // is the one the disclosure is about.
                self.close_thinking();
                self.state.answer.push_str(&answer);
                events.push(Event::Answer(answer));
            }
            for delta in choice.delta.tool_calls {
                self.mark_first_token();
                let index = self.absorb_tool_call(delta);
                events.push(Event::ToolCall(index));
            }
            if let Some(reason) = choice.finish_reason {
                finish = Some(Finish::from_reason(&reason));
            }
        }

        if measured {
            events.push(Event::Measured);
        }
        if let Some(finish) = finish {
            self.settle(Some(finish), events);
        }
    }

    fn absorb_tool_call(&mut self, delta: wire::ToolCallDelta) -> usize {
        let index = delta.index;
        if self.state.tool_calls.len() <= index {
            self.state
                .tool_calls
                .resize_with(index + 1, ToolCall::default);
        }
        let call = &mut self.state.tool_calls[index];
        if let Some(id) = delta.id {
            call.id = id;
        }
        if let Some(function) = delta.function {
            if let Some(name) = function.name {
                call.name.push_str(&name);
            }
            if let Some(arguments) = function.arguments {
                call.arguments.push_str(&arguments);
            }
        }
        index
    }

    /// End the thinking span, if one is open.
    fn close_thinking(&mut self) {
        if self.state.thinking_elapsed.is_some() {
            return;
        }
        if let Some(started) = self.thinking_started {
            self.state.thinking_elapsed = Some(self.clock.now() - started);
        }
    }

    fn mark_first_token(&mut self) {
        if self.state.time_to_first_token.is_none() {
            self.state.time_to_first_token = Some(self.clock.now() - self.started);
        }
    }

    fn settle(&mut self, finish: Option<Finish>, events: &mut Vec<Event>) {
        self.settled = true;
        // A turn that thought and then stopped without answering still spent
        // that time thinking.
        self.close_thinking();
        self.state.elapsed = self.clock.now() - self.started;
        for call in &mut self.state.tool_calls {
            call.complete = true;
        }
        let finish =
            finish.or_else(|| (!self.state.tool_calls.is_empty()).then_some(Finish::ToolCalls));
        if let Some(finish) = finish {
            self.state.finish = Some(finish.clone());
            events.push(Event::Finished(finish));
        }
    }
}

impl Default for TurnStream {
    fn default() -> Self {
        Self::new()
    }
}

/// A small local model sometimes writes a tool call into its prose instead of
/// calling one. Nothing executed, so it is neither shown nor persisted.
///
/// Only a tag that is *followed by a call* is stripped. An answer explaining
/// what a leak looks like will contain the tag as prose, and eating the rest of
/// that answer would be a worse bug than the one being fixed.
pub fn strip_tool_noise(answer: &str) -> String {
    const OPEN: &str = "<tool_call>";
    const CLOSE: &str = "</tool_call>";

    let mut out = String::with_capacity(answer.len());
    let mut rest = answer;
    while let Some(open) = rest.find(OPEN) {
        let after = &rest[open + OPEN.len()..];
        if !opens_a_call(after) {
            out.push_str(&rest[..open + OPEN.len()]);
            rest = after;
            continue;
        }
        out.push_str(&rest[..open]);
        match after.find(CLOSE) {
            Some(close) => rest = &after[close + CLOSE.len()..],
            // Unterminated: the model was still writing it when it stopped.
            None => return out.trim_end().to_string(),
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

fn opens_a_call(after: &str) -> bool {
    let after = after.trim_start();
    after.starts_with("<function") || after.starts_with('{')
}

/// Tool calls the server wrote into the *thinking* instead of into
/// `tool_calls`, recovered so the turn can carry on.
///
/// This is not defensive programming, it is a specific upstream bug with a
/// specific shape. Qwen 3.5 and 3.6 under llama.cpp regularly emit a complete,
/// well-formed `<tool_call>` block into `reasoning_content`, leave `tool_calls`
/// absent and `content` empty, and report `finish_reason: stop` —
/// [ggml-org/llama.cpp#22684]. Nothing about the response says anything went
/// wrong. The model did the right thing and the parse dropped it, and what the
/// user sees is a turn that answered with silence.
///
/// Measured against this server it was **five times out of five** on one
/// scenario, so leaving it unhandled is not a rare-path decision.
///
/// Two shapes, both of which occur:
///
/// ```text
/// <tool_call><function=planner><parameter=args>["list"]</parameter></function></tool_call>
/// <tool_call>{"name": "planner", "arguments": {"args": ["list"]}}</tool_call>
/// ```
///
/// Anything that does not parse cleanly into a name and a JSON object is
/// ignored rather than guessed at — a malformed block also occurs, and running
/// half a call the model did not finish writing would be worse than running
/// none.
pub fn recover_tool_calls(thinking: &str) -> Vec<ToolCall> {
    const OPEN: &str = "<tool_call>";
    const CLOSE: &str = "</tool_call>";

    let mut bodies = Vec::new();
    let mut rest = thinking;
    while let Some(open) = rest.find(OPEN) {
        let after = &rest[open + OPEN.len()..];
        let Some(close) = after.find(CLOSE) else {
            // Unterminated: the model stopped mid-write, so there is no call
            // here to run.
            break;
        };
        bodies.push(&after[..close]);
        rest = &after[close + CLOSE.len()..];
    }

    // **The opening tag is often missing**, and that is the common case rather
    // than a corruption. llama.cpp consumes `<tool_call>` as a parse marker,
    // then fails on what follows and emits only the remainder — so the thinking
    // ends with a bare `<function=…>…</function></tool_call>` and no opener at
    // all. Measured on this server it was four silent rounds in five.
    if bodies.is_empty() {
        let mut rest = thinking;
        while let Some(at) = rest.find("<function=") {
            let after = &rest[at..];
            let Some(end) = after.find("</function>") else {
                break;
            };
            bodies.push(&after[..end + "</function>".len()]);
            rest = &after[end + "</function>".len()..];
        }
    }

    let mut recovered = Vec::new();
    for body in bodies {
        if let Some((name, arguments)) = parse_call_body(body) {
            recovered.push(ToolCall {
                // Distinct per call, and marked, so a transcript shows plainly
                // that this one was rescued rather than parsed.
                id: format!("recovered_{}", recovered.len()),
                name,
                arguments,
                complete: true,
                outcome: None,
            });
        }
    }
    recovered
}

/// A `<parameter>` value as JSON, tolerating the escaping the leak adds.
///
/// A value that arrives as `["search", "report"]` is a JSON array. The same
/// value sometimes arrives as `[\"search\", \"report\"]` — the server escaped it
/// on the way into a JSON string field and the leak preserved the escapes.
/// Parsing that as JSON fails, and treating it as a plain string would hand the
/// tool one long word where it expected a list.
fn parameter_value(raw: &str) -> serde_json::Value {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) {
        return value;
    }
    let unescaped = raw.replace("\\\"", "\"").replace("\\\\", "\\");
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&unescaped) {
        return value;
    }
    serde_json::Value::String(raw.to_string())
}

/// One `<tool_call>` body as a name and a JSON argument object.
fn parse_call_body(body: &str) -> Option<(String, String)> {
    let body = body.trim();

    // The JSON form: {"name": …, "arguments": {…}}.
    if body.starts_with('{') {
        let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
        let name = parsed.get("name")?.as_str()?.trim().to_string();
        if name.is_empty() {
            return None;
        }
        let arguments = match parsed.get("arguments") {
            Some(serde_json::Value::String(text)) => text.clone(),
            Some(value) => value.to_string(),
            None => "{}".to_string(),
        };
        return Some((name, arguments));
    }

    // The XML form the froggeric template teaches.
    let name = between(body, "<function=", ">")?.trim().to_string();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let mut arguments = serde_json::Map::new();
    let mut rest = body;
    while let Some(at) = rest.find("<parameter=") {
        let after = &rest[at + "<parameter=".len()..];
        let Some(gt) = after.find('>') else { break };
        let key = after[..gt].trim().to_string();
        let value_and_rest = &after[gt + 1..];
        let Some(end) = value_and_rest.find("</parameter>") else {
            break;
        };
        let raw = value_and_rest[..end].trim();
        rest = &value_and_rest[end + "</parameter>".len()..];
        if key.is_empty() {
            continue;
        }
        // A parameter's text is JSON when the model wrote a list or a number,
        // and a plain string when it wrote a word. Both are common.
        arguments.insert(key, parameter_value(raw));
    }
    if arguments.is_empty() {
        return None;
    }
    Some((name, serde_json::Value::Object(arguments).to_string()))
}

/// The text between two markers, if both are there in order.
fn between<'a>(haystack: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = haystack.find(open)? + open.len();
    let end = haystack[start..].find(close)? + start;
    Some(&haystack[start..end])
}

/// The numbers under a finished turn. This is the shape that reaches the disk,
/// so the durations are plain milliseconds rather than a `Duration`'s
/// `{secs, nanos}` — a thread file is meant to be readable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnMetrics {
    #[serde(default)]
    pub prompt_tokens: u32,
    /// How much of the prompt the server already had. Worth showing: it is the
    /// difference between a turn that cost a full prefill and one that did not.
    #[serde(default)]
    pub cached_tokens: u32,
    #[serde(default)]
    pub generated_tokens: u32,
    #[serde(default)]
    pub prompt_per_second: Option<f64>,
    #[serde(default)]
    pub generation_per_second: Option<f64>,
    #[serde(default)]
    pub draft_acceptance: Option<f64>,
    #[serde(default)]
    pub time_to_first_token_ms: Option<u64>,
    /// How long it thought before answering. Persisted, so a reopened thread
    /// still says "Thought for 4s".
    #[serde(default)]
    pub thinking_ms: Option<u64>,
    #[serde(default)]
    pub elapsed_ms: u64,
}

impl TurnMetrics {
    /// llama.cpp's timings are preferred over the wall clock wherever it has
    /// them: the wall clock measures the network too.
    ///
    /// The one exception is the prompt size. `timings.prompt_n` counts only the
    /// tokens actually *prefilled*, so a turn that hit the prompt cache reports
    /// 23 for a 5,000-token conversation — a true statement about the prefill
    /// and a wrong one about how full the context is. The whole prompt is
    /// `cache_n + prompt_n`, which llama.cpp reports and which agrees with
    /// `usage.prompt_tokens`.
    pub fn of(state: &TurnState) -> Self {
        let timings = state.timings;
        let usage = state.usage;
        Self {
            prompt_tokens: timings
                .map(|t| t.prompt_total())
                .filter(|n| *n > 0)
                .or_else(|| usage.map(|u| u.prompt_tokens))
                .unwrap_or(0),
            cached_tokens: timings.map(|t| t.cache_n).unwrap_or(0),
            generated_tokens: timings
                .map(|t| t.predicted_n)
                .filter(|n| *n > 0)
                .or_else(|| usage.map(|u| u.completion_tokens))
                .unwrap_or(0),
            prompt_per_second: timings.and_then(|t| t.prompt_per_second()),
            generation_per_second: timings
                .and_then(|t| t.generation_per_second())
                .or_else(|| estimate(state)),
            draft_acceptance: timings.and_then(|t| t.draft_acceptance()),
            time_to_first_token_ms: state
                .time_to_first_token
                .map(|ttft| ttft.as_millis() as u64),
            thinking_ms: state
                .thinking_elapsed
                .map(|elapsed| elapsed.as_millis() as u64),
            elapsed_ms: state.elapsed.as_millis() as u64,
        }
    }

    /// The caption under a turn. Empty when the server reported nothing, so the
    /// UI draws no line rather than a line of zeroes.
    pub fn one_line(&self) -> String {
        let mut parts = Vec::new();
        if self.prompt_tokens > 0 {
            let mut counted = format!("{} in", thousands(self.prompt_tokens));
            if self.cached_tokens > 0 {
                counted.push_str(&format!(" ({} cached)", thousands(self.cached_tokens)));
            }
            parts.push(counted);
        }
        if self.generated_tokens > 0 {
            parts.push(format!("{} out", thousands(self.generated_tokens)));
        }
        if let Some(rate) = self.generation_per_second {
            parts.push(format!("{rate:.0} tok/s"));
        }
        if let Some(ttft) = self.time_to_first_token_ms {
            parts.push(format!("{:.1} s to first", ttft as f64 / 1000.0));
        }
        if let Some(acceptance) = self.draft_acceptance {
            parts.push(format!("{:.0}% draft", acceptance * 100.0));
        }
        parts.join(" · ")
    }
}

/// Generation rate from the wall clock, for a server that reports no timings.
/// Deliberately measured from the first token, not from the request, so the
/// prefill is not counted against the generation rate.
fn estimate(state: &TurnState) -> Option<f64> {
    let generated = state.usage?.completion_tokens;
    let first = state.time_to_first_token?;
    let generating = state.elapsed.checked_sub(first)?;
    (generated > 0 && !generating.is_zero())
        .then(|| f64::from(generated) / generating.as_secs_f64())
}

fn thousands(value: u32) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manual() -> (TurnStream, std::rc::Rc<ManualClock>) {
        struct Shared(std::rc::Rc<ManualClock>);
        impl Clock for Shared {
            fn now(&self) -> Duration {
                self.0.now()
            }
        }
        let clock = std::rc::Rc::new(ManualClock::default());
        let stream = TurnStream::with_clock(Box::new(Shared(clock.clone())));
        (stream, clock)
    }

    fn frame(json: &str) -> String {
        format!("data: {json}\n\n")
    }

    fn called_with(arguments: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: "x".into(),
            arguments: arguments.into(),
            complete: true,
            outcome: None,
        }
    }

    #[test]
    fn a_calls_arguments_read_as_labelled_fields() {
        let fields = called_with(r#"{"subject":"Roof","related_to":"Contractors"}"#).fields();
        assert_eq!(
            fields,
            [
                Field::new("Related to", "Contractors"),
                Field::new("Subject", "Roof"),
            ]
        );
    }

    #[test]
    fn an_argv_reads_as_the_command_line_it_becomes() {
        // `gh` and the sibling CLIs all take one, and a JSON array of strings
        // is the least readable way to show somebody a command.
        let fields = called_with(r#"{"args":["pr","list","--json","number,title"]}"#).fields();
        assert_eq!(fields, [Field::new("Args", "pr list --json number,title")]);
    }

    #[test]
    fn something_genuinely_nested_stays_json_rather_than_being_flattened() {
        // A spreadsheet's `sheets` is a list of objects. Joining it into a
        // sentence would be worse than showing what it is.
        let fields = called_with(r#"{"sheets":[{"name":"Q1","rows":[["a","b"]]}]}"#).fields();
        assert_eq!(fields.len(), 1);
        assert!(fields[0].value.contains("\"name\": \"Q1\""), "{fields:?}");
        assert!(fields[0].is_block());
    }

    #[test]
    fn a_script_wants_a_block_and_a_short_path_does_not() {
        let script = Field::new("Code", "import math\nprint(math.pi)");
        assert!(script.is_block());
        assert!(!Field::new("Path", "notes/roof.md").is_block());
        assert!(Field::new("Query", "x".repeat(200)).is_block());
    }

    #[test]
    fn arguments_that_never_finished_arriving_are_shown_as_they_are() {
        // Mid-stream a call holds half an object. Reporting "no arguments"
        // would be a lie, and the raw text is the only true thing available.
        let fields = called_with(r#"{"query":"the roof"#).fields();
        assert_eq!(fields.len(), 1);
        assert!(fields[0].value.starts_with('{'), "{fields:?}");

        assert!(called_with("").fields().is_empty());
        assert!(called_with("{}").fields().is_empty());
    }

    #[test]
    fn thinking_and_answer_are_folded_into_separate_streams() {
        let (mut stream, _clock) = manual();
        stream.push(&frame(
            r#"{"choices":[{"delta":{"reasoning_content":"the user "}}]}"#,
        ));
        stream.push(&frame(
            r#"{"choices":[{"delta":{"reasoning_content":"wants X"}}]}"#,
        ));
        stream.push(&frame(r#"{"choices":[{"delta":{"content":"Here "}}]}"#));
        stream.push(&frame(
            r#"{"choices":[{"delta":{"content":"you go."},"finish_reason":"stop"}]}"#,
        ));

        let state = stream.finish();
        assert_eq!(state.thinking, "the user wants X");
        assert_eq!(state.answer, "Here you go.");
        assert_eq!(state.finish, Some(Finish::Stop));
    }

    #[test]
    fn events_carry_the_fragment_not_the_accumulation() {
        let (mut stream, _clock) = manual();
        let first = stream.push(&frame(r#"{"choices":[{"delta":{"content":"Here "}}]}"#));
        let second = stream.push(&frame(r#"{"choices":[{"delta":{"content":"you go."}}]}"#));
        assert_eq!(first, vec![Event::Answer("Here ".into())]);
        assert_eq!(second, vec![Event::Answer("you go.".into())]);
    }

    #[test]
    fn time_to_first_token_is_measured_from_the_first_thing_that_arrived() {
        let (mut stream, clock) = manual();
        clock.advance(Duration::from_millis(300));
        stream.push(&frame(
            r#"{"choices":[{"delta":{"reasoning_content":"hmm"}}]}"#,
        ));
        clock.advance(Duration::from_millis(700));
        stream.push(&frame(
            r#"{"choices":[{"delta":{"content":"yes"},"finish_reason":"stop"}]}"#,
        ));

        let state = stream.finish();
        assert_eq!(state.time_to_first_token, Some(Duration::from_millis(300)));
        assert_eq!(state.elapsed, Duration::from_millis(1000));
    }

    #[test]
    fn thinking_is_timed_from_its_first_token_to_the_first_answer_token() {
        let (mut stream, clock) = manual();
        clock.advance(Duration::from_millis(200));
        stream.push(&frame(
            r#"{"choices":[{"delta":{"reasoning_content":"hmm"}}]}"#,
        ));
        clock.advance(Duration::from_millis(4000));
        stream.push(&frame(
            r#"{"choices":[{"delta":{"reasoning_content":" still"}}]}"#,
        ));
        clock.advance(Duration::from_millis(300));
        stream.push(&frame(
            r#"{"choices":[{"delta":{"content":"yes"},"finish_reason":"stop"}]}"#,
        ));

        let state = stream.finish();
        assert_eq!(state.thinking_elapsed, Some(Duration::from_millis(4300)));
        assert_eq!(state.metrics().thinking_ms, Some(4300));
    }

    #[test]
    fn a_turn_that_never_thinks_reports_no_thinking_time() {
        let (mut stream, _clock) = manual();
        stream.push(&frame(
            r#"{"choices":[{"delta":{"content":"yes"},"finish_reason":"stop"}]}"#,
        ));
        let state = stream.finish();
        assert_eq!(state.thinking_elapsed, None);
        assert_eq!(state.metrics().thinking_ms, None);
    }

    #[test]
    fn thinking_that_never_reaches_an_answer_is_still_timed() {
        // Cut off, or a model that spends the whole budget deliberating.
        let (mut stream, clock) = manual();
        stream.push(&frame(
            r#"{"choices":[{"delta":{"reasoning_content":"hmm"}}]}"#,
        ));
        clock.advance(Duration::from_millis(900));
        stream.cancel();
        assert_eq!(
            stream.finish().thinking_elapsed,
            Some(Duration::from_millis(900))
        );
    }

    #[test]
    fn thinking_after_the_answer_starts_does_not_extend_the_span() {
        let (mut stream, clock) = manual();
        stream.push(&frame(
            r#"{"choices":[{"delta":{"reasoning_content":"hmm"}}]}"#,
        ));
        clock.advance(Duration::from_millis(500));
        stream.push(&frame(r#"{"choices":[{"delta":{"content":"yes"}}]}"#));
        clock.advance(Duration::from_millis(5000));
        stream.push(&frame(
            r#"{"choices":[{"delta":{"reasoning_content":"actually…"}}]}"#,
        ));

        assert_eq!(
            stream.finish().thinking_elapsed,
            Some(Duration::from_millis(500))
        );
    }

    #[test]
    fn a_tool_call_arriving_in_fragments_is_one_call() {
        let (mut stream, _clock) = manual();
        stream.push(&frame(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"recall","arguments":"{\"que"}}]}}]}"#,
        ));
        stream.push(&frame(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ry\":\"scanner\"}"}}]}}]}"#,
        ));
        stream.push(&frame(
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ));

        let state = stream.finish();
        assert_eq!(state.tool_calls.len(), 1);
        let call = &state.tool_calls[0];
        assert_eq!(call.id, "call_1");
        assert_eq!(call.name, "recall");
        assert_eq!(call.arguments, r#"{"query":"scanner"}"#);
        assert!(call.complete);
        assert_eq!(call.primary_argument().as_deref(), Some("scanner"));
        assert_eq!(state.finish, Some(Finish::ToolCalls));
    }

    #[test]
    fn a_chip_for_a_list_argument_names_what_is_in_it() {
        // `use_tools` takes a list, and a chip reading only "use_tools" hides
        // the one thing anybody looking at it wants to know.
        let call = ToolCall {
            name: "use_tools".into(),
            arguments: r#"{"names":["documents","python"]}"#.into(),
            ..ToolCall::default()
        };
        assert_eq!(
            call.primary_argument().as_deref(),
            Some("documents, python")
        );

        // A list of anything else is still not a chip's worth of text.
        let odd = ToolCall {
            arguments: r#"{"names":[{"a":1}]}"#.into(),
            ..ToolCall::default()
        };
        assert_eq!(odd.primary_argument(), None);
    }

    #[test]
    fn two_parallel_tool_calls_stay_apart() {
        let (mut stream, _clock) = manual();
        stream.push(&frame(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"recall","arguments":"{}"}},{"index":1,"id":"b","function":{"name":"fetch_url","arguments":"{}"}}]}}]}"#,
        ));
        let state = stream.finish();
        assert_eq!(state.tool_calls.len(), 2);
        assert_eq!(state.tool_calls[0].name, "recall");
        assert_eq!(state.tool_calls[1].name, "fetch_url");
    }

    #[test]
    fn a_bad_frame_is_an_event_and_the_stream_carries_on() {
        let (mut stream, _clock) = manual();
        stream.push(&frame(r#"{"choices":[{"delta":{"content":"one"}}]}"#));
        let events = stream.push("data: not json at all\n\n");
        stream.push(&frame(r#"{"choices":[{"delta":{"content":" two"}}]}"#));

        assert!(matches!(events.as_slice(), [Event::Failed(_)]));
        assert_eq!(stream.state().answer, "one two");
    }

    #[test]
    fn the_done_sentinel_settles_a_turn_that_reported_no_reason() {
        let (mut stream, _clock) = manual();
        stream.push(&frame(r#"{"choices":[{"delta":{"content":"hi"}}]}"#));
        let events = stream.push("data: [DONE]\n\n");
        // Nothing to report: no finish reason arrived and no tool call is
        // pending, so there is no Finished event to invent.
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(stream.state().answer, "hi");
    }

    #[test]
    fn a_stream_cut_off_mid_answer_still_yields_the_turn() {
        let (mut stream, clock) = manual();
        stream.push(&frame(
            r#"{"choices":[{"delta":{"content":"half an ans"}}]}"#,
        ));
        clock.advance(Duration::from_millis(500));
        stream.end();

        let state = stream.finish();
        assert_eq!(state.answer, "half an ans");
        assert_eq!(state.finish, None);
        assert_eq!(state.elapsed, Duration::from_millis(500));
    }

    #[test]
    fn cancelling_settles_the_turn_as_cancelled() {
        let (mut stream, _clock) = manual();
        stream.push(&frame(r#"{"choices":[{"delta":{"content":"partial"}}]}"#));
        let events = stream.cancel();
        assert_eq!(events, vec![Event::Finished(Finish::Cancelled)]);
        assert_eq!(stream.finish().finish, Some(Finish::Cancelled));
    }

    #[test]
    fn the_prompt_size_is_the_whole_prompt_not_just_what_was_prefilled() {
        // A cached turn prefills almost nothing. Reporting that as the prompt
        // size makes the context-usage bar read near zero on a long thread.
        let state = TurnState {
            usage: Some(Usage {
                prompt_tokens: 5_000,
                completion_tokens: 40,
                total_tokens: 5_040,
            }),
            timings: Some(Timings {
                cache_n: 4_977,
                prompt_n: 23,
                prompt_ms: 10.0,
                predicted_n: 40,
                predicted_ms: 400.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let metrics = state.metrics();
        assert_eq!(metrics.prompt_tokens, 5_000);
        assert_eq!(metrics.cached_tokens, 4_977);
        // The prefill *rate* still comes from what was actually processed.
        assert_eq!(metrics.prompt_per_second, Some(2300.0));
        // And the caption says where the saving came from.
        assert!(
            metrics.one_line().contains("5,000 in (4,977 cached)"),
            "{}",
            metrics.one_line()
        );
    }

    #[test]
    fn usage_and_timings_land_as_one_measured_event() {
        let (mut stream, _clock) = manual();
        let events = stream.push(&frame(
            r#"{"choices":[],"usage":{"prompt_tokens":812,"completion_tokens":140,"total_tokens":952},"timings":{"prompt_n":812,"prompt_ms":406.0,"predicted_n":140,"predicted_ms":1750.0}}"#,
        ));
        assert_eq!(events, vec![Event::Measured]);

        let metrics = stream.finish().metrics();
        assert_eq!(metrics.prompt_tokens, 812);
        assert_eq!(metrics.generated_tokens, 140);
        assert_eq!(metrics.generation_per_second, Some(80.0));
    }

    #[test]
    fn the_metrics_line_reads_as_a_caption() {
        let metrics = TurnMetrics {
            prompt_tokens: 1412,
            generated_tokens: 140,
            generation_per_second: Some(84.0),
            time_to_first_token_ms: Some(320),
            draft_acceptance: Some(0.75),
            ..Default::default()
        };
        assert_eq!(
            metrics.one_line(),
            "1,412 in · 140 out · 84 tok/s · 0.3 s to first · 75% draft"
        );
    }

    #[test]
    fn a_turn_the_server_measured_nothing_for_draws_no_line() {
        assert_eq!(TurnMetrics::default().one_line(), "");
    }

    #[test]
    fn a_server_without_timings_falls_back_to_the_wall_clock() {
        let (mut stream, clock) = manual();
        clock.advance(Duration::from_millis(500));
        stream.push(&frame(r#"{"choices":[{"delta":{"content":"x"}}]}"#));
        clock.advance(Duration::from_millis(1000));
        stream.push(&frame(
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":50,"total_tokens":60}}"#,
        ));
        stream.push(&frame(
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ));

        // 50 tokens over the second that followed the first one — the half
        // second of prefill is not counted against the generation rate.
        let metrics = stream.finish().metrics();
        assert_eq!(metrics.generation_per_second, Some(50.0));
    }

    /// The exact `reasoning_content` this server produced for
    /// `planner/ambiguous-is-a-question`, five times out of five.
    const LEAKED_XML: &str = "I should search for the task first.\n<tool_call>\n\
         <function=planner>\n<parameter=args>\n[\"search\", \"quarterly report\"]\n\
         </parameter>\n</function>\n</tool_call>";

    #[test]
    fn a_call_the_server_left_in_the_thinking_is_recovered() {
        // llama.cpp#22684: Qwen 3.5/3.6 write a complete `<tool_call>` into
        // `reasoning_content`, leave `tool_calls` absent and `content` empty,
        // and report `finish_reason: stop`. Nothing says anything went wrong.
        let recovered = recover_tool_calls(LEAKED_XML);
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].name, "planner");
        assert!(recovered[0].complete);

        let arguments: serde_json::Value =
            serde_json::from_str(&recovered[0].arguments).expect("valid JSON");
        assert_eq!(
            arguments["args"],
            serde_json::json!(["search", "quarterly report"])
        );
    }

    #[test]
    fn the_json_form_of_a_leaked_call_is_recovered_too() {
        let recovered = recover_tool_calls(
            "<tool_call>{\"name\": \"planner\", \"arguments\": {\"args\": [\"overview\"]}}</tool_call>",
        );
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].name, "planner");
        let arguments: serde_json::Value =
            serde_json::from_str(&recovered[0].arguments).expect("valid JSON");
        assert_eq!(arguments["args"], serde_json::json!(["overview"]));
    }

    #[test]
    fn several_leaked_calls_are_all_recovered_and_kept_distinct() {
        let mut both = LEAKED_XML.to_string();
        both.push_str(
            "\n<tool_call>\n<function=planner>\n<parameter=args>\n[\"overview\"]\n\
             </parameter>\n</function>\n</tool_call>",
        );
        let recovered = recover_tool_calls(&both);
        assert_eq!(recovered.len(), 2);
        // Distinct ids, or the results would collide when they are paired back
        // up with their calls.
        assert_ne!(recovered[0].id, recovered[1].id);
    }

    #[test]
    fn a_call_with_its_opening_tag_eaten_is_still_recovered() {
        // The common shape, not a corner: llama.cpp consumes `<tool_call>` as a
        // parse marker, fails on what follows, and leaks only the remainder.
        // Four silent rounds in five looked exactly like this.
        let headless = "<function=planner>\n<parameter=args>\n[\"overview\"]\n\
                        </parameter>\n</function>\n</tool_call>";
        let recovered = recover_tool_calls(headless);
        assert_eq!(recovered.len(), 1, "{recovered:?}");
        assert_eq!(recovered[0].name, "planner");
        let arguments: serde_json::Value =
            serde_json::from_str(&recovered[0].arguments).expect("valid JSON");
        assert_eq!(arguments["args"], serde_json::json!(["overview"]));
    }

    #[test]
    fn an_escaped_argument_is_unescaped_rather_than_taken_as_one_long_word() {
        // Also seen on the wire. Treating this as a plain string would hand
        // `planner` one word where it expects a list, and the call would run
        // and fail rather than not run at all — which is worse.
        let escaped = "<function=planner>\n<parameter=args>\n                       [\\\"search\\\", \\\"quarterly report\\\"]\n\
                       </parameter>\n</function>\n</tool_call>";
        let recovered = recover_tool_calls(escaped);
        assert_eq!(recovered.len(), 1, "{recovered:?}");
        let arguments: serde_json::Value =
            serde_json::from_str(&recovered[0].arguments).expect("valid JSON");
        assert_eq!(
            arguments["args"],
            serde_json::json!(["search", "quarterly report"]),
            "got {}",
            recovered[0].arguments
        );
    }

    #[test]
    fn a_malformed_leak_is_left_alone_rather_than_guessed_at() {
        // Also produced by this server, once in five: the tags are garbled and
        // there is no honest call in it. Running half of one the model never
        // finished writing would be worse than running none.
        for garbled in [
            "<tool_call>\n<ext>\n<function_name>\nplanner\n</function>\n</tool_call>",
            // Unterminated: it stopped mid-write.
            "<tool_call>\n<function=planner>\n<parameter=args>\n[\"sea",
            // A function with no arguments at all.
            "<tool_call>\n<function=planner>\n</function>\n</tool_call>",
            "no call here at all",
            "",
        ] {
            assert!(
                recover_tool_calls(garbled).is_empty(),
                "recovered something from {garbled:?}"
            );
        }
    }

    #[test]
    fn recovery_only_happens_when_the_turn_would_otherwise_be_silent() {
        // A turn that already has an answer must not gain a call it did not
        // make: the model writes `<tool_call>` in prose while explaining
        // itself, and `strip_tool_noise` exists for exactly that.
        let (mut stream, _clock) = manual();
        stream.push(&frame(&format!(
            r#"{{"choices":[{{"delta":{{"reasoning_content":{},"content":"Here you go."}},"finish_reason":"stop"}}]}}"#,
            serde_json::to_string(LEAKED_XML).expect("json")
        )));
        let state = stream.finish();
        assert_eq!(state.answer, "Here you go.");
        assert!(state.tool_calls.is_empty(), "{:?}", state.tool_calls);
        assert_eq!(state.recovered_calls, 0);
    }

    #[test]
    fn a_silent_turn_is_rescued_end_to_end() {
        // The whole point: this exact stream used to settle with no answer and
        // no calls, and the user saw an empty reply.
        let (mut stream, _clock) = manual();
        stream.push(&frame(&format!(
            r#"{{"choices":[{{"delta":{{"reasoning_content":{}}},"finish_reason":"stop"}}]}}"#,
            serde_json::to_string(LEAKED_XML).expect("json")
        )));
        let state = stream.finish();
        assert_eq!(state.tool_calls.len(), 1);
        assert_eq!(state.tool_calls[0].name, "planner");
        assert_eq!(state.recovered_calls, 1);
        assert!(!state.is_empty());
    }

    #[test]
    fn a_call_written_into_the_answer_is_rescued_rather_than_deleted() {
        // The other half of the same fault. `strip_tool_noise` removed it and
        // nothing put it back, so a call the model did make became a turn that
        // answered with nothing — indistinguishable, from the outside, from a
        // model that simply said nothing.
        let (mut stream, _clock) = manual();
        stream.push(&frame(&format!(
            r#"{{"choices":[{{"delta":{{"content":{}}},"finish_reason":"stop"}}]}}"#,
            serde_json::to_string(
                LEAKED_XML.trim_start_matches("I should search for the task first.\n")
            )
            .expect("json")
        )));
        let state = stream.finish();
        assert_eq!(state.tool_calls.len(), 1, "{:?}", state.answer);
        assert_eq!(state.tool_calls[0].name, "planner");
        assert_eq!(state.recovered_calls, 1);
    }

    #[test]
    fn a_call_in_the_thinking_wins_over_one_in_the_answer() {
        // Both channels can carry the same leak, and running it twice would
        // repeat whatever it does. The thinking is the documented shape, so it
        // is the one that is read.
        let bare = LEAKED_XML.trim_start_matches("I should search for the task first.\n");
        let (mut stream, _clock) = manual();
        stream.push(&frame(&format!(
            r#"{{"choices":[{{"delta":{{"reasoning_content":{},"content":{}}},"finish_reason":"stop"}}]}}"#,
            serde_json::to_string(LEAKED_XML).expect("json"),
            serde_json::to_string(bare).expect("json")
        )));
        let state = stream.finish();
        assert_eq!(state.tool_calls.len(), 1, "{:?}", state.tool_calls);
        assert_eq!(state.recovered_calls, 1);
    }

    #[test]
    fn a_leaked_tool_call_is_stripped_from_the_answer() {
        let answer = "Let me look.\n<tool_call><function=recall>{\"query\":\"x\"}</function></tool_call>\nHere it is.";
        assert_eq!(strip_tool_noise(answer), "Let me look.\n\nHere it is.");
    }

    #[test]
    fn a_half_written_leak_takes_the_rest_of_the_answer_with_it() {
        let answer = "One moment.\n<tool_call><function=recall>{\"que";
        assert_eq!(strip_tool_noise(answer), "One moment.");
    }

    #[test]
    fn prose_that_merely_mentions_tools_is_left_alone() {
        let answer = "Use `recall` to search, and `<tool_call>` is what a leak looks like.";
        assert_eq!(strip_tool_noise(answer), answer);
    }

    #[test]
    fn an_empty_turn_is_recognisable_as_one() {
        assert!(TurnState::default().is_empty());
        let (mut stream, _clock) = manual();
        stream.push(&frame(r#"{"choices":[{"delta":{"content":"x"}}]}"#));
        assert!(!stream.finish().is_empty());
    }
}
