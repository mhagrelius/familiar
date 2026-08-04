//! Gmail is not quite IMAP, and pretending otherwise breaks quietly.
//!
//! Everything in `imap.rs` is the standard, and against a standard server it is
//! all that is needed. Gmail answers the same commands and means different
//! things by them, in three ways that matter here:
//!
//! * **There are no folders, only labels.** What IMAP calls a mailbox, Gmail
//!   shows as a label, and its special ones live under a `[Gmail]/` prefix —
//!   `[Gmail]/Trash`, `[Gmail]/All Mail`, `[Gmail]/Sent Mail`. A `MOVE` to
//!   `Trash` fails on Gmail with "no such mailbox", which reads to the model
//!   like the message was not there.
//! * **A message has many labels at once.** `UID STORE +X-GM-LABELS` adds one
//!   without removing anything, which is what "label this" means to somebody
//!   using Gmail. Standard IMAP keywords do exist there, and are not the same
//!   thing: they do not appear as labels in the web interface, so the user
//!   would file a morning's post and see nothing happen.
//! * **Search is Gmail's own language.** `X-GM-RAW` takes the query syntax from
//!   the search box — `from:ada is:unread has:attachment older_than:7d` — which
//!   is also, conveniently, the syntax a language model has read a million
//!   examples of. Against Gmail the model's own words go almost straight
//!   through, where IMAP `SEARCH` needs them translated and loses most of them
//!   on the way.
//!
//! Archiving is the case that shows why this cannot be one path with a few
//! substitutions. On a standard server, archive means move the message to a
//! folder called Archive. On Gmail it means *remove the Inbox label* and leave
//! everything else alone — the message stays in All Mail, because on Gmail it
//! never left.
//!
//! The dialect is chosen from the server's own `CAPABILITY` reply where there
//! is one, and guessed from the hostname before the first response arrives.

/// Which server this is, as far as anything here needs to care.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dialect {
    /// RFC 3501 and nothing assumed beyond it.
    #[default]
    Standard,
    /// Gmail, or anything else answering `X-GM-EXT-1`.
    Gmail,
}

impl Dialect {
    /// What a hostname suggests, before a server has said anything.
    ///
    /// A guess, and only used until [`Self::from_capability`] can replace it
    /// with the server's own answer. Google Workspace accounts on a custom
    /// domain still connect to `imap.gmail.com`, so the host is a better signal
    /// than the address.
    pub fn of_host(host: &str) -> Self {
        let host = host.trim().to_lowercase();
        if host.ends_with("gmail.com") || host.ends_with("googlemail.com") {
            Self::Gmail
        } else {
            Self::Standard
        }
    }

    /// What the server said it can do. `X-GM-EXT-1` is Gmail's marker for the
    /// three extensions this module uses, and it is advertised by Google
    /// Workspace domains that this host check would miss.
    pub fn from_capability(capability: &str) -> Option<Self> {
        capability
            .to_uppercase()
            .contains("X-GM-EXT-1")
            .then_some(Self::Gmail)
    }

    /// Where a deleted message goes.
    pub fn trash(self) -> &'static str {
        match self {
            Self::Standard => "Trash",
            Self::Gmail => "[Gmail]/Trash",
        }
    }

    /// Where an archived message goes, when archiving is a move at all.
    ///
    /// `None` on Gmail, where it is not: archiving there removes the Inbox
    /// label and moves nothing.
    pub fn archive(self) -> Option<&'static str> {
        match self {
            Self::Standard => Some("Archive"),
            Self::Gmail => None,
        }
    }

    /// Whether a search may be handed to the server in the user's own words.
    pub fn speaks_raw_search(self) -> bool {
        self == Self::Gmail
    }

    /// A mailbox name as this server spells it.
    ///
    /// The model writes "Trash", "All Mail", "Sent" — the names a person uses —
    /// and on Gmail those are three of the handful that carry a prefix. Any
    /// other name is a label the user made and is left exactly as written,
    /// because it is theirs.
    pub fn mailbox(self, named: &str) -> String {
        let named = named.trim().trim_matches(['"', '\'']);
        if self == Self::Standard || named.starts_with("[Gmail]") {
            return named.to_string();
        }
        match named.to_lowercase().as_str() {
            "trash" | "bin" | "deleted" | "deleted items" => "[Gmail]/Trash",
            "all mail" | "all" | "archive" | "allmail" => "[Gmail]/All Mail",
            "sent" | "sent mail" | "sent items" => "[Gmail]/Sent Mail",
            "drafts" | "draft" => "[Gmail]/Drafts",
            "spam" | "junk" => "[Gmail]/Spam",
            "starred" => "[Gmail]/Starred",
            "important" => "[Gmail]/Important",
            // INBOX is INBOX on both, and is the one name IMAP reserves.
            "inbox" => "INBOX",
            _ => return named.to_string(),
        }
        .to_string()
    }

    /// The search command's argument, given what the model wrote.
    ///
    /// On Gmail the query goes over almost unchanged, inside one quoted string,
    /// so `has:attachment` and `older_than:7d` — which IMAP has no way to
    /// express — simply work. Everywhere else it is translated by
    /// [`super::criteria`], which is the lossy path and the only one available.
    pub fn search(self, query: &str) -> String {
        if !self.speaks_raw_search() {
            return super::criteria(query);
        }
        let query = query.trim();
        if query.is_empty() {
            return "ALL".to_string();
        }
        // Gmail's `is:` prefix is what its search box takes; the model tends to
        // write the bare word, having been taught it by this application's own
        // guidance. Both are accepted here rather than in the prompt.
        let spelled: Vec<String> = query
            .split_whitespace()
            .map(|word| match word.to_lowercase().as_str() {
                "unread" | "unseen" => "is:unread".to_string(),
                "read" | "seen" => "is:read".to_string(),
                "flagged" => "is:starred".to_string(),
                _ => word.to_string(),
            })
            .collect();
        format!("X-GM-RAW {}", quoted(&spelled.join(" ")))
    }
}

/// An IMAP quoted string. The same escaping [`super::quoted`] does, which is
/// what keeps a search term from becoming syntax — and it matters more here,
/// because on Gmail the user's words reach the wire almost as written.
fn quoted(text: &str) -> String {
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Labelling, as this server does it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Labelling {
    /// `UID STORE … +FLAGS (Keyword)` — standard IMAP keywords.
    Keywords(Vec<String>),
    /// `UID STORE … +X-GM-LABELS ("Name")` — a real Gmail label, which is what
    /// the user will actually see.
    GmailLabels(Vec<String>),
}

impl Dialect {
    pub fn labelling(self, names: &[String]) -> Labelling {
        match self {
            // Keywords cannot hold a space, which is why the standard path
            // replaces them with underscores. Gmail labels can, and mangling
            // "Needs Reply" into "Needs_Reply" would make a second label beside
            // the one the user already has.
            Self::Standard => Labelling::Keywords(
                names
                    .iter()
                    .map(|name| name.replace(' ', "_"))
                    .collect::<Vec<_>>(),
            ),
            Self::Gmail => Labelling::GmailLabels(names.to_vec()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gmail_is_recognised_by_its_host_and_by_what_it_says_about_itself() {
        assert_eq!(Dialect::of_host("imap.gmail.com"), Dialect::Gmail);
        assert_eq!(Dialect::of_host("IMAP.GoogleMail.com"), Dialect::Gmail);
        assert_eq!(Dialect::of_host("mail.fastmail.com"), Dialect::Standard);
        // A Workspace domain the host check would miss, caught by the server's
        // own answer.
        assert_eq!(
            Dialect::from_capability("* CAPABILITY IMAP4rev1 X-GM-EXT-1 UIDPLUS"),
            Some(Dialect::Gmail)
        );
        assert_eq!(
            Dialect::from_capability("* CAPABILITY IMAP4rev1 UIDPLUS MOVE"),
            None
        );
    }

    #[test]
    fn the_trash_is_where_this_server_keeps_it() {
        // The whole reason this module exists: `MOVE 42 Trash` against Gmail
        // fails with "no such mailbox", and a delete that quietly did nothing
        // is worse than one that refused.
        assert_eq!(Dialect::Standard.trash(), "Trash");
        assert_eq!(Dialect::Gmail.trash(), "[Gmail]/Trash");
    }

    #[test]
    fn archiving_on_gmail_is_not_a_move() {
        assert_eq!(Dialect::Standard.archive(), Some("Archive"));
        assert_eq!(Dialect::Gmail.archive(), None);
    }

    #[test]
    fn the_names_a_person_says_become_the_names_gmail_uses() {
        let gmail = Dialect::Gmail;
        assert_eq!(gmail.mailbox("Trash"), "[Gmail]/Trash");
        assert_eq!(gmail.mailbox("sent"), "[Gmail]/Sent Mail");
        assert_eq!(gmail.mailbox("All Mail"), "[Gmail]/All Mail");
        assert_eq!(gmail.mailbox("Spam"), "[Gmail]/Spam");
        assert_eq!(gmail.mailbox("INBOX"), "INBOX");
        // Already spelled out, and not spelled twice.
        assert_eq!(gmail.mailbox("[Gmail]/Trash"), "[Gmail]/Trash");
        // A label the user made is theirs, and is left alone.
        assert_eq!(gmail.mailbox("Roof 2026"), "Roof 2026");
        // A standard server is left entirely alone.
        assert_eq!(Dialect::Standard.mailbox("Trash"), "Trash");
        assert_eq!(Dialect::Standard.mailbox("Archive"), "Archive");
    }

    #[test]
    fn a_gmail_search_goes_over_in_the_words_the_model_wrote() {
        let built = Dialect::Gmail.search("from:ada unread has:attachment");
        assert_eq!(built, "X-GM-RAW \"from:ada is:unread has:attachment\"");
        // Things IMAP cannot express at all, which is the point.
        assert!(Dialect::Gmail
            .search("older_than:7d larger:5M")
            .contains("older_than:7d"));
        assert_eq!(Dialect::Gmail.search("   "), "ALL");
    }

    #[test]
    fn a_gmail_search_term_still_cannot_become_a_command() {
        // The user's words reach the wire nearly as written here, so the
        // quoting is doing more work than on the standard path, not less.
        let built = Dialect::Gmail.search(r#"from:ada" UID STORE 1 +FLAGS (\Deleted"#);
        let quotes = built.matches('"').count() - built.matches("\\\"").count();
        assert_eq!(quotes % 2, 0, "a value closed its quote early: {built}");
        assert!(built.starts_with("X-GM-RAW \""), "{built}");
        assert!(built.ends_with('"'), "{built}");
    }

    #[test]
    fn a_standard_search_is_translated_as_it_always_was() {
        assert_eq!(
            Dialect::Standard.search("from:ada unread"),
            "FROM \"ada\" UNSEEN"
        );
    }

    #[test]
    fn labelling_gmail_makes_a_label_and_labelling_imap_makes_a_keyword() {
        let names = vec!["Needs Reply".to_string()];
        // A keyword cannot hold a space; a Gmail label can, and squashing it
        // would make a second label beside the one the user already has.
        assert_eq!(
            Dialect::Standard.labelling(&names),
            Labelling::Keywords(vec!["Needs_Reply".into()])
        );
        assert_eq!(
            Dialect::Gmail.labelling(&names),
            Labelling::GmailLabels(vec!["Needs Reply".into()])
        );
    }
}
