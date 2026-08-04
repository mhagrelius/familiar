//! The wire: what goes to `llama-server` and what comes back.
//!
//! This module and [`super::turn`] are the only two that know the shape of
//! llama.cpp's OpenAI-compatible endpoint. Everything above them sees Rust
//! types. Keeping that seam narrow is what makes the Cogsworth WebSocket
//! transport a sibling of this file later rather than a rewrite, and it is why
//! a llama.cpp change is diagnosed here rather than guessed at across the app.
//!
//! Two departures from OpenAI's schema are llama.cpp's own, and both are the
//! reason this file exists:
//!
//! * **`reasoning_content`** on the delta carries the model's thinking. Some
//!   builds spell it `reasoning`; both are accepted.
//! * **`timings`** rides at the top level of a chunk — not inside `usage` —
//!   and holds true prefill and generation throughput plus speculative-decode
//!   acceptance. It is the difference between honest numbers and wall-clock
//!   guesses.

use serde::{Deserialize, Serialize};

/// What Familiar sends. Fields the caller leaves `None` are omitted rather than
/// sent as null, because llama-server treats a present null as a value.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChatRequest {
    /// llama-server serves whatever model it was launched with and ignores
    /// this, but a gateway in front of it will not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub messages: Vec<Message>,
    pub stream: bool,
    /// Asks for the final usage chunk, which is where the token counts live
    /// when streaming.
    pub stream_options: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Honoured only when llama-server was started without
    /// `--reasoning-budget`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_budget: Option<i32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDeclaration>,
}

impl ChatRequest {
    /// A streaming request with no sampling overrides and no tools.
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            model: None,
            messages,
            stream: true,
            stream_options: StreamOptions::default(),
            temperature: None,
            top_p: None,
            max_tokens: None,
            reasoning_budget: None,
            tools: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            include_usage: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One message in the model's view of a conversation.
///
/// `reasoning_content` is here because llama-server accepts it and the
/// froggeric template re-emits it inside `<think>` tags when it was started
/// with `preserve_thinking`. What is *not* supported is a structured
/// `text_reasoning` content part, which is a different shape and is rejected —
/// that is the distinction the old "history never carries reasoning" rule was
/// really about. Whether to send it is `Thread::messages_for_model`'s call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    /// The model's own prior reasoning, sent back only when the template can
    /// use it. Omitted entirely otherwise, which is what every other server
    /// expects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolInvocation>,
    /// Set on a `tool` message, naming the call it answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::text(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::text(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text(Role::Assistant, content)
    }

    /// The result of a tool call, addressed to the call that asked for it.
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(Content::Text(content.into())),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }

    /// A question with images attached to it.
    ///
    /// The text goes first: a model reading parts in order should know what is
    /// being asked before it looks at the pictures.
    pub fn user_with_images(text: impl Into<String>, images: Vec<String>) -> Self {
        let mut parts = vec![Part::Text { text: text.into() }];
        for url in images {
            parts.push(Part::ImageUrl {
                image_url: ImageUrl { url },
            });
        }
        Self {
            role: Role::User,
            content: Some(Content::Parts(parts)),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// The words this message says, with any images left out.
    pub fn text_of(&self) -> &str {
        self.content.as_ref().map(Content::text).unwrap_or("")
    }

    fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(Content::Text(content.into())),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn with_tool_calls(mut self, calls: Vec<ToolInvocation>) -> Self {
        self.tool_calls = calls;
        self
    }

    /// Carry the model's prior reasoning back to it.
    pub fn with_reasoning(mut self, reasoning: impl Into<String>) -> Self {
        let reasoning = reasoning.into();
        self.reasoning_content = (!reasoning.is_empty()).then_some(reasoning);
        self
    }
}

/// What a message says.
///
/// Plain text for almost everything, and a list of parts when an image is
/// attached — which is the shape OpenAI defined and llama-server implements,
/// so an image is `image_url` carrying a `data:` URL rather than an upload.
/// Untagged, because the wire has no discriminator: it is a string or an array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<Part>),
}

impl Content {
    /// The words, with any images left out. Everything that reasons about a
    /// conversation — titles, summaries, compaction — wants this.
    pub fn text(&self) -> &str {
        match self {
            Self::Text(text) => text,
            Self::Parts(parts) => parts
                .iter()
                .find_map(|part| match part {
                    Part::Text { text } => Some(text.as_str()),
                    Part::ImageUrl { .. } => None,
                })
                .unwrap_or(""),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Text(text) => text.is_empty(),
            Self::Parts(parts) => parts.is_empty(),
        }
    }
}

impl From<String> for Content {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text {
        text: String,
    },
    ImageUrl {
        #[serde(rename = "image_url")]
        image_url: ImageUrl,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageUrl {
    /// A `data:image/png;base64,…` URL. llama-server decodes it and hands the
    /// pixels to the projector loaded with `--mmproj`.
    pub url: String,
}

/// A tool call as it appears in history: settled, with whole arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolInvocation {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionInvocation,
}

impl ToolInvocation {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: "function".into(),
            function: FunctionInvocation {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionInvocation {
    pub name: String,
    /// A JSON object, as a string. That is the protocol, not an oversight.
    pub arguments: String,
}

/// A tool offered to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDeclaration {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDeclaration,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionDeclaration {
    pub name: String,
    pub description: String,
    /// A JSON Schema object.
    pub parameters: serde_json::Value,
}

// -- What comes back ---------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Chunk {
    #[serde(default)]
    pub choices: Vec<Choice>,
    /// Present on the final chunk when `include_usage` was asked for.
    #[serde(default)]
    pub usage: Option<Usage>,
    /// llama.cpp's own, at the top level. Not in OpenAI's schema.
    #[serde(default)]
    pub timings: Option<Timings>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
    /// The model's thinking. `reasoning` is the older spelling.
    #[serde(default, alias = "reasoning")]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
}

/// A tool call arriving a fragment at a time. `index` is the identity: `id` and
/// `name` come once, `arguments` accumulate across many chunks.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ToolCallDelta {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
}

/// llama.cpp's timing block: the true numbers, in milliseconds and counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Timings {
    /// Tokens taken from the prompt cache rather than prefilled. The whole
    /// prompt is `cache_n + prompt_n`, which is why `prompt_n` alone reads as
    /// 23 tokens on a 5,000-token conversation that hit the cache.
    #[serde(default)]
    pub cache_n: u32,
    /// Tokens actually prefilled this turn.
    #[serde(default)]
    pub prompt_n: u32,
    #[serde(default)]
    pub prompt_ms: f64,
    #[serde(default)]
    pub predicted_n: u32,
    #[serde(default)]
    pub predicted_ms: f64,
    /// Speculative decoding: drafted tokens, and how many were accepted.
    #[serde(default)]
    pub draft_n: Option<u32>,
    #[serde(default)]
    pub draft_n_accepted: Option<u32>,
}

impl Timings {
    /// Every token of the prompt, cached or not.
    pub fn prompt_total(&self) -> u32 {
        self.cache_n + self.prompt_n
    }

    /// Prefill throughput, or `None` if nothing was prefilled.
    pub fn prompt_per_second(&self) -> Option<f64> {
        rate(self.prompt_n, self.prompt_ms)
    }

    /// Generation throughput, or `None` if nothing was generated.
    pub fn generation_per_second(&self) -> Option<f64> {
        rate(self.predicted_n, self.predicted_ms)
    }

    /// The fraction of drafted tokens the model kept, when speculative decoding
    /// is on.
    pub fn draft_acceptance(&self) -> Option<f64> {
        match (self.draft_n, self.draft_n_accepted) {
            (Some(drafted), Some(accepted)) if drafted > 0 => {
                Some(f64::from(accepted) / f64::from(drafted))
            }
            _ => None,
        }
    }
}

fn rate(tokens: u32, ms: f64) -> Option<f64> {
    (tokens > 0 && ms > 0.0).then(|| f64::from(tokens) * 1000.0 / ms)
}

/// The server declining, as a value. An overloaded or misconfigured server is
/// an expected outcome of asking it something, not an exception.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ServerError {
    #[serde(default)]
    pub message: String,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ErrorEnvelope {
    error: ServerError,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WireError {
    /// The server answered, and said no.
    Server(ServerError),
    /// A frame that is not a chunk and not an error. Carries the payload,
    /// because the next question is always "what did it actually send".
    Malformed { payload: String, detail: String },
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Server(error) => write!(f, "{}", error.message),
            Self::Malformed { detail, .. } => write!(f, "unreadable response: {detail}"),
        }
    }
}

impl std::error::Error for WireError {}

/// Parse one SSE payload into a chunk.
///
/// An error frame is a `Server` error rather than a parse failure: it is JSON
/// the server meant to send.
pub fn parse_chunk(payload: &str) -> Result<Chunk, WireError> {
    match serde_json::from_str::<Chunk>(payload) {
        // An error envelope also parses as an all-defaults chunk, so it is
        // checked before the empty case is accepted.
        Ok(chunk) if !is_empty(&chunk) => Ok(chunk),
        Ok(chunk) => match serde_json::from_str::<ErrorEnvelope>(payload) {
            Ok(envelope) => Err(WireError::Server(envelope.error)),
            Err(_) => Ok(chunk),
        },
        Err(source) => match serde_json::from_str::<ErrorEnvelope>(payload) {
            Ok(envelope) => Err(WireError::Server(envelope.error)),
            Err(_) => Err(WireError::Malformed {
                payload: payload.to_string(),
                detail: source.to_string(),
            }),
        },
    }
}

fn is_empty(chunk: &Chunk) -> bool {
    chunk.choices.is_empty() && chunk.usage.is_none() && chunk.timings.is_none()
}

// -- SSE framing -------------------------------------------------------------

/// One event off the wire, before anyone has looked at the JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sse {
    Data(String),
    /// The `[DONE]` sentinel that closes an OpenAI-shaped stream.
    Done,
}

/// Turns arbitrary reads into whole SSE events.
///
/// A read off a socket lands wherever it lands — mid-line, mid-token, mid-UTF-8
/// as far as the socket cares — so the decoder buffers the tail and emits only
/// what is complete. Comments (`: keep-alive`) and field lines other than
/// `data:` are dropped: llama-server sends them and nothing above wants them.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: String,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, text: &str) -> Vec<Sse> {
        self.buffer.push_str(text);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.find('\n') {
            let line: String = self.buffer.drain(..=end).collect();
            if let Some(event) = decode_line(line.trim_end_matches(['\n', '\r'])) {
                events.push(event);
            }
        }
        events
    }

    /// Flush whatever is left when the stream closes without a final newline.
    pub fn finish(&mut self) -> Vec<Sse> {
        let remainder = std::mem::take(&mut self.buffer);
        decode_line(remainder.trim_end_matches(['\n', '\r']))
            .into_iter()
            .collect()
    }
}

fn decode_line(line: &str) -> Option<Sse> {
    let payload = line.strip_prefix("data:")?.trim_start();
    if payload.is_empty() {
        return None;
    }
    if payload == "[DONE]" {
        return Some(Sse::Done);
    }
    Some(Sse::Data(payload.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_omits_the_sampling_it_was_not_given() {
        let request = ChatRequest::new(vec![Message::user("hello")]);
        let json = serde_json::to_string(&request).expect("serialize");
        assert!(!json.contains("temperature"), "{json}");
        assert!(!json.contains("tools"), "{json}");
        assert!(!json.contains("\"model\""), "{json}");
        assert!(json.contains(r#""include_usage":true"#), "{json}");
    }

    #[test]
    fn thinking_is_absent_unless_it_is_asked_for() {
        // The default is silence: a server or template that cannot use prior
        // reasoning must not be sent any.
        let json = serde_json::to_string(&Message::assistant("done")).expect("serialize");
        assert_eq!(json, r#"{"role":"assistant","content":"done"}"#);

        let carried = serde_json::to_string(&Message::assistant("done").with_reasoning("weighing"))
            .expect("serialize");
        assert!(
            carried.contains(r#""reasoning_content":"weighing""#),
            "{carried}"
        );
    }

    #[test]
    fn an_image_rides_as_a_content_part_beside_the_question() {
        let message =
            Message::user_with_images("what is this?", vec!["data:image/png;base64,AAAA".into()]);
        let json = serde_json::to_string(&message).expect("serialize");
        // The shape llama-server implements: a list of parts, text first.
        assert!(json.contains(r#""type":"text""#), "{json}");
        assert!(json.contains(r#""type":"image_url""#), "{json}");
        assert!(
            json.contains(r#""url":"data:image/png;base64,AAAA""#),
            "{json}"
        );
        assert!(
            json.find("text").unwrap() < json.find("image_url").unwrap(),
            "the question should come before the picture: {json}"
        );
        // And everything that reasons about the conversation still sees words.
        assert_eq!(message.text_of(), "what is this?");
    }

    #[test]
    fn a_message_without_images_stays_a_plain_string_on_the_wire() {
        // Sending `[{"type":"text",…}]` for every message would work and would
        // also change every cached prefix for no reason.
        let json = serde_json::to_string(&Message::user("hello")).expect("serialize");
        assert_eq!(json, r#"{"role":"user","content":"hello"}"#);
    }

    #[test]
    fn a_tool_result_names_the_call_it_answers() {
        let json =
            serde_json::to_string(&Message::tool_result("call_7", "3 notes")).expect("serialize");
        assert!(json.contains(r#""role":"tool""#), "{json}");
        assert!(json.contains(r#""tool_call_id":"call_7""#), "{json}");
    }

    #[test]
    fn a_content_chunk_parses() {
        let chunk = parse_chunk(
            r#"{"choices":[{"delta":{"content":"The scanner "},"finish_reason":null}]}"#,
        )
        .expect("chunk");
        assert_eq!(
            chunk.choices[0].delta.content.as_deref(),
            Some("The scanner ")
        );
        assert_eq!(chunk.choices[0].delta.reasoning_content, None);
    }

    #[test]
    fn thinking_parses_under_either_spelling() {
        let current =
            parse_chunk(r#"{"choices":[{"delta":{"reasoning_content":"hmm"}}]}"#).expect("chunk");
        let older = parse_chunk(r#"{"choices":[{"delta":{"reasoning":"hmm"}}]}"#).expect("chunk");
        assert_eq!(
            current.choices[0].delta.reasoning_content.as_deref(),
            Some("hmm")
        );
        assert_eq!(
            older.choices[0].delta.reasoning_content.as_deref(),
            Some("hmm")
        );
    }

    #[test]
    fn the_final_chunk_carries_usage_and_timings_with_no_choices() {
        let chunk = parse_chunk(
            r#"{"choices":[],
                "usage":{"prompt_tokens":812,"completion_tokens":140,"total_tokens":952},
                "timings":{"prompt_n":812,"prompt_ms":406.0,"predicted_n":140,
                           "predicted_ms":1750.0,"draft_n":40,"draft_n_accepted":30}}"#,
        )
        .expect("chunk");

        let usage = chunk.usage.expect("usage");
        assert_eq!(usage.prompt_tokens, 812);
        let timings = chunk.timings.expect("timings");
        assert_eq!(timings.prompt_per_second(), Some(2000.0));
        assert_eq!(timings.generation_per_second(), Some(80.0));
        assert_eq!(timings.draft_acceptance(), Some(0.75));
    }

    #[test]
    fn the_whole_prompt_is_the_cached_part_plus_the_prefilled_part() {
        let timings = Timings {
            cache_n: 4_977,
            prompt_n: 23,
            prompt_ms: 10.0,
            ..Default::default()
        };
        assert_eq!(timings.prompt_total(), 5_000);
        // The rate is over what was actually processed, which is the honest
        // number for "how fast did it prefill".
        assert_eq!(timings.prompt_per_second(), Some(2300.0));
    }

    #[test]
    fn timings_without_speculative_decoding_report_no_acceptance() {
        let timings = Timings {
            predicted_n: 10,
            predicted_ms: 100.0,
            ..Default::default()
        };
        assert_eq!(timings.draft_acceptance(), None);
        assert_eq!(timings.prompt_per_second(), None);
    }

    #[test]
    fn an_error_frame_is_a_server_error_not_a_parse_failure() {
        let error = parse_chunk(
            r#"{"error":{"message":"context size exceeded","type":"server_error","code":"500"}}"#,
        )
        .expect_err("error");
        match error {
            WireError::Server(server) => {
                assert_eq!(server.message, "context size exceeded");
                assert_eq!(server.kind.as_deref(), Some("server_error"));
            }
            other => panic!("expected a server error, got {other:?}"),
        }
    }

    #[test]
    fn a_frame_that_is_neither_keeps_what_it_said() {
        let error = parse_chunk("<html>502 Bad Gateway</html>").expect_err("error");
        match error {
            WireError::Malformed { payload, .. } => assert!(payload.contains("502")),
            other => panic!("expected malformed, got {other:?}"),
        }
    }

    #[test]
    fn a_reply_split_mid_line_emits_nothing_until_it_is_whole() {
        let mut decoder = SseDecoder::new();
        assert_eq!(decoder.push("data: {\"cho"), vec![]);
        assert_eq!(decoder.push("ices\":[]}"), vec![]);
        assert_eq!(
            decoder.push("\n\n"),
            vec![Sse::Data(r#"{"choices":[]}"#.into())]
        );
    }

    #[test]
    fn several_events_in_one_read_come_out_in_order() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push("data: {\"a\":1}\n\ndata: {\"b\":2}\n\ndata: [DONE]\n\n");
        assert_eq!(
            events,
            vec![
                Sse::Data(r#"{"a":1}"#.into()),
                Sse::Data(r#"{"b":2}"#.into()),
                Sse::Done,
            ]
        );
    }

    #[test]
    fn comments_and_carriage_returns_are_dropped() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push(": keep-alive\r\nevent: message\r\ndata: {\"a\":1}\r\n\r\n");
        assert_eq!(events, vec![Sse::Data(r#"{"a":1}"#.into())]);
    }

    #[test]
    fn a_stream_that_closes_without_a_newline_still_yields_its_last_event() {
        let mut decoder = SseDecoder::new();
        assert_eq!(decoder.push("data: [DONE]"), vec![]);
        assert_eq!(decoder.finish(), vec![Sse::Done]);
    }
}
