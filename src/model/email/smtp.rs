//! Sending: the message to put on the wire, and the conversation that puts it
//! there.
//!
//! Pure, like [`super::imap`]. The socket belongs to `ui::mail`.
//!
//! **Header injection is the whole risk here.** A subject is a single header
//! line, and a subject containing a newline followed by `Bcc: everyone@…` is
//! two headers — so an assistant that was asked to send one email sends a
//! different one. The model composes these from text it may have read *in
//! another email*, which is exactly the untrusted path. Every header value goes
//! through [`header_safe`], and the test that proves it is the one to keep.

/// A message about to be sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outgoing {
    pub from: String,
    pub to: Vec<String>,
    pub subject: String,
    pub body: String,
}

/// Why a message was not sent. None of these reach the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NoRecipient,
    NoBody,
    BadAddress(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRecipient => f.write_str("there is nobody to send it to — pass `to=`"),
            Self::NoBody => f.write_str("there is nothing to send — pass `body=`"),
            Self::BadAddress(what) => {
                write!(f, "{what:?} is not an email address")
            }
        }
    }
}

impl Outgoing {
    pub fn check(&self) -> Result<(), Refusal> {
        if self.to.is_empty() {
            return Err(Refusal::NoRecipient);
        }
        if self.body.trim().is_empty() {
            return Err(Refusal::NoBody);
        }
        for address in self.to.iter().chain(std::iter::once(&self.from)) {
            if !plausible(address) {
                return Err(Refusal::BadAddress(address.clone()));
            }
        }
        Ok(())
    }

    /// The RFC 5322 message, headers and all.
    ///
    /// Every header value is stripped of anything that could start a new one.
    /// The body is not — a body may contain whatever it likes — but it *is*
    /// dot-stuffed by [`data`], because a line consisting of one dot ends the
    /// message.
    pub fn render(&self, date: &str, message_id: &str) -> String {
        format!(
            "From: {}\r\nTo: {}\r\nSubject: {}\r\nDate: {}\r\nMessage-ID: {}\r\n\
             MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{}",
            header_safe(&self.from),
            header_safe(&self.to.join(", ")),
            header_safe(&self.subject),
            header_safe(date),
            header_safe(message_id),
            self.body.replace("\r\n", "\n").replace('\n', "\r\n"),
        )
    }
}

/// A header value with everything that could forge a header taken out.
///
/// Newlines are the attack; the rest are control characters that have no
/// business in a header and that some servers treat as line breaks anyway.
pub fn header_safe(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

/// Enough of an address to be worth trying. Not a validator — nothing short of
/// sending is — but it catches the model passing a name, a sentence, or a list
/// it forgot to split.
pub fn plausible(address: &str) -> bool {
    let address = address.trim();
    let Some((local, domain)) = address.rsplit_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !address.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// The message body as SMTP wants it: CRLF endings, dot-stuffed, terminated.
///
/// A line that is exactly `.` ends the DATA command, so a message containing
/// one would be truncated there and the rest interpreted as commands. Doubling
/// the leading dot is the fix the protocol specifies.
pub fn data(message: &str) -> String {
    let mut out = String::with_capacity(message.len() + 8);
    for line in message.split("\r\n") {
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str(".\r\n");
    out
}

/// The commands, in order, for one message.
///
/// `AUTH PLAIN` carries the credentials base64-encoded, which is not encryption
/// — the connection is TLS and that is what protects them.
pub fn conversation(message: &Outgoing, user: &str, password: &str, host: &str) -> Vec<String> {
    vec![
        format!("EHLO {}", header_safe(host)),
        format!(
            "AUTH PLAIN {}",
            super::imap::base64_encode(format!("\0{user}\0{password}").as_bytes())
        ),
        format!("MAIL FROM:<{}>", header_safe(&message.from)),
    ]
    .into_iter()
    .chain(
        message
            .to
            .iter()
            .map(|to| format!("RCPT TO:<{}>", header_safe(to))),
    )
    .chain(std::iter::once("DATA".to_string()))
    .collect()
}

/// Whether an SMTP reply line is a success. `2xx` and `3xx` are; the rest are
/// the server saying no, and the text after the code is why.
pub fn accepted(line: &str) -> bool {
    matches!(line.trim().chars().next(), Some('2') | Some('3'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message() -> Outgoing {
        Outgoing {
            from: "me@post.example".into(),
            to: vec!["ada@prins.example".into()],
            subject: "Roof quote".into(),
            body: "Could you send the quote through?\n\nThanks".into(),
        }
    }

    #[test]
    fn a_subject_cannot_forge_a_second_header() {
        // The whole risk in this file. The model composes a subject from text
        // it may have read in *another email*, and a newline in it turns one
        // message into a different one.
        let mut forged = message();
        forged.subject = "Invoice\r\nBcc: everyone@example.test\r\nX-Evil: yes".into();
        let rendered = forged.render("Mon, 3 Aug 2026 09:00:00 +0000", "<1@familiar>");

        // The words survive — they are now part of a (silly-looking) subject —
        // and that is fine. What must not happen is a *header line* starting
        // with them, which is the only thing that would change where the
        // message goes.
        let (headers, _) = rendered.split_once("\r\n\r\n").expect("a header block");
        for line in headers.split("\r\n") {
            assert!(
                !line.starts_with("Bcc:"),
                "a Bcc header was forged: {rendered}"
            );
            assert!(
                !line.starts_with("X-Evil"),
                "a header was forged: {rendered}"
            );
        }
        // Seven headers went in and seven came out. Counting them is the check
        // that actually catches a forgery; counting blank lines is not, because
        // a body with a paragraph break in it legitimately has one.
        assert_eq!(
            headers.split("\r\n").count(),
            7,
            "the header count changed, so something forged one: {rendered}"
        );
    }

    #[test]
    fn a_recipient_cannot_smuggle_anything_into_the_envelope() {
        let mut forged = message();
        forged.to = vec!["ada@prins.example\r\nRCPT TO:<mallory@example.test>".into()];
        let spoken = conversation(&forged, "u", "p", "localhost");
        assert_eq!(
            spoken
                .iter()
                .filter(|line| line.starts_with("RCPT"))
                .count(),
            1,
            "{spoken:?}"
        );
    }

    #[test]
    fn a_line_that_would_end_the_message_early_is_stuffed() {
        // A body line of exactly "." terminates DATA; everything after it
        // would be read as commands.
        let stuffed = data("Here is the list:\r\n.\r\nAnd the rest");
        assert!(stuffed.contains("\r\n..\r\n"), "{stuffed}");
        assert!(stuffed.ends_with("\r\n.\r\n"));
        assert!(stuffed.contains("And the rest"));
    }

    #[test]
    fn a_message_with_nobody_to_send_it_to_is_refused_before_anything_connects() {
        let mut empty = message();
        empty.to.clear();
        assert_eq!(empty.check(), Err(Refusal::NoRecipient));

        let mut silent = message();
        silent.body = "   ".into();
        assert_eq!(silent.check(), Err(Refusal::NoBody));

        assert!(message().check().is_ok());
    }

    #[test]
    fn something_that_is_not_an_address_is_caught_here_rather_than_by_the_server() {
        for bad in ["Ada Prins", "ada@localhost", "@prins.example", "a@b.", ""] {
            assert!(!plausible(bad), "{bad:?} was accepted");
        }
        for good in ["ada@prins.example", "a.b+c@sub.domain.co.uk"] {
            assert!(plausible(good), "{good:?} was refused");
        }

        let mut wrong = message();
        wrong.to = vec!["Ada Prins".into()];
        assert!(matches!(wrong.check(), Err(Refusal::BadAddress(_))));
    }

    #[test]
    fn the_conversation_authenticates_before_it_names_anybody() {
        let spoken = conversation(&message(), "user", "secret", "mail.example");
        assert!(spoken[0].starts_with("EHLO"));
        assert!(spoken[1].starts_with("AUTH PLAIN "));
        assert!(spoken[2].starts_with("MAIL FROM:<me@post.example>"));
        assert!(spoken.last().unwrap() == "DATA");
        // The credentials are encoded, not in the clear — TLS is what actually
        // protects them, but a password in a log line is still a password.
        assert!(!spoken[1].contains("secret"));
    }

    #[test]
    fn a_refusal_is_told_apart_from_an_acceptance() {
        assert!(accepted("250 OK"));
        assert!(accepted("354 Start mail input"));
        assert!(!accepted("535 Authentication failed"));
        assert!(!accepted("550 No such user"));
    }

    #[test]
    fn the_body_keeps_its_shape_and_the_headers_are_all_there() {
        let rendered = message().render("Mon, 3 Aug 2026 09:00:00 +0000", "<1@familiar>");
        for header in [
            "From:",
            "To:",
            "Subject:",
            "Date:",
            "Message-ID:",
            "MIME-Version:",
        ] {
            assert!(rendered.contains(header), "{header} is missing");
        }
        assert!(rendered.contains("Could you send the quote through?"));
        assert!(rendered.ends_with("Thanks"));
        // Every line ending is CRLF, which is what the protocol requires.
        assert!(!rendered.contains('\n') || !rendered.replace("\r\n", "").contains('\n'));
    }
}
