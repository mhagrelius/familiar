//! Vectors, on a thread of their own.
//!
//! `recall` used to match words. It found "roof" from "roof" and nothing at all
//! from "the thing the contractor did in spring", and the guidance had a
//! paragraph telling the model to try two or three phrasings — which is a
//! workaround for a search engine, written into a prompt, paid for on every
//! turn. Brain grew a semantic half; this is the socket that feeds it.
//!
//! **Everything here is blocking, on one worker thread.** Two reasons, and
//! neither is about the query:
//!
//! * A catch-up pass is a batch — on a first run it is every note in the vault,
//!   one request each — and there is nothing for the user to watch while it
//!   happens. Running it on the main loop would stall the window for as long as
//!   the vault takes.
//! * A `soup::Session` belongs to the thread that made it. One thread, one
//!   session, and no sharing: the alternative is the kind of bug that only shows
//!   up on a slow day.
//!
//! Replies come back with [`glib::idle_add_once`], which is the whole of the
//! plumbing — there is no channel crate here and no runtime.
//!
//! **The whole thing is optional.** No server, a server without `--embeddings`,
//! a model that changed underneath the cache: each of those ends with `recall`
//! searching lexically, which is what it did before and is a perfectly good
//! answer. Nothing in the application waits on this.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use brain::model::semantic::{self, Digest, EmbedError, Embedder, Store, Wanted};

pub use crate::model::memory::Passage;
use gtk::glib;
use soup::prelude::*;

/// What a model wants prepended to a passage and to a question.
///
/// The retrieval models worth running locally are trained asymmetrically, and
/// the prefix is not decoration: it is how the model is told which side of the
/// asymmetry this text is on. Omitting it costs recall silently, since the
/// vectors still come back and still rank.
///
/// Matched on the model's name because that is all the server offers, and the
/// scheme is folded into what the store keys on — vectors made with a prefix and
/// without it are not comparable, and the model's own name would not have
/// changed to say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Prefixes {
    scheme: &'static str,
    document: &'static str,
    query: &'static str,
}

fn prefixes_for(model: &str) -> Prefixes {
    let name = model.to_lowercase();
    if name.contains("nomic-embed") {
        Prefixes {
            scheme: "nomic",
            document: "search_document: ",
            query: "search_query: ",
        }
    } else if name.contains("e5") {
        Prefixes {
            scheme: "e5",
            document: "passage: ",
            query: "query: ",
        }
    } else if name.contains("bge") && !name.contains("m3") {
        // BGE prefixes the question only, and says so in its model card.
        Prefixes {
            scheme: "bge",
            document: "",
            query: "Represent this sentence for searching relevant passages: ",
        }
    } else {
        Prefixes {
            scheme: "plain",
            document: "",
            query: "",
        }
    }
}

/// A llama.cpp server that turns text into vectors, on the worker thread.
struct Llama {
    base: String,
    session: soup::Session,
    model: String,
    prefixes: Prefixes,
}

/// How long a *query* may take. A `recall` waits on this inside a turn, so the
/// worst case is a stall somebody watches.
///
/// Measured against the NAS this runs on: a query embeds in 0.05 s, so eight
/// seconds is a hundred-fold headroom and exists only to bound the case where
/// the box is asleep. An unroutable address falls back to matching words after
/// exactly this long.
const QUERY_TIMEOUT: u32 = 8;

/// How long a batch of *document* chunks may take.
///
/// Much longer, because nobody is waiting on it and because the far side is CPU
/// inference: one 1,500-character chunk is about a second there, and a request
/// carries [`CHUNKS_PER_REQUEST`] of them. Sixty seconds is roughly fifteen
/// times the expected cost, which leaves room for a NAS that is also doing
/// something else.
const BATCH_TIMEOUT: u32 = 60;

/// The most characters in one chunk sent for embedding.
///
/// Brain splits notes at 2,000 characters, which it calls "a conservative 512
/// tokens for English prose". It is not conservative enough: real prose came out
/// at 519 tokens and the server refused it outright — `input (519 tokens) is too
/// large to process`, because llama.cpp's physical batch defaults to 512. Every
/// note with a full-size chunk would have failed to embed, silently, for ever.
///
/// 1,500 characters measured at about 390 tokens on the same prose, which leaves
/// room for text that tokenizes denser than English. A server configured with a
/// larger batch loses nothing by it but a few more requests.
const SAFE_CHUNK_CHARS: usize = 1_500;

/// How many chunks go in one request.
///
/// All sixteen of a note's chunks at once took 17.8 s on the NAS — one long
/// request that blocks the lane and, on a slower day, trips the timeout. Four is
/// about four seconds, which is a unit of work worth retrying.
const CHUNKS_PER_REQUEST: usize = 4;

/// How long to leave a server alone after it failed to answer.
///
/// Without this, every `recall` made while the server is unreachable pays a
/// fresh connection timeout before falling back to matching words — which turns
/// "semantic search is unavailable" into "every note lookup takes eight
/// seconds". One request finds out; the rest of the minute is free.
const RETRY_AFTER: Duration = Duration::from_secs(60);

impl Llama {
    fn connect(base: &str, timeout: u32) -> Result<Self, EmbedError> {
        let session = soup::Session::builder().timeout(timeout).build();
        let base = base.trim_end_matches('/').to_string();
        let body = get(&session, &format!("{base}/v1/models"))?;

        let parsed: serde_json::Value = serde_json::from_str(&body)
            .map_err(|error| EmbedError(format!("the server's model list is not JSON: {error}")))?;
        let model = parsed["data"][0]["id"]
            .as_str()
            .ok_or_else(|| EmbedError("the server named no model".into()))?
            .to_string();

        let prefixes = prefixes_for(&model);
        Ok(Self {
            base,
            session,
            model,
            prefixes,
        })
    }

    fn embed_prefixed(&self, prefix: &str, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let input: Vec<String> = texts.iter().map(|text| format!("{prefix}{text}")).collect();
        let request = serde_json::json!({ "input": input, "model": self.model });
        let body = post(
            &self.session,
            &format!("{}/v1/embeddings", self.base),
            &request.to_string(),
        )?;
        parse_embeddings(&body, texts.len())
    }
}

impl Embedder for Llama {
    /// The model *and* how it was prompted. Changing the prefix scheme changes
    /// every vector the model produces, and the model's own name would not move
    /// to say so — the store would go on comparing new vectors against old ones
    /// and rank plausibly and wrongly.
    fn model(&self) -> String {
        format!("{}+{}", self.model, self.prefixes.scheme)
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.embed_prefixed(self.prefixes.document, texts)
    }

    fn embed_query(&self, query: &str) -> Result<Vec<f32>, EmbedError> {
        self.embed_prefixed(
            self.prefixes.query,
            std::slice::from_ref(&query.to_string()),
        )?
        .into_iter()
        .next()
        .ok_or_else(|| EmbedError("no vector came back for the query".into()))
    }
}

fn get(session: &soup::Session, url: &str) -> Result<String, EmbedError> {
    let message =
        soup::Message::new("GET", url).map_err(|_| EmbedError(format!("{url} is not a URL")))?;
    let bytes = session
        .send_and_read(&message, gio::Cancellable::NONE)
        .map_err(|error| EmbedError(error.to_string()))?;
    let status = message.status_code() as u16;
    if !(200..300).contains(&status) {
        return Err(EmbedError(format!("the server answered {status}")));
    }
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

fn post(session: &soup::Session, url: &str, body: &str) -> Result<String, EmbedError> {
    let message =
        soup::Message::new("POST", url).map_err(|_| EmbedError(format!("{url} is not a URL")))?;
    message.set_request_body_from_bytes(
        Some("application/json"),
        Some(&glib::Bytes::from_owned(body.as_bytes().to_vec())),
    );
    let bytes = session
        .send_and_read(&message, gio::Cancellable::NONE)
        .map_err(|error| EmbedError(error.to_string()))?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    let status = message.status_code() as u16;
    if !(200..300).contains(&status) {
        return Err(EmbedError(complaint(&text, status)));
    }
    Ok(text)
}

/// What went wrong, from the server's own words where it gave any.
///
/// One of these is worth singling out — "start it with `--embeddings`" is the
/// mistake people actually make, and a bare 500 tells them nothing.
fn complaint(body: &str, status: u16) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|parsed| {
            parsed["error"]["message"]
                .as_str()
                .or_else(|| parsed["message"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("the server answered {status}"))
}

/// Pull the vectors out of a `/v1/embeddings` response.
///
/// The count has to match: anything else is a protocol error, and guessing at
/// the alignment would silently give one note another's meaning.
pub fn parse_embeddings(body: &str, expected: usize) -> Result<Vec<Vec<f32>>, EmbedError> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| EmbedError(format!("that is not JSON: {error}")))?;
    if let Some(message) = parsed["error"]["message"].as_str() {
        return Err(EmbedError(message.to_string()));
    }
    let data = parsed["data"]
        .as_array()
        .ok_or_else(|| EmbedError("the reply had no data".into()))?;

    let mut vectors = Vec::with_capacity(data.len());
    for item in data {
        // llama.cpp answers `embedding: [f32]` for one vector and
        // `embedding: [[f32]]` when the server is in pooling-none mode. Take
        // the first row of the latter rather than failing on it.
        let raw = &item["embedding"];
        let numbers = raw
            .as_array()
            .and_then(|row| match row.first() {
                Some(serde_json::Value::Array(_)) => row.first().and_then(|f| f.as_array()),
                _ => Some(row),
            })
            .ok_or_else(|| EmbedError("an item had no embedding".into()))?;
        vectors.push(
            numbers
                .iter()
                .filter_map(|number| number.as_f64().map(|n| n as f32))
                .collect(),
        );
    }
    if vectors.len() != expected {
        return Err(EmbedError(format!(
            "asked for {expected} embeddings and got {}",
            vectors.len()
        )));
    }
    Ok(vectors)
}

// -- the worker ----------------------------------------------------------------

/// One question to embed. Somebody is waiting on this.
struct Query {
    text: String,
    answer: Box<dyn FnOnce(Option<Vec<f32>>) + Send>,
}

/// A whole vault to bring level. Nobody is waiting on this.
struct Batch {
    passages: Vec<Passage>,
    store_path: PathBuf,
    answer: Box<dyn FnOnce(Option<Store>) + Send>,
}

/// A handle on the embedding threads.
///
/// **Two of them, and that is the point.** Both lanes talk to the same server,
/// but a catch-up pass over a real vault is hundreds of requests at about a
/// second each on CPU — and a `recall` sharing that queue would sit behind the
/// whole pass rather than behind one request. Splitting them costs an idle
/// thread and buys an interactive lane that a background job cannot block.
///
/// Each owns its own `soup::Session`, which it must: a session belongs to the
/// thread that made it. They also carry different timeouts, because eight
/// seconds is generous for a question and would kill a batch.
///
/// Dropping the handle ends both threads: their receive loops stop when the
/// senders go, which is what closing the application does.
pub struct Embeddings {
    queries: mpsc::Sender<Query>,
    batches: mpsc::Sender<Batch>,
}

impl Embeddings {
    /// Start the threads. They connect lazily, on the first job, so a server
    /// that is not running costs nothing at launch.
    pub fn start(base: &str) -> Self {
        let (queries, asked) = mpsc::channel::<Query>();
        let (batches, wanted) = mpsc::channel::<Batch>();

        let url = base.to_string();
        std::thread::Builder::new()
            .name("familiar-embed-query".into())
            .spawn(move || query_lane(&url, asked))
            .expect("a thread");

        let url = base.to_string();
        std::thread::Builder::new()
            .name("familiar-embed-batch".into())
            .spawn(move || batch_lane(&url, wanted))
            .expect("a thread");

        Self { queries, batches }
    }

    /// Embed a question. `None` — no server, or it refused — means the caller
    /// searches lexically, which is what it did before this existed.
    pub fn query<F>(&self, text: &str, answer: F)
    where
        F: FnOnce(Option<Vec<f32>>) + 'static,
    {
        // A send that fails means the thread is gone. Nothing waits on this, so
        // the caller is told what an absent server would have told it.
        let _ = self.queries.send(Query {
            text: text.to_string(),
            answer: on_the_main_thread(answer),
        });
    }

    /// Bring the vectors level with the vault.
    ///
    /// `None` means nothing changed, or nothing could — either way the caller
    /// keeps the store it already had.
    pub fn catch_up<F>(&self, passages: Vec<Passage>, store_path: PathBuf, answer: F)
    where
        F: FnOnce(Option<Store>) + 'static,
    {
        let _ = self.batches.send(Batch {
            passages,
            store_path,
            answer: on_the_main_thread(answer),
        });
    }
}

/// Wrap a main-thread callback so a worker can hand it a value.
///
/// The closure the caller wrote is not `Send` — it captures widgets and `Rc`s —
/// so it is moved into an idle callback instead, and only the *result* crosses
/// the thread boundary. `glib::idle_add_once` runs on the main context, which is
/// where the caller already was.
fn on_the_main_thread<T, F>(answer: F) -> Box<dyn FnOnce(T) + Send>
where
    T: Send + 'static,
    F: FnOnce(T) + 'static,
{
    let answer = glib::thread_guard::ThreadGuard::new(answer);
    Box::new(move |value: T| {
        glib::idle_add_once(move || {
            (answer.into_inner())(value);
        });
    })
}

/// Connect on demand, and stay away for a while after a failure.
///
/// A server started after the application is a normal thing to do, so giving up
/// for ever would need a restart to notice — but retrying on every job means
/// every job made while it is down waits out a connection timeout first.
struct Lane {
    base: String,
    timeout: u32,
    server: Option<Llama>,
    failed_at: Option<Instant>,
}

impl Lane {
    fn new(base: &str, timeout: u32) -> Self {
        Self {
            base: base.to_string(),
            timeout,
            server: None,
            failed_at: None,
        }
    }

    fn server(&mut self) -> Option<&Llama> {
        let resting = self.failed_at.is_some_and(|at| at.elapsed() < RETRY_AFTER);
        if self.server.is_none() && !resting {
            match Llama::connect(&self.base, self.timeout) {
                Ok(llama) => {
                    self.server = Some(llama);
                    self.failed_at = None;
                }
                Err(_) => self.failed_at = Some(Instant::now()),
            }
        }
        self.server.as_ref()
    }

    /// It answered before and does not now. Drop it, and rest before the next.
    fn broke(&mut self) {
        self.server = None;
        self.failed_at = Some(Instant::now());
    }
}

fn query_lane(base: &str, inbox: mpsc::Receiver<Query>) {
    let mut lane = Lane::new(base, QUERY_TIMEOUT);
    for Query { text, answer } in inbox {
        let vector = lane
            .server()
            .and_then(|llama| llama.embed_query(&text).ok());
        if vector.is_none() && lane.server.is_some() {
            lane.broke();
        }
        answer(vector);
    }
}

fn batch_lane(base: &str, inbox: mpsc::Receiver<Batch>) {
    let mut lane = Lane::new(base, BATCH_TIMEOUT);
    for Batch {
        passages,
        store_path,
        answer,
    } in inbox
    {
        let Some(llama) = lane.server() else {
            answer(None);
            continue;
        };
        match catch_up(llama, &passages, &store_path) {
            Some(store) => answer(Some(store)),
            None => {
                lane.broke();
                answer(None);
            }
        }
    }
}

/// Bring the store level with a snapshot of the vault.
///
/// Brain's own `semantic::catch_up` wants an `Index`, which lives on the main
/// thread with the rest of `Memory`. The pieces it is built from are public, so
/// this is the same lifecycle over a snapshot that can cross a thread: notice
/// what changed, carry moves across, forget what is gone, embed the rest.
///
/// A server that stops answering ends the pass rather than failing it. What was
/// embedded stays embedded and the next pass resumes — hammering a server that
/// just refused fifty requests with fifty more is how a laptop's fans come on.
fn catch_up(llama: &Llama, passages: &[Passage], store_path: &std::path::Path) -> Option<Store> {
    let mut store = Store::load(store_path);
    let reset = store.set_model(&llama.model());

    let wanted: Vec<Wanted> = passages
        .iter()
        .map(|passage| Wanted {
            id: passage.id.clone(),
            digest: semantic::digest_of(&passage.title, &passage.text),
        })
        .collect();
    let plan = semantic::plan(&store, &wanted);
    let embedding = plan.embed.clone();
    // Bookkeeping first: it needs no model, so a store written out after this
    // point is behind rather than wrong, whatever the server does next.
    store.apply(&plan);

    let mut wrote = plan.moved.len() + plan.drop.len() > 0 || reset;
    let mut refused = false;
    for id in &embedding {
        let Some(passage) = passages.iter().find(|passage| passage.id == *id) else {
            continue;
        };
        let pieces = shorten(semantic::chunks(&passage.title, &passage.text));
        if pieces.is_empty() {
            continue;
        }
        match embed_all(llama, &pieces) {
            Some(vectors) => {
                store.insert(
                    id,
                    semantic::digest_of(&passage.title, &passage.text),
                    vectors,
                );
                wrote = true;
            }
            None => {
                refused = true;
                break;
            }
        }
    }

    if wrote {
        let _ = store.save(store_path);
    }
    // A pass that got nowhere at all is reported as a failure so the worker
    // reconnects; one that got part way is a result, and the rest waits.
    if refused && !wrote {
        return None;
    }
    Some(store)
}

/// Every chunk of one note, in requests small enough to be worth retrying.
///
/// Sending all sixteen at once took 17.8 s against the NAS this was built for —
/// one long request holding the lane, and on a slower day one that trips the
/// timeout and loses the whole note. In fours it is about four seconds a
/// request. `None` if any of them failed: a note with half its vectors would
/// rank as if the other half did not exist, and leaving it unembedded is the
/// honest state — the next pass tries again.
fn embed_all(llama: &Llama, pieces: &[String]) -> Option<Vec<Vec<f32>>> {
    let mut vectors = Vec::with_capacity(pieces.len());
    for group in pieces.chunks(CHUNKS_PER_REQUEST) {
        vectors.extend(llama.embed(group).ok()?);
    }
    Some(vectors)
}

/// Cut anything too long for the server to accept in one go.
///
/// Brain splits at 2,000 characters and calls that "a conservative 512 tokens
/// for English prose". Measured, real prose came out at 519 and llama.cpp
/// refused it — `input (519 tokens) is too large to process`, because the
/// physical batch defaults to 512. Every note holding a full-size chunk failed
/// to embed, and failed silently, because an unembedded note simply does not
/// come back from a semantic search.
///
/// Split on whitespace where there is any, so a cut lands between words rather
/// than through one.
fn shorten(pieces: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(pieces.len());
    for piece in pieces {
        let mut rest = piece.as_str();
        while rest.chars().count() > SAFE_CHUNK_CHARS {
            let mut at = rest
                .char_indices()
                .nth(SAFE_CHUNK_CHARS)
                .map_or(rest.len(), |(at, _)| at);
            if let Some(space) = rest[..at].rfind(char::is_whitespace) {
                // Not so far back that the piece becomes tiny — a run of text
                // with no spaces in it has to be cut somewhere.
                if space > at / 2 {
                    at = space;
                }
            }
            let (head, tail) = rest.split_at(at);
            if !head.trim().is_empty() {
                out.push(head.trim().to_string());
            }
            rest = tail.trim_start();
        }
        if !rest.trim().is_empty() {
            out.push(rest.trim().to_string());
        }
    }
    out
}

/// A note's digest, for a caller deciding whether a rescan changed anything.
pub fn digest_of(title: &str, text: &str) -> Digest {
    semantic::digest_of(title, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_with_one_vector_per_input_parses() {
        let body = r#"{"data":[{"embedding":[0.1,0.2]},{"embedding":[0.3,0.4]}]}"#;
        assert_eq!(
            parse_embeddings(body, 2).expect("parse"),
            [vec![0.1, 0.2], vec![0.3, 0.4]]
        );
    }

    #[test]
    fn a_nested_embedding_takes_its_first_row() {
        // llama.cpp answers this shape in pooling-none mode, and failing on it
        // would make the feature depend on a server flag nobody documents.
        let body = r#"{"data":[{"embedding":[[1.0,2.0]]}]}"#;
        assert_eq!(parse_embeddings(body, 1).expect("parse"), [vec![1.0, 2.0]]);
    }

    #[test]
    fn a_count_that_does_not_match_is_a_protocol_error() {
        // Guessing at the alignment would silently give one note another's
        // meaning, which nothing downstream could detect.
        let body = r#"{"data":[{"embedding":[0.1]}]}"#;
        assert!(parse_embeddings(body, 2).is_err());
    }

    #[test]
    fn a_server_without_embeddings_turned_on_says_so() {
        let body = r#"{"error":{"message":"This server does not support embeddings. Start it with `--embeddings`"}}"#;
        let error = parse_embeddings(body, 1).expect_err("should fail");
        assert!(error.0.contains("--embeddings"), "{error}");
    }

    #[test]
    fn a_reply_that_is_not_json_is_an_error_rather_than_a_panic() {
        assert!(parse_embeddings("<html>502</html>", 1).is_err());
    }

    #[test]
    fn the_prefix_scheme_is_part_of_what_the_store_keys_on() {
        // Vectors made with a prefix and without it are not comparable, and the
        // model's own name would not change to say so.
        assert_eq!(prefixes_for("nomic-embed-text-v1.5").scheme, "nomic");
        assert_eq!(prefixes_for("multilingual-e5-large").scheme, "e5");
        assert_eq!(prefixes_for("bge-small-en").scheme, "bge");
        assert_eq!(prefixes_for("something-else").scheme, "plain");
        // BGE prefixes the question and not the passage, which its card says.
        assert!(prefixes_for("bge-small-en").document.is_empty());
        assert!(!prefixes_for("bge-small-en").query.is_empty());
    }

    #[test]
    fn a_chunk_too_long_for_the_server_is_cut_before_it_is_sent() {
        // The bug this exists for: brain splits at 2,000 characters, real prose
        // came out at 519 tokens, and llama.cpp's physical batch defaults to
        // 512 — so every note with a full-size chunk failed to embed, and
        // failed silently, because an unembedded note simply does not come back
        // from a semantic search.
        let long = "word ".repeat(800);
        let pieces = shorten(vec![long.clone()]);
        assert!(pieces.len() > 1, "{} pieces", pieces.len());
        for piece in &pieces {
            assert!(
                piece.chars().count() <= SAFE_CHUNK_CHARS,
                "a piece ran to {}",
                piece.chars().count()
            );
        }
        // Nothing is lost on the way: the words come back in order.
        let rejoined = pieces.join(" ");
        assert_eq!(rejoined.split_whitespace().count(), 800);
    }

    #[test]
    fn a_chunk_that_already_fits_is_left_exactly_as_it_was() {
        let pieces = vec!["a short note about the roof".to_string()];
        assert_eq!(shorten(pieces.clone()), pieces);
    }

    #[test]
    fn a_run_of_text_with_no_spaces_is_still_cut() {
        // Otherwise a base64 blob or a long URL in a note is a chunk that can
        // never be sent, and the note never becomes searchable.
        let unbroken = "x".repeat(SAFE_CHUNK_CHARS * 3);
        let pieces = shorten(vec![unbroken]);
        assert_eq!(pieces.len(), 3);
        assert!(pieces.iter().all(|p| p.chars().count() <= SAFE_CHUNK_CHARS));
    }

    #[test]
    fn a_cut_lands_between_words_rather_than_through_one() {
        let prose = "The north slope was replaced in April. ".repeat(80);
        let pieces = shorten(vec![prose]);
        assert!(pieces.len() > 1);
        for piece in &pieces {
            assert!(
                !piece.starts_with(' ') && !piece.ends_with(' '),
                "{piece:?}"
            );
        }
    }

    #[test]
    fn a_complaint_prefers_the_servers_own_words() {
        assert_eq!(
            complaint(r#"{"error":{"message":"no slot available"}}"#, 503),
            "no slot available"
        );
        assert_eq!(complaint("<html>", 502), "the server answered 502");
    }
}
