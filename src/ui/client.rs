//! Talking to llama-server, on the main loop.
//!
//! libsoup is the platform's HTTP client and its async calls complete on the
//! GLib main loop, so a streamed turn is a chain of callbacks on the thread
//! that owns the widgets — no runtime, no channel, no worker thread, and no
//! hand-off to get the text back onto the UI thread. Cancellation is a
//! `gio::Cancellable`, which is the same object the stop button and Escape both
//! trigger.
//!
//! Bytes are decoded here and only here. A socket read ends wherever the socket
//! decides, which is regularly in the middle of a multi-byte character — an
//! em-dash split across two reads becomes two replacement characters if each
//! read is decoded on its own. The incomplete tail is held back for the next
//! read, so what reaches [`crate::model::turn::TurnStream`] is always valid
//! UTF-8.

use std::cell::RefCell;
use std::rc::Rc;

use gio::prelude::*;
use gtk::glib;
use soup::prelude::*;

use crate::model::wire::ChatRequest;

/// How much to ask for per read. A turn arrives a few tokens at a time, so this
/// is never full.
const READ_SIZE: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// Nothing answered. The server is asleep, or the URL is wrong.
    Unreachable(String),
    /// It answered, with a refusal.
    Http { status: u16, body: String },
    /// It answered and then the connection broke.
    Transport(String),
    /// The stop button, or Escape.
    Cancelled,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(detail) => write!(f, "{detail}"),
            Self::Http { status, body } => {
                let body = body.trim();
                if body.is_empty() {
                    write!(f, "the server answered {status}")
                } else {
                    write!(f, "{body}")
                }
            }
            Self::Transport(detail) => write!(f, "{detail}"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// What the server says about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerInfo {
    /// `n_ctx`: what the context-usage bar measures against. Worth asking for
    /// rather than assuming — this one is launched with 175,104.
    pub context_window: Option<u32>,
    /// The alias llama-server was started with, for the bottom bar.
    pub model: Option<String>,
}

pub struct Client {
    session: soup::Session,
    base_url: RefCell<String>,
}

impl Client {
    pub fn new(base_url: &str) -> Self {
        Self {
            session: soup::Session::new(),
            base_url: RefCell::new(base_url.trim_end_matches('/').to_string()),
        }
    }

    pub fn set_base_url(&self, base_url: &str) {
        self.base_url
            .replace(base_url.trim_end_matches('/').to_string());
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.borrow())
    }

    /// Ask the server what it is. Answers `None` for anything it does not say,
    /// rather than guessing.
    pub fn probe<F>(&self, on_answer: F)
    where
        F: Fn(Result<ServerInfo, ClientError>) + 'static,
    {
        let message = soup::Message::new("GET", &self.url("/props"));
        let Ok(message) = message else {
            on_answer(Err(ClientError::Unreachable(
                "that server address is not a URL".into(),
            )));
            return;
        };

        // Cloned for the callback: the borrow above outlives the call, so the
        // status has to be read through a handle the closure owns.
        let sent = message.clone();
        self.session.send_and_read_async(
            &message,
            glib::Priority::DEFAULT,
            gio::Cancellable::NONE,
            move |result| match result {
                Ok(bytes) => {
                    let status = sent.status_code() as u16;
                    if !(200..300).contains(&status) {
                        on_answer(Err(ClientError::Http {
                            status,
                            body: String::from_utf8_lossy(&bytes).to_string(),
                        }));
                        return;
                    }
                    on_answer(Ok(parse_props(&String::from_utf8_lossy(&bytes))));
                }
                Err(error) => on_answer(Err(ClientError::Unreachable(error.to_string()))),
            },
        );
    }

    /// Start a streaming completion.
    ///
    /// `on_text` is called with every decoded fragment as it arrives, and
    /// `on_finished` exactly once. The returned `Cancellable` stops it.
    pub fn stream<T, F>(
        &self,
        request: &ChatRequest,
        on_text: T,
        on_finished: F,
    ) -> gio::Cancellable
    where
        T: Fn(&str) + 'static,
        F: FnOnce(Result<(), ClientError>) + 'static,
    {
        let cancellable = gio::Cancellable::new();
        let on_text = Rc::new(on_text);
        // FnOnce behind a RefCell so the read loop, which may end at several
        // different points, can take it and call it exactly once.
        let on_finished = Rc::new(RefCell::new(Some(on_finished)));

        let body = match serde_json::to_vec(request) {
            Ok(body) => body,
            Err(error) => {
                finish(&on_finished, Err(ClientError::Transport(error.to_string())));
                return cancellable;
            }
        };

        let message = match soup::Message::new("POST", &self.url("/v1/chat/completions")) {
            Ok(message) => message,
            Err(_) => {
                finish(
                    &on_finished,
                    Err(ClientError::Unreachable(
                        "that server address is not a URL".into(),
                    )),
                );
                return cancellable;
            }
        };
        message.set_request_body_from_bytes(
            Some("application/json"),
            Some(&glib::Bytes::from_owned(body)),
        );

        let sent = message.clone();
        self.session.send_async(
            &message,
            glib::Priority::DEFAULT,
            Some(&cancellable),
            glib::clone!(
                #[strong]
                cancellable,
                move |result| {
                    let stream = match result {
                        Ok(stream) => stream,
                        Err(error) => {
                            finish(&on_finished, Err(classify(&error, &cancellable)));
                            return;
                        }
                    };

                    let status = sent.status_code() as u16;
                    if !(200..300).contains(&status) {
                        // The body is the interesting part of a refusal, so it
                        // is read before the error is reported.
                        read_all(stream, cancellable.clone(), move |body| {
                            finish(&on_finished, Err(ClientError::Http { status, body }));
                        });
                        return;
                    }

                    read_stream(stream, cancellable, on_text, on_finished);
                }
            ),
        );

        cancellable
    }
}

fn finish<F: FnOnce(Result<(), ClientError>) + 'static>(
    on_finished: &Rc<RefCell<Option<F>>>,
    outcome: Result<(), ClientError>,
) {
    if let Some(finisher) = on_finished.borrow_mut().take() {
        finisher(outcome);
    }
}

/// Read until the stream ends, handing every decoded fragment to `on_text`.
///
/// Written as a function that re-arms itself rather than a loop: each read is a
/// callback on the main loop, so the "loop" is the callback scheduling the next
/// read.
fn read_stream<T, F>(
    stream: gio::InputStream,
    cancellable: gio::Cancellable,
    on_text: Rc<T>,
    on_finished: Rc<RefCell<Option<F>>>,
) where
    T: Fn(&str) + 'static,
    F: FnOnce(Result<(), ClientError>) + 'static,
{
    // The bytes of a character that was split across two reads.
    let tail: Rc<RefCell<Vec<u8>>> = Rc::new(RefCell::new(Vec::new()));
    read_next(stream, cancellable, on_text, on_finished, tail);
}

fn read_next<T, F>(
    stream: gio::InputStream,
    cancellable: gio::Cancellable,
    on_text: Rc<T>,
    on_finished: Rc<RefCell<Option<F>>>,
    tail: Rc<RefCell<Vec<u8>>>,
) where
    T: Fn(&str) + 'static,
    F: FnOnce(Result<(), ClientError>) + 'static,
{
    stream.clone().read_async(
        vec![0u8; READ_SIZE],
        glib::Priority::DEFAULT,
        Some(&cancellable.clone()),
        move |result| match result {
            Ok((buffer, 0)) => {
                let _ = buffer;
                // A tail left at end of stream is a truncated character, which
                // means the connection died mid-token. There is nothing to
                // salvage and the turn keeps whatever arrived before it.
                tail.borrow_mut().clear();
                finish(&on_finished, Ok(()));
            }
            Ok((buffer, read)) => {
                let mut pending = tail.borrow_mut();
                pending.extend_from_slice(&buffer[..read]);
                let text = take_valid_utf8(&mut pending);
                drop(pending);
                if !text.is_empty() {
                    on_text(&text);
                }
                read_next(stream, cancellable, on_text, on_finished, tail);
            }
            Err(error) => {
                finish(&on_finished, Err(classify(&error.1, &cancellable)));
            }
        },
    );
}

/// Read a whole body to a string, for the error path.
fn read_all<F: FnOnce(String) + 'static>(
    stream: gio::InputStream,
    cancellable: gio::Cancellable,
    on_body: F,
) {
    let collected = Rc::new(RefCell::new(Vec::new()));
    let on_body = Rc::new(RefCell::new(Some(on_body)));
    read_all_next(stream, cancellable, collected, on_body);
}

fn read_all_next<F: FnOnce(String) + 'static>(
    stream: gio::InputStream,
    cancellable: gio::Cancellable,
    collected: Rc<RefCell<Vec<u8>>>,
    on_body: Rc<RefCell<Option<F>>>,
) {
    stream.clone().read_async(
        vec![0u8; READ_SIZE],
        glib::Priority::DEFAULT,
        Some(&cancellable.clone()),
        move |result| match result {
            Ok((_, 0)) | Err(_) => {
                if let Some(finisher) = on_body.borrow_mut().take() {
                    let body = String::from_utf8_lossy(&collected.borrow()).to_string();
                    finisher(body);
                }
            }
            Ok((buffer, read)) => {
                collected.borrow_mut().extend_from_slice(&buffer[..read]);
                read_all_next(stream, cancellable, collected, on_body);
            }
        },
    );
}

/// Take everything that is valid UTF-8, leaving an incomplete trailing
/// character behind for the next read.
fn take_valid_utf8(buffer: &mut Vec<u8>) -> String {
    let valid_up_to = match std::str::from_utf8(buffer) {
        Ok(_) => buffer.len(),
        Err(error) => match error.error_len() {
            // A truly invalid sequence, not a split character. Nothing can be
            // done with it, so it is consumed lossily rather than jamming the
            // stream forever.
            Some(_) => {
                let text = String::from_utf8_lossy(buffer).to_string();
                buffer.clear();
                return text;
            }
            None => error.valid_up_to(),
        },
    };
    let rest = buffer.split_off(valid_up_to);
    let text = String::from_utf8_lossy(buffer).to_string();
    *buffer = rest;
    text
}

fn classify(error: &glib::Error, cancellable: &gio::Cancellable) -> ClientError {
    if cancellable.is_cancelled() || error.matches(gio::IOErrorEnum::Cancelled) {
        return ClientError::Cancelled;
    }
    if error.matches(gio::IOErrorEnum::ConnectionRefused)
        || error.matches(gio::IOErrorEnum::HostNotFound)
        || error.matches(gio::IOErrorEnum::HostUnreachable)
        || error.matches(gio::IOErrorEnum::NetworkUnreachable)
        || error.matches(gio::IOErrorEnum::TimedOut)
    {
        return ClientError::Unreachable(error.message().to_string());
    }
    ClientError::Transport(error.message().to_string())
}

/// llama.cpp's `/props`. Everything here is optional: a gateway in front of the
/// server may answer a shape of its own, and a missing field is a thing not
/// shown rather than a failure.
fn parse_props(body: &str) -> ServerInfo {
    let Ok(props) = serde_json::from_str::<serde_json::Value>(body) else {
        return ServerInfo::default();
    };
    let context_window = props
        .get("default_generation_settings")
        .and_then(|settings| settings.get("n_ctx"))
        .and_then(serde_json::Value::as_u64)
        .map(|n_ctx| n_ctx as u32);
    let model = props
        .get("model_alias")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            props
                .get("model_path")
                .and_then(serde_json::Value::as_str)
                .and_then(|path| path.rsplit(['/', '\\']).next())
                .map(|file| file.trim_end_matches(".gguf").to_string())
        });
    ServerInfo {
        context_window,
        model,
    }
}

// -- Exa ---------------------------------------------------------------------

/// Post JSON to a URL that needs an API key, and hand back the body.
///
/// Exa rather than a general HTTP client: this is the one place Familiar talks
/// to anything that is not the local server, and keeping it here means the
/// egress points can be counted on one hand.
/// A GET that identifies the application, and hands back the body or nothing.
///
/// `api.weather.gov` refuses a request with no `User-Agent` and asks that it
/// name the application, so this cannot go through the generic session
/// defaults. `accept` is a parameter because the other caller is Hacker News'
/// search index, which wants plain JSON rather than the weather service's
/// `application/geo+json`. `Accept-Encoding: gzip` earns its place here rather than as a
/// nicety: the hourly forecast is 164 KB uncompressed and 5 KB gzipped, and
/// libsoup decompresses transparently.
///
/// A non-2xx is `None` rather than an error value, because every caller's
/// recovery is the same — try the next station, or do without that piece — and
/// distinguishing a 404 from a 503 would only give them something to ignore.
pub fn get_with_agent<F>(
    session: &soup::Session,
    url: &str,
    agent: &str,
    accept: &str,
    on_answer: F,
) where
    F: FnOnce(Option<String>) + 'static,
{
    let Ok(message) = soup::Message::new("GET", url) else {
        on_answer(None);
        return;
    };
    if let Some(headers) = message.request_headers() {
        headers.append("user-agent", agent);
        headers.append("accept", accept);
        headers.append("accept-encoding", "gzip");
    }

    let sent = message.clone();
    session.send_and_read_async(
        &message,
        glib::Priority::DEFAULT,
        gio::Cancellable::NONE,
        move |result| {
            let status = sent.status_code() as u16;
            match result {
                Ok(bytes) if (200..300).contains(&status) => {
                    on_answer(Some(String::from_utf8_lossy(&bytes).to_string()))
                }
                _ => on_answer(None),
            }
        },
    );
}

pub fn post_json<F>(
    session: &soup::Session,
    url: &str,
    api_key: &str,
    body: Vec<u8>,
    on_answer: F,
) -> gio::Cancellable
where
    F: FnOnce(Result<String, ClientError>) + 'static,
{
    let cancellable = gio::Cancellable::new();
    let Ok(message) = soup::Message::new("POST", url) else {
        on_answer(Err(ClientError::Unreachable("that is not a URL".into())));
        return cancellable;
    };

    message.set_request_body_from_bytes(
        Some("application/json"),
        Some(&glib::Bytes::from_owned(body)),
    );
    if let Some(headers) = message.request_headers() {
        headers.append("x-api-key", api_key);
        headers.append("accept", "application/json");
    }

    let sent = message.clone();
    session.send_and_read_async(
        &message,
        glib::Priority::DEFAULT,
        Some(&cancellable),
        move |result| match result {
            Ok(bytes) => {
                let status = sent.status_code() as u16;
                let body = String::from_utf8_lossy(&bytes).to_string();
                if (200..300).contains(&status) {
                    on_answer(Ok(body));
                } else {
                    on_answer(Err(ClientError::Http { status, body }));
                }
            }
            Err(error) => on_answer(Err(ClientError::Unreachable(error.to_string()))),
        },
    );
    cancellable
}

/// A session for talking to the web, kept apart from the one talking to the
/// model so a slow search cannot queue behind a streaming turn.
pub fn web_session() -> soup::Session {
    soup::Session::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_character_split_across_two_reads_is_not_corrupted() {
        // "…" is three bytes. Arriving one byte at a time, decoding each read
        // on its own would produce three replacement characters.
        let ellipsis = "…".as_bytes();
        let mut buffer = Vec::new();
        let mut out = String::new();
        for byte in ellipsis {
            buffer.push(*byte);
            out.push_str(&take_valid_utf8(&mut buffer));
        }
        assert_eq!(out, "…");
        assert!(buffer.is_empty());
    }

    #[test]
    fn whole_text_comes_straight_through() {
        let mut buffer = b"data: {\"a\":1}\n\n".to_vec();
        assert_eq!(take_valid_utf8(&mut buffer), "data: {\"a\":1}\n\n");
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_trailing_partial_character_is_held_back() {
        let mut buffer = "ok…".as_bytes()[..4].to_vec();
        assert_eq!(take_valid_utf8(&mut buffer), "ok");
        assert_eq!(buffer.len(), 2);
    }

    #[test]
    fn genuinely_invalid_bytes_do_not_jam_the_stream() {
        let mut buffer = vec![0xff, 0xfe, b'o', b'k'];
        let text = take_valid_utf8(&mut buffer);
        assert!(text.ends_with("ok"), "{text}");
        assert!(buffer.is_empty());
    }

    #[test]
    fn props_report_the_context_window_and_the_model() {
        let info = parse_props(
            r#"{"default_generation_settings":{"n_ctx":175104},"model_path":"/srv/llama/models/Qwen3.6-27B-UD-Q5_K_XL.gguf"}"#,
        );
        assert_eq!(info.context_window, Some(175_104));
        assert_eq!(info.model.as_deref(), Some("Qwen3.6-27B-UD-Q5_K_XL"));
    }

    #[test]
    fn an_alias_is_preferred_to_a_filename() {
        let info = parse_props(r#"{"model_alias":"qwen3.6-27b","model_path":"/srv/x.gguf"}"#);
        assert_eq!(info.model.as_deref(), Some("qwen3.6-27b"));
    }

    #[test]
    fn a_server_that_answers_something_else_is_not_a_failure() {
        assert_eq!(parse_props("not json"), ServerInfo::default());
        assert_eq!(parse_props("{}"), ServerInfo::default());
    }
}
