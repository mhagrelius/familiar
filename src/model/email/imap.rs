//! IMAP, as far as an assistant needs it: the commands, and how to read what
//! comes back.
//!
//! Pure. Nothing here opens a socket — [`crate::ui::mail`] does that — because
//! the fiddly half of IMAP is not the connection, it is the grammar, and a
//! grammar that can only be exercised against somebody's mail server is a
//! grammar nobody exercises.
//!
//! # The two things that make IMAP awkward
//!
//! **Literals.** A server may answer `{412}\r\n` followed by exactly 412 bytes
//! of anything at all, newlines and quotes included. So a response cannot be
//! split into lines and read: it has to be *counted*. [`Reader`] does that, and
//! it is the reason this module exists as a state machine rather than a set of
//! regexes.
//!
//! **UIDs, not sequence numbers.** A sequence number changes when anything in
//! the mailbox is deleted, so a number read in one command can name a different
//! message by the next. Every command here is a `UID` command, and the ids that
//! reach the model are UIDs. Getting this wrong archives the wrong mail.

use std::collections::VecDeque;

/// What a client may say. One variant per thing the assistant can do, so the
/// gate in `super` can reason about *verbs* rather than about strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Login {
        user: String,
        password: String,
    },
    /// Every mailbox, so "which folders are there" needs no guessing.
    List,
    Select(String),
    /// Read-only, for a mailbox the assistant is only looking at.
    Examine(String),
    /// `UID SEARCH <criteria>`, already validated by `super::search`.
    Search(String),
    /// Envelopes and a slice of the body, for a set of UIDs.
    Fetch(Vec<u32>),
    /// The whole text of one message.
    FetchBody(u32),
    AddFlags {
        uids: Vec<u32>,
        flags: Vec<String>,
    },
    RemoveFlags {
        uids: Vec<u32>,
        flags: Vec<String>,
    },
    Move {
        uids: Vec<u32>,
        mailbox: String,
    },
    /// What the server can do, asked before assuming it is Gmail or is not.
    Capability,
    /// `UID STORE … ±X-GM-LABELS`, which is the only way to put a label on a
    /// message that Gmail will actually show. Standard keywords are a different
    /// thing there and are invisible in the web interface.
    GmailLabels {
        uids: Vec<u32>,
        labels: Vec<String>,
        /// Adding, or taking away.
        add: bool,
    },
    Logout,
}

impl Command {
    /// The wire form, without the tag or the trailing CRLF.
    ///
    /// Kept separate from tagging so a test can read a command without
    /// counting how many have gone before it.
    pub fn line(&self) -> String {
        match self {
            Self::Login { user, password } => {
                format!("LOGIN {} {}", quoted(user), quoted(password))
            }
            Self::List => "LIST \"\" \"*\"".to_string(),
            Self::Select(mailbox) => format!("SELECT {}", quoted(mailbox)),
            Self::Examine(mailbox) => format!("EXAMINE {}", quoted(mailbox)),
            Self::Search(criteria) => format!("UID SEARCH {criteria}"),
            // `BODY.PEEK` rather than `BODY`: reading a message must not mark
            // it read. Deciding that for the user is exactly the kind of
            // silent side effect an assistant should never have.
            Self::Fetch(uids) => format!(
                "UID FETCH {} (UID FLAGS INTERNALDATE ENVELOPE BODY.PEEK[TEXT]<0.{}>)",
                set_of(uids),
                PREVIEW
            ),
            Self::FetchBody(uid) => {
                format!("UID FETCH {uid} (UID FLAGS ENVELOPE BODY.PEEK[TEXT]<0.{FULL}>)")
            }
            Self::AddFlags { uids, flags } => {
                format!("UID STORE {} +FLAGS ({})", set_of(uids), flags.join(" "))
            }
            Self::RemoveFlags { uids, flags } => {
                format!("UID STORE {} -FLAGS ({})", set_of(uids), flags.join(" "))
            }
            Self::Move { uids, mailbox } => {
                format!("UID MOVE {} {}", set_of(uids), quoted(mailbox))
            }
            Self::Capability => "CAPABILITY".to_string(),
            Self::GmailLabels { uids, labels, add } => format!(
                "UID STORE {} {}X-GM-LABELS ({})",
                set_of(uids),
                if *add { '+' } else { '-' },
                // Quoted individually: a Gmail label may hold a space, and an
                // unquoted one would be read as two labels — which on the
                // adding path silently makes two new labels nobody wanted.
                labels
                    .iter()
                    .map(|label| quoted(label))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Self::Logout => "LOGOUT".to_string(),
        }
    }

    /// Whether this changes anything on the server. What `super`'s gate reads.
    pub fn mutates(&self) -> bool {
        matches!(
            self,
            Self::AddFlags { .. }
                | Self::RemoveFlags { .. }
                | Self::Move { .. }
                | Self::GmailLabels { .. }
        )
    }
}

/// How much of a body comes back in a listing. Enough to triage on — a subject
/// plus the first paragraph is what tells a receipt from a request.
pub const PREVIEW: usize = 700;

/// How much of a body comes back when the message itself was asked for.
pub const FULL: usize = 20_000;

/// A UID set, as IMAP writes one. Contiguous runs collapse, because a mailbox
/// sweep is otherwise a line of four hundred numbers.
fn set_of(uids: &[u32]) -> String {
    let mut sorted: Vec<u32> = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut parts: Vec<String> = Vec::new();
    let mut at = 0;
    while at < sorted.len() {
        let start = sorted[at];
        let mut end = start;
        while at + 1 < sorted.len() && sorted[at + 1] == end + 1 {
            at += 1;
            end = sorted[at];
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}:{end}")
        });
        at += 1;
    }
    parts.join(",")
}

/// An IMAP quoted string. Backslash and quote are the only escapes there are.
fn quoted(text: &str) -> String {
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// The server refused: bad mailbox, bad credentials, over quota.
    No,
    /// The client got the protocol wrong, which here means this module did.
    Bad,
}

/// A complete response: everything untagged, and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: Status,
    /// The text after the status word on the tagged line.
    pub detail: String,
    /// Every untagged line, literals already spliced in.
    pub lines: Vec<String>,
}

impl Response {
    pub fn is_ok(&self) -> bool {
        self.status == Status::Ok
    }
}

/// Accumulates bytes until a whole tagged response has arrived.
///
/// The reason this is not `split('\n')`: a literal announces a byte count and
/// then sends bytes, which may contain anything. Counting them is the only way
/// to know where the response resumes — and a client that guesses will one day
/// read a message body as though it were protocol.
#[derive(Debug, Default)]
pub struct Reader {
    buffer: Vec<u8>,
    lines: Vec<String>,
    /// Untagged lines completed but not yet claimed.
    ready: VecDeque<Response>,
}

impl Reader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed what arrived, and take any responses that are now complete.
    pub fn push(&mut self, bytes: &[u8], tag: &str) -> Vec<Response> {
        self.buffer.extend_from_slice(bytes);
        self.drain(tag);
        self.ready.drain(..).collect()
    }

    fn drain(&mut self, tag: &str) {
        loop {
            let Some(end) = find_crlf(&self.buffer) else {
                return;
            };
            let line = String::from_utf8_lossy(&self.buffer[..end]).to_string();

            // A line ending in `{n}` promises n more bytes that are not
            // protocol. If they have not all arrived, wait.
            if let Some(count) = literal_length(&line) {
                let body_at = end + 2;
                if self.buffer.len() < body_at + count {
                    return;
                }
                let body =
                    String::from_utf8_lossy(&self.buffer[body_at..body_at + count]).to_string();
                self.buffer.drain(..body_at + count);
                // The literal belongs to the line that announced it.
                self.lines.push(format!("{line}\r\n{body}"));
                continue;
            }

            self.buffer.drain(..end + 2);
            if let Some(rest) = line.strip_prefix(&format!("{tag} ")) {
                let (status, detail) = split_status(rest);
                self.ready.push_back(Response {
                    status,
                    detail,
                    lines: std::mem::take(&mut self.lines),
                });
                continue;
            }
            self.lines.push(line);
        }
    }
}

fn find_crlf(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|pair| pair == b"\r\n")
}

/// The byte count a line's trailing `{n}` announces, if it has one.
///
/// `{n+}` is a non-synchronising literal, which servers send unprompted; the
/// count is read the same way.
fn literal_length(line: &str) -> Option<usize> {
    let open = line.rfind('{')?;
    if !line.ends_with('}') {
        return None;
    }
    let inner = &line[open + 1..line.len() - 1];
    inner.trim_end_matches('+').parse().ok()
}

fn split_status(rest: &str) -> (Status, String) {
    let (word, detail) = rest.split_once(' ').unwrap_or((rest, ""));
    let status = match word.to_uppercase().as_str() {
        "OK" => Status::Ok,
        "NO" => Status::No,
        _ => Status::Bad,
    };
    (status, detail.trim().to_string())
}

/// One message, as much as a listing needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Message {
    pub uid: u32,
    pub date: String,
    pub from: String,
    pub subject: String,
    pub flags: Vec<String>,
    /// The start of the body, already unfolded.
    pub preview: String,
}

impl Message {
    pub fn is_unread(&self) -> bool {
        !self
            .flags
            .iter()
            .any(|flag| flag.eq_ignore_ascii_case("\\Seen"))
    }
}

/// Pull the messages out of a `UID FETCH` response.
///
/// Tolerant on purpose. Servers differ in field order, in whether they quote
/// or send literals, and in what they include unasked; a parser that demanded
/// one shape would work against one server. What is extracted is what is
/// needed, wherever it turns up.
pub fn messages(response: &Response) -> Vec<Message> {
    response
        .lines
        .iter()
        .filter(|line| line.contains("FETCH"))
        .filter_map(|line| message(line))
        .collect()
}

fn message(line: &str) -> Option<Message> {
    let uid = after(line, "UID ")
        .and_then(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()
                .map(str::to_string)
        })
        .and_then(|digits| digits.parse().ok())?;

    let envelope = between(line, "ENVELOPE (", ")").unwrap_or_default();
    let fields = top_level(&envelope);

    Some(Message {
        uid,
        date: fields.first().cloned().unwrap_or_default(),
        subject: decode_header(&fields.get(1).cloned().unwrap_or_default()),
        from: decode_header(&address(
            fields.get(2).map(String::as_str).unwrap_or_default(),
        )),
        flags: flags_in(line),
        preview: squash(&body_of(line)),
    })
}

/// The flags on a FETCH line.
fn flags_in(line: &str) -> Vec<String> {
    between(line, "FLAGS (", ")")
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// The body text, which arrives as the literal after `BODY[TEXT]`.
fn body_of(line: &str) -> String {
    let Some(at) = line.find("BODY[TEXT]") else {
        return String::new();
    };
    match line[at..].split_once("\r\n") {
        Some((_, body)) => body.to_string(),
        None => String::new(),
    }
}

/// The first address in an IMAP address list, as `Name <local@host>`.
///
/// The shape is `((name adl mailbox host) ...)`, and every part may be NIL.
fn address(list: &str) -> String {
    // `((name adl mailbox host) …)` — two levels, so the first `between` gets
    // the inner group *including* its own parentheses and they come off here.
    let inner = between(list, "(", ")").unwrap_or_default();
    let inner = inner.trim();
    let inner = inner
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or(inner);
    let parts = top_level(inner);
    let name = parts.first().cloned().unwrap_or_default();
    let mailbox = parts.get(2).cloned().unwrap_or_default();
    let host = parts.get(3).cloned().unwrap_or_default();

    let address = match (mailbox.is_empty(), host.is_empty()) {
        (false, false) => format!("{mailbox}@{host}"),
        _ => String::new(),
    };
    match (name.is_empty(), address.is_empty()) {
        (false, false) => format!("{name} <{address}>"),
        (true, false) => address,
        (false, true) => name,
        (true, true) => "unknown".to_string(),
    }
}

/// Split a parenthesised list into its top-level items, respecting quotes,
/// nesting and literals. `NIL` becomes empty.
fn top_level(text: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    let mut current = String::new();

    for character in text.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            // Quotes are dropped from a value at the top level and *kept*
            // inside a nested list, because that list is re-parsed later and
            // an address whose name is "Ada Prins" would otherwise come back
            // as two fields.
            '"' => {
                quoted = !quoted;
                if depth > 0 {
                    current.push(character);
                }
            }
            '(' if !quoted => {
                depth += 1;
                current.push(character);
            }
            ')' if !quoted => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            ' ' if !quoted && depth == 0 => {
                if !current.is_empty() {
                    items.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(character),
        }
    }
    if !current.is_empty() {
        items.push(current);
    }
    items
        .into_iter()
        .map(|item| if item == "NIL" { String::new() } else { item })
        .collect()
}

/// Mailbox names out of a `LIST` response.
///
/// The line is `* LIST (\HasNoChildren) "/" "INBOX"`, and the name is the last
/// item — quoted, or a literal for anything with an accent in it.
pub fn mailboxes(response: &Response) -> Vec<String> {
    response
        .lines
        .iter()
        .filter(|line| line.starts_with("* LIST"))
        .filter_map(|line| {
            if let Some((head, body)) = line.split_once("\r\n") {
                // A literal name: the count was on the first line, the name is
                // what followed it.
                let _ = head;
                return Some(body.trim().to_string());
            }
            let items = top_level(line);
            items.last().cloned()
        })
        .filter(|name| !name.is_empty())
        .collect()
}

/// The UIDs a `UID SEARCH` found, newest first.
///
/// Newest first because that is the order anybody wants mail in, and because a
/// truncated list should keep the *recent* end.
pub fn found(response: &Response) -> Vec<u32> {
    let mut uids: Vec<u32> = response
        .lines
        .iter()
        .filter_map(|line| line.strip_prefix("* SEARCH"))
        .flat_map(|rest| rest.split_whitespace())
        .filter_map(|word| word.parse().ok())
        .collect();
    uids.sort_unstable_by(|a, b| b.cmp(a));
    uids
}

/// Decode the RFC 2047 encoded words a subject line is full of.
///
/// `=?UTF-8?B?…?=` and `=?UTF-8?Q?…?=`. Not decoding them leaves the model
/// reading `=?utf-8?B?UmU6IEludm9pY2U=?=` and trying to triage on it.
pub fn decode_header(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("=?") {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find("?=").map(|at| start + at + 2) else {
            break;
        };
        let word = &rest[start..end];
        match decode_word(word) {
            Some(decoded) => out.push_str(&decoded),
            None => out.push_str(word),
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn decode_word(word: &str) -> Option<String> {
    let inner = word.strip_prefix("=?")?.strip_suffix("?=")?;
    let mut parts = inner.splitn(3, '?');
    let _charset = parts.next()?;
    let encoding = parts.next()?;
    let payload = parts.next()?;

    let bytes = match encoding.to_uppercase().as_str() {
        "B" => base64_decode(payload)?,
        "Q" => quoted_printable(payload, true),
        _ => return None,
    };
    Some(String::from_utf8_lossy(&bytes).to_string())
}

/// Quoted-printable, with the `_`-is-a-space rule that applies in headers.
fn quoted_printable(text: &str, header: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'=' if at + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[at + 1..at + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        at += 3;
                    }
                    Err(_) => {
                        out.push(bytes[at]);
                        at += 1;
                    }
                }
            }
            b'_' if header => {
                out.push(b' ');
                at += 1;
            }
            byte => {
                out.push(byte);
                at += 1;
            }
        }
    }
    out
}

/// Base64, by hand. One small table beats a dependency for the one place this
/// is needed.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut accumulator: u32 = 0;
    let mut bits = 0u32;
    for character in text.chars().filter(|c| !c.is_whitespace()) {
        if character == '=' {
            break;
        }
        let value = match character {
            'A'..='Z' => character as u32 - 'A' as u32,
            'a'..='z' => character as u32 - 'a' as u32 + 26,
            '0'..='9' => character as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            _ => return None,
        };
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    Some(out)
}

/// Base64, the other way, for SMTP authentication.
pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let block = ((chunk[0] as u32) << 16)
            | ((*chunk.get(1).unwrap_or(&0) as u32) << 8)
            | (*chunk.get(2).unwrap_or(&0) as u32);
        out.push(TABLE[(block >> 18) as usize & 63] as char);
        out.push(TABLE[(block >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(block >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[block as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// A body as one readable block: quoted-printable decoded, wrapping undone,
/// and the runs of blank lines closed up.
fn squash(text: &str) -> String {
    let decoded = if text.contains("=\r\n") || text.contains("=3D") {
        String::from_utf8_lossy(&quoted_printable(&text.replace("=\r\n", ""), false)).to_string()
    } else {
        text.to_string()
    };
    let mut out = String::new();
    let mut blank = 0;
    for line in decoded.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

fn after<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
    text.find(marker).map(|at| &text[at + marker.len()..])
}

/// The text between the first `open` and its matching `close`, counting nesting
/// so an envelope's inner lists do not end it early.
fn between(text: &str, open: &str, close: &str) -> Option<String> {
    let start = text.find(open)? + open.len();
    let rest = &text[start..];
    if open.ends_with('(') {
        let mut depth = 1usize;
        let mut quoted = false;
        let mut escaped = false;
        for (at, character) in rest.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match character {
                '\\' if quoted => escaped = true,
                '"' => quoted = !quoted,
                '(' if !quoted => depth += 1,
                ')' if !quoted => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(rest[..at].to_string());
                    }
                }
                _ => {}
            }
        }
        return None;
    }
    rest.find(close).map(|at| rest[..at].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(bytes: &str) -> Vec<Response> {
        Reader::new().push(bytes.as_bytes(), "a1")
    }

    #[test]
    fn a_uid_set_collapses_runs_and_stays_sorted() {
        assert_eq!(set_of(&[3, 1, 2, 9, 7, 8]), "1:3,7:9");
        assert_eq!(set_of(&[5]), "5");
        assert_eq!(set_of(&[2, 2, 4]), "2,4");
    }

    #[test]
    fn reading_a_message_never_marks_it_read() {
        // `BODY` sets \Seen; `BODY.PEEK` does not. Deciding on the user's
        // behalf that they have read something is the kind of silent side
        // effect this whole capability has to avoid.
        let listing = Command::Fetch(vec![1, 2]).line();
        assert!(listing.contains("BODY.PEEK"), "{listing}");
        assert!(!listing.contains("BODY[TEXT]"), "{listing}");
        assert!(Command::FetchBody(7).line().contains("BODY.PEEK"));
    }

    #[test]
    fn every_command_addresses_messages_by_uid() {
        // A sequence number names a different message after anything is
        // deleted. Getting this wrong archives the wrong mail.
        for command in [
            Command::Fetch(vec![1]),
            Command::FetchBody(1),
            Command::Search("UNSEEN".into()),
            Command::AddFlags {
                uids: vec![1],
                flags: vec!["\\Seen".into()],
            },
            Command::Move {
                uids: vec![1],
                mailbox: "Archive".into(),
            },
        ] {
            assert!(
                command.line().starts_with("UID "),
                "not a UID command: {}",
                command.line()
            );
        }
    }

    #[test]
    fn only_the_commands_that_change_something_say_they_do() {
        assert!(!Command::List.mutates());
        assert!(!Command::Fetch(vec![1]).mutates());
        assert!(!Command::Search("ALL".into()).mutates());
        assert!(Command::AddFlags {
            uids: vec![1],
            flags: vec!["\\Deleted".into()]
        }
        .mutates());
        assert!(Command::Move {
            uids: vec![1],
            mailbox: "Trash".into()
        }
        .mutates());
    }

    #[test]
    fn a_mailbox_name_with_a_quote_in_it_cannot_break_out_of_the_command() {
        let line = Command::Select("Weird\" OR DELETE".into()).line();
        assert_eq!(line, r#"SELECT "Weird\" OR DELETE""#);
    }

    #[test]
    fn a_tagged_response_completes_and_carries_its_untagged_lines() {
        let responses = read("* 1 EXISTS\r\n* FLAGS (\\Seen)\r\na1 OK SELECT completed\r\n");
        assert_eq!(responses.len(), 1);
        assert!(responses[0].is_ok());
        assert_eq!(responses[0].detail, "SELECT completed");
        assert_eq!(responses[0].lines.len(), 2);
    }

    #[test]
    fn a_refusal_is_a_status_and_not_a_parse_failure() {
        let responses = read("a1 NO [AUTHENTICATIONFAILED] Invalid credentials\r\n");
        assert_eq!(responses[0].status, Status::No);
        assert!(responses[0].detail.contains("Invalid credentials"));
    }

    #[test]
    fn a_literal_is_counted_rather_than_split_on() {
        // The whole reason this is a state machine. The body below contains a
        // CRLF and a line that looks exactly like a tagged response; a reader
        // that split on newlines would end the response in the middle of it.
        let body = "Hello\r\na1 OK not really the end\r\nBye";
        let wire = format!(
            "* 1 FETCH (UID 9 BODY[TEXT] {{{}}}\r\n{body})\r\na1 OK FETCH completed\r\n",
            body.len()
        );
        let responses = read(&wire);
        assert_eq!(responses.len(), 1, "the literal ended the response early");
        assert!(responses[0].is_ok());
        assert!(responses[0].lines[0].contains("not really the end"));
    }

    #[test]
    fn a_response_split_across_reads_is_assembled() {
        let mut reader = Reader::new();
        assert!(reader.push(b"* 1 EXI", "a1").is_empty());
        assert!(reader.push(b"STS\r\na1 O", "a1").is_empty());
        let responses = reader.push(b"K done\r\n", "a1");
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].lines, ["* 1 EXISTS"]);
    }

    #[test]
    fn an_envelope_becomes_a_message_a_person_could_read() {
        let wire = "* 5 FETCH (UID 42 FLAGS (\\Seen) ENVELOPE (\"Tue, 4 Aug 2026 09:14:00 +0100\" \
                    \"Invoice 8871 is overdue\" ((\"Ada Prins\" NIL \"billing\" \"prins.example\")) \
                    NIL NIL NIL NIL NIL NIL NIL))\r\na1 OK done\r\n";
        let found = messages(&read(wire).remove(0));
        assert_eq!(found.len(), 1);
        let message = &found[0];
        assert_eq!(message.uid, 42);
        assert_eq!(message.subject, "Invoice 8871 is overdue");
        assert_eq!(message.from, "Ada Prins <billing@prins.example>");
        assert_eq!(message.date, "Tue, 4 Aug 2026 09:14:00 +0100");
        assert!(!message.is_unread(), "\\Seen was not read as read");
    }

    #[test]
    fn a_message_with_no_name_on_the_address_still_reads() {
        let wire = "* 1 FETCH (UID 3 FLAGS () ENVELOPE (\"now\" \"Re: roof\" \
                    ((NIL NIL \"vandenberg\" \"roofing.example\")) NIL NIL NIL NIL NIL NIL NIL))\r\n\
                    a1 OK done\r\n";
        let found = messages(&read(wire).remove(0));
        assert_eq!(found[0].from, "vandenberg@roofing.example");
        assert!(found[0].is_unread());
    }

    #[test]
    fn an_encoded_subject_is_decoded_rather_than_shown_as_gibberish() {
        // Otherwise the model triages on "=?utf-8?B?UmU6IEludm9pY2U=?=".
        assert_eq!(decode_header("=?utf-8?B?UmU6IEludm9pY2U=?="), "Re: Invoice");
        assert_eq!(decode_header("=?UTF-8?Q?Re=3A_faktura?="), "Re: faktura");
        assert_eq!(decode_header("plain subject"), "plain subject");
        // A mixture, which is how a reply to an encoded subject arrives.
        assert_eq!(
            decode_header("Re: =?utf-8?B?cm9vZg==?= today"),
            "Re: roof today"
        );
    }

    #[test]
    fn base64_round_trips() {
        for text in ["", "a", "ab", "abc", "hello there"] {
            let encoded = base64_encode(text.as_bytes());
            assert_eq!(
                base64_decode(&encoded).map(|b| String::from_utf8_lossy(&b).to_string()),
                Some(text.to_string()),
                "{text:?} did not survive {encoded}"
            );
        }
    }

    #[test]
    fn search_results_come_back_newest_first() {
        let responses = read("* SEARCH 3 1 9 7\r\na1 OK done\r\n");
        assert_eq!(found(&responses[0]), [9, 7, 3, 1]);
        // A search with no hits is an empty list, not a failure.
        let empty = read("* SEARCH\r\na1 OK done\r\n");
        assert!(found(&empty[0]).is_empty());
    }

    #[test]
    fn mailboxes_are_read_out_of_a_list_response() {
        let wire = "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
                    * LIST (\\HasNoChildren) \"/\" \"Archive\"\r\n\
                    * LIST (\\Noselect) \"/\" \"[Gmail]\"\r\n\
                    a1 OK done\r\n";
        assert_eq!(
            mailboxes(&read(wire).remove(0)),
            ["INBOX", "Archive", "[Gmail]"]
        );
    }

    #[test]
    fn a_wrapped_quoted_printable_body_reads_as_prose() {
        let wire = format!(
            "* 1 FETCH (UID 1 BODY[TEXT] {{{}}}\r\n{})\r\na1 OK done\r\n",
            "Your invoice is =\r\noverdue by =C2=A3120.\r\n\r\n\r\nThanks".len(),
            "Your invoice is =\r\noverdue by =C2=A3120.\r\n\r\n\r\nThanks"
        );
        let found = messages(&read(&wire).remove(0));
        assert!(
            found[0].preview.contains("overdue by £120"),
            "{:?}",
            found[0].preview
        );
        // Runs of blank lines close up rather than filling the context.
        assert!(!found[0].preview.contains("\n\n\n"));
    }
}
