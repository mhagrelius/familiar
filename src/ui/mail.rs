//! The socket half of mail: IMAP and SMTP over a real connection.
//!
//! Everything about the *protocol* is in `model::email`, which is pure and
//! tested. This is the part that cannot be: connecting, TLS, and driving a
//! request/response conversation on the GLib main loop so the window keeps
//! running while a mailbox is being read.
//!
//! `gio` provides the TLS, which is why this needs no new dependency: a
//! `SocketClient` with `set_tls(true)` gives a verified connection with the
//! system's trust store behind it. The alternative was an IMAP crate and a
//! TLS crate, for a protocol whose useful subset is three hundred lines.
//!
//! **Plain connections are allowed and default to off.** Not laxity: a bridge
//! — Proton's, or an offline sync daemon — listens on localhost without TLS
//! because there is no network to protect, and refusing that would rule out a
//! real way people read mail. It is off unless somebody sets it, and the
//! preferences row says what it means.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::gio;
use gtk::prelude::*;

use crate::model::email::{self, imap, smtp};

/// Where the mail is, and who to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    /// Off only for a bridge on localhost.
    pub tls: bool,
    /// The address messages are sent from, and the SMTP server that sends them.
    pub from: String,
    pub smtp_host: String,
    pub smtp_port: u16,
}

impl Account {
    /// Whether there is enough here to try.
    pub fn is_configured(&self) -> bool {
        !self.host.trim().is_empty() && !self.user.trim().is_empty()
    }
}

/// How long to wait on a server that has stopped answering.
const TIMEOUT: u32 = 30;

/// One IMAP conversation, in progress.
struct Session {
    /// Held, not borrowed. The streams below belong to it, and dropping the
    /// connection closes them — which showed up as "Stream is already closed"
    /// on the very first command, because the callback that made the session
    /// let the connection go the moment it returned.
    _connection: gio::SocketConnection,
    input: gio::InputStream,
    output: gio::OutputStream,
    reader: RefCell<imap::Reader>,
    counter: Cell<usize>,
    /// Which server this turned out to be. Guessed from the hostname when the
    /// session is made and replaced by what `CAPABILITY` said, so a Workspace
    /// domain that does not look like Gmail is still treated as Gmail.
    dialect: Cell<email::Dialect>,
}

impl Session {
    fn tag(&self) -> String {
        self.counter.set(self.counter.get() + 1);
        format!("f{}", self.counter.get())
    }

    /// Send one command and read until its tagged response arrives.
    fn send<F>(self: &Rc<Self>, command: &imap::Command, done: F)
    where
        F: FnOnce(Result<imap::Response, String>) + 'static,
    {
        let tag = self.tag();
        let line = format!("{tag} {}\r\n", command.line());
        let session = self.clone();
        let bytes = glib::Bytes::from_owned(line.into_bytes());
        self.output.write_all_async(
            bytes,
            glib::Priority::DEFAULT,
            gio::Cancellable::NONE,
            move |written| {
                if let Err((_, error)) = &written {
                    done(Err(format!("the connection closed while sending: {error}")));
                    return;
                }
                collect(session, tag, Box::new(done));
            },
        );
    }
}

type Settle = Box<dyn FnOnce(Result<imap::Response, String>)>;

/// Read until the response tagged `tag` is complete.
fn collect(session: Rc<Session>, tag: String, done: Settle) {
    session.input.clone().read_bytes_async(
        8192,
        glib::Priority::DEFAULT,
        gio::Cancellable::NONE,
        move |read| {
            let Ok(bytes) = read else {
                done(Err("the connection closed while reading".into()));
                return;
            };
            if bytes.is_empty() {
                done(Err("the server closed the connection".into()));
                return;
            }
            let mut responses = session.reader.borrow_mut().push(&bytes, &tag);
            if let Some(response) = responses.pop() {
                done(Ok(response));
                return;
            }
            collect(session, tag, done);
        },
    );
}

/// Open a connection and log in.
fn connect<F>(account: &Account, done: F)
where
    F: FnOnce(Result<Rc<Session>, String>) + 'static,
{
    let client = gio::SocketClient::new();
    client.set_tls(account.tls);
    client.set_timeout(TIMEOUT);
    // The greeting is untagged, so the first read is drained before anything is
    // sent; `collect` with a tag nothing matches would never finish.
    let account = account.clone();
    let (host, port) = (account.host.clone(), account.port);
    client.connect_to_host_async(&host, port, gio::Cancellable::NONE, move |connected| {
        let connection = match connected {
            Ok(connection) => connection,
            Err(error) => {
                done(Err(format!(
                    "could not reach {}:{} — {error}",
                    account.host, account.port
                )));
                return;
            }
        };
        let session = Rc::new(Session {
            input: connection.input_stream(),
            output: connection.output_stream(),
            _connection: connection,
            reader: RefCell::new(imap::Reader::new()),
            counter: Cell::new(0),
            dialect: Cell::new(email::Dialect::of_host(&account.host)),
        });
        // Read the greeting and throw it away, then authenticate.
        session.input.clone().read_bytes_async(
            4096,
            glib::Priority::DEFAULT,
            gio::Cancellable::NONE,
            move |greeted| {
                if greeted.is_err() {
                    done(Err("the server sent no greeting".into()));
                    return;
                }
                let login = imap::Command::Login {
                    user: account.user.clone(),
                    password: account.password.clone(),
                };
                let ready = session.clone();
                session.send(&login, move |response| match response {
                    Ok(response) if response.is_ok() => ask_capability(ready, done),
                    // The detail is the server's own, and it is the thing
                    // the user needs: "Invalid credentials", "app password
                    // required", "too many connections".
                    Ok(response) => done(Err(format!(
                        "the mail server said no: {}{}",
                        response.detail,
                        app_password_hint(&account, &response.detail)
                    ))),
                    Err(error) => done(Err(error)),
                });
            },
        );
    });
}

use gtk::glib;

/// Ask what the server can do, and believe it over the hostname.
///
/// A failure here is not a failure to connect: the session is logged in and
/// works, and all that is lost is knowing which dialect it speaks — for which
/// the hostname guess is already in place. So the answer is read if it comes
/// and shrugged off if it does not.
fn ask_capability<F>(session: Rc<Session>, done: F)
where
    F: FnOnce(Result<Rc<Session>, String>) + 'static,
{
    let ready = session.clone();
    session.send(&imap::Command::Capability, move |response| {
        if let Ok(response) = &response {
            let said = format!("{}\n{}", response.lines.join("\n"), response.detail);
            if let Some(dialect) = email::Dialect::from_capability(&said) {
                ready.dialect.set(dialect);
            }
        }
        done(Ok(ready));
    });
}

/// The one login failure worth explaining rather than repeating.
///
/// Gmail has not accepted an account password over IMAP for years. What it
/// sends back is a bare "Invalid credentials", which is exactly what it sends
/// for a typo — so somebody who pasted the password they log in with has no way
/// to tell those apart, and the obvious next thing to try is the same password
/// again.
fn app_password_hint(account: &Account, detail: &str) -> String {
    if email::Dialect::of_host(&account.host) != email::Dialect::Gmail {
        return String::new();
    }
    let detail = detail.to_lowercase();
    if !detail.contains("credentials") && !detail.contains("authenticat") {
        return String::new();
    }
    ". Gmail does not accept the password you sign in with — it needs a 16-character app \
     password, from myaccount.google.com/apppasswords, and that page only appears once \
     2-Step Verification is on."
        .to_string()
}

/// Run one `mail` invocation and hand back what the model should read.
///
/// Every verb is a short scripted conversation, and each step's completion
/// starts the next — the same shape the weather chain uses, and for the same
/// reason: the main loop keeps running throughout.
pub fn run<F>(account: &Account, args: &[String], done: F)
where
    F: FnOnce(Result<String, String>) + 'static,
{
    if !account.is_configured() {
        done(Err(
            "no mail account is set up. Tell the user to add one in Preferences → Assistant → \
             Mail: a server, a username and a password."
                .into(),
        ));
        return;
    }
    let Some(verb) = email::verb(args).map(str::to_lowercase) else {
        done(Err("`mail` needs a verb".into()));
        return;
    };
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();

    if verb == "send" || verb == "reply" {
        send_message(account, &rest, done);
        return;
    }

    let account = account.clone();
    connect(&account, move |session| {
        let session = match session {
            Ok(session) => session,
            Err(error) => {
                done(Err(error));
                return;
            }
        };
        match verb.as_str() {
            "folders" | "mailboxes" => {
                session.clone().send(&imap::Command::List, move |response| {
                    done(match response {
                        Ok(response) if response.is_ok() => {
                            let named = imap::mailboxes(&response);
                            Ok(format!("{} folder(s):\n{}", named.len(), named.join("\n")))
                        }
                        Ok(response) => Err(response.detail),
                        Err(error) => Err(error),
                    })
                })
            }
            "search" | "list" => search(session, mailbox_of(&rest), rest.join(" "), done),
            "read" | "show" => match first_id(&rest) {
                Some(uid) => read_one(session, mailbox_of(&rest), uid, done),
                None => done(Err("`read` needs a message id, as `read 42`".into())),
            },
            other => file_them(session, other.to_string(), rest, done),
        }
    });
}

/// `in:Archive` anywhere in the arguments, or the inbox.
fn mailbox_of(args: &[String]) -> String {
    args.iter()
        .find_map(|word| word.strip_prefix("in:"))
        .map(|name| name.trim_matches(['"', '\'']).to_string())
        .unwrap_or_else(|| "INBOX".to_string())
}

fn first_id(args: &[String]) -> Option<u32> {
    args.iter().find_map(|word| word.parse().ok())
}

/// The ids a filing verb was given: every bare number, capped.
fn ids_in(args: &[String]) -> Vec<u32> {
    args.iter()
        .filter_map(|word| word.parse::<u32>().ok())
        .take(email::MAX_TOUCHED)
        .collect()
}

fn search<F>(session: Rc<Session>, mailbox: String, query: String, done: F)
where
    F: FnOnce(Result<String, String>) + 'static,
{
    let criteria = session.dialect.get().search(&strip_keys(&query));
    let mailbox = session.dialect.get().mailbox(&mailbox);
    let selected = session.clone();
    session.send(&imap::Command::Examine(mailbox.clone()), move |response| {
        match response {
            Ok(response) if !response.is_ok() => {
                done(Err(format!("{mailbox}: {}", response.detail)));
                return;
            }
            Err(error) => {
                done(Err(error));
                return;
            }
            _ => {}
        }
        let fetching = selected.clone();
        selected.send(&imap::Command::Search(criteria), move |response| {
            let uids = match response {
                Ok(response) if response.is_ok() => imap::found(&response),
                Ok(response) => {
                    done(Err(response.detail));
                    return;
                }
                Err(error) => {
                    done(Err(error));
                    return;
                }
            };
            if uids.is_empty() {
                done(Ok(email::listing(&mailbox, &[], 0)));
                return;
            }
            let total = uids.len();
            let wanted: Vec<u32> = uids.into_iter().take(email::MAX_RESULTS).collect();
            fetching.send(&imap::Command::Fetch(wanted), move |response| {
                done(match response {
                    Ok(response) if response.is_ok() => {
                        let mut found = imap::messages(&response);
                        found.sort_by_key(|message| std::cmp::Reverse(message.uid));
                        Ok(email::listing(&mailbox, &found, total))
                    }
                    Ok(response) => Err(response.detail),
                    Err(error) => Err(error),
                })
            });
        });
    });
}

/// `in:` is for us, not for the search criteria.
fn strip_keys(query: &str) -> String {
    query
        .split_whitespace()
        .filter(|word| !word.starts_with("in:"))
        .filter(|word| word.parse::<u32>().is_err())
        .collect::<Vec<_>>()
        .join(" ")
}

fn read_one<F>(session: Rc<Session>, mailbox: String, uid: u32, done: F)
where
    F: FnOnce(Result<String, String>) + 'static,
{
    let mailbox = session.dialect.get().mailbox(&mailbox);
    let fetching = session.clone();
    session.send(&imap::Command::Examine(mailbox), move |response| {
        if let Ok(response) = &response {
            if !response.is_ok() {
                done(Err(response.detail.clone()));
                return;
            }
        }
        fetching.send(&imap::Command::FetchBody(uid), move |response| {
            done(match response {
                Ok(response) if response.is_ok() => match imap::messages(&response).first() {
                    Some(message) => Ok(format!(
                        "[{}] {}\nFrom: {}\nSubject: {}\n\n{}",
                        message.uid, message.date, message.from, message.subject, message.preview
                    )),
                    None => Err(format!("there is no message {uid} in that folder")),
                },
                Ok(response) => Err(response.detail),
                Err(error) => Err(error),
            })
        });
    });
}

/// `label`, `move`, `delete` and their synonyms: select, then act.
fn file_them<F>(session: Rc<Session>, verb: String, args: Vec<String>, done: F)
where
    F: FnOnce(Result<String, String>) + 'static,
{
    let uids = ids_in(&args);
    if uids.is_empty() {
        done(Err(format!(
            "`{verb}` needs at least one message id, as `{verb} 42 …`"
        )));
        return;
    }
    let dialect = session.dialect.get();
    let mailbox = dialect.mailbox(&mailbox_of(&args));
    let acting = session.clone();
    session.send(&imap::Command::Select(mailbox), move |response| {
        if let Ok(response) = &response {
            if !response.is_ok() {
                done(Err(response.detail.clone()));
                return;
            }
        }
        let count = uids.len();
        let command = match verb.as_str() {
            "delete" | "trash" => imap::Command::Move {
                uids,
                mailbox: dialect.trash().to_string(),
            },
            // Archiving on Gmail is not a move: the message keeps every label
            // it has and loses the Inbox one, which is what leaving the inbox
            // *is* there. Moving it to All Mail instead would be a no-op that
            // reported success, since it is already in All Mail.
            "archive" if dialect.archive().is_none() && destination(&args).is_none() => {
                imap::Command::GmailLabels {
                    uids,
                    labels: vec!["\\Inbox".to_string()],
                    add: false,
                }
            }
            "move" | "archive" => imap::Command::Move {
                uids,
                mailbox: dialect.mailbox(
                    &destination(&args)
                        .or_else(|| dialect.archive().map(str::to_string))
                        .unwrap_or_else(|| "Archive".into()),
                ),
            },
            "unlabel" | "unflag" => match dialect.labelling(&labels_in(&args)) {
                email::Labelling::Keywords(flags) => imap::Command::RemoveFlags { uids, flags },
                email::Labelling::GmailLabels(labels) => imap::Command::GmailLabels {
                    uids,
                    labels,
                    add: false,
                },
            },
            _ => match dialect.labelling(&labels_in(&args)) {
                email::Labelling::Keywords(flags) => imap::Command::AddFlags { uids, flags },
                email::Labelling::GmailLabels(labels) => imap::Command::GmailLabels {
                    uids,
                    labels,
                    add: true,
                },
            },
        };
        let said = describe(&verb, count, &args, dialect);
        acting.send(&command, move |response| {
            done(match response {
                Ok(response) if response.is_ok() => Ok(said),
                Ok(response) => Err(response.detail),
                Err(error) => Err(error),
            })
        });
    });
}

/// The folder a move names: the first word that is not an id or a key.
fn destination(args: &[String]) -> Option<String> {
    args.iter()
        .filter(|word| word.parse::<u32>().is_err())
        .find(|word| !word.contains(':') && !word.starts_with(['+', '-']))
        .map(|name| name.trim_matches(['"', '\'']).to_string())
}

/// The labels a filing verb names. `+Invoices` and `Invoices` both mean the
/// label; a leading backslash would be a system flag and is not accepted from
/// the model.
///
/// Spaces are left alone here. A standard IMAP keyword cannot hold one and a
/// Gmail label can, so squashing it to an underscore is the dialect's decision
/// — and making it here would have quietly created a second `Needs_Reply`
/// beside the `Needs Reply` the user already had.
fn labels_in(args: &[String]) -> Vec<String> {
    let named: Vec<String> = args
        .iter()
        .filter(|word| word.parse::<u32>().is_err() && !word.contains(':'))
        .map(|word| word.trim_start_matches(['+', '-']).trim_matches('"'))
        .filter(|word| !word.is_empty() && !word.starts_with('\\'))
        .map(str::to_string)
        .collect();
    if named.is_empty() {
        return vec!["\\Flagged".to_string()];
    }
    named
}

fn describe(verb: &str, count: usize, args: &[String], dialect: email::Dialect) -> String {
    match verb {
        "delete" | "trash" => format!("Moved {count} message(s) to the Trash."),
        // On Gmail this took the Inbox label off and left the message where it
        // was, so saying it was moved somewhere would be a report of something
        // that did not happen.
        "archive" if dialect.archive().is_none() && destination(args).is_none() => {
            format!("Archived {count} message(s) — out of the inbox, still in All Mail.")
        }
        "move" | "archive" => format!(
            "Moved {count} message(s) to {}.",
            destination(args)
                .or_else(|| dialect.archive().map(str::to_string))
                .unwrap_or_else(|| "Archive".into())
        ),
        "unlabel" | "unflag" => format!(
            "Removed {} from {count} message(s).",
            labels_in(args).join(", ")
        ),
        _ => format!(
            "Labelled {count} message(s) {}.",
            labels_in(args).join(", ")
        ),
    }
}

/// Send one message, over SMTP.
fn send_message<F>(account: &Account, args: &[String], done: F)
where
    F: FnOnce(Result<String, String>) + 'static,
{
    let message = smtp::Outgoing {
        from: account.from.clone(),
        to: value_of(args, "to")
            .map(|to| {
                to.split([',', ';'])
                    .map(|one| one.trim().to_string())
                    .filter(|one| !one.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        subject: value_of(args, "subject").unwrap_or_default(),
        body: value_of(args, "body").unwrap_or_default(),
    };
    if let Err(refusal) = message.check() {
        done(Err(refusal.to_string()));
        return;
    }

    let account = account.clone();
    let client = gio::SocketClient::new();
    client.set_tls(account.tls);
    client.set_timeout(TIMEOUT);
    let (smtp_host, smtp_port) = (account.smtp_host.clone(), account.smtp_port);
    client.connect_to_host_async(
        &smtp_host,
        smtp_port,
        gio::Cancellable::NONE,
        move |connected| {
            let connection = match connected {
                Ok(connection) => connection,
                Err(error) => {
                    done(Err(format!(
                        "could not reach {}:{} — {error}",
                        account.smtp_host, account.smtp_port
                    )));
                    return;
                }
            };
            let steps = smtp::conversation(&message, &account.user, &account.password, "familiar");
            let body = smtp::data(&message.render(&now(), &message_id()));
            let recipients = message.to.join(", ");
            speak(
                Rc::new(connection.input_stream()),
                Rc::new(connection.output_stream()),
                steps,
                body,
                Box::new(move |result| done(result.map(|_| format!("Sent to {recipients}.")))),
            );
        },
    );
}

type Spoken = Box<dyn FnOnce(Result<(), String>)>;

/// Walk the SMTP conversation, one line at a time, checking each reply.
fn speak(
    input: Rc<gio::InputStream>,
    output: Rc<gio::OutputStream>,
    mut steps: Vec<String>,
    body: String,
    done: Spoken,
) {
    // The greeting and every reply are read the same way: one chunk, first
    // digit decides.
    input.clone().read_bytes_async(
        4096,
        glib::Priority::DEFAULT,
        gio::Cancellable::NONE,
        move |read| {
            let reply = match read {
                Ok(bytes) if !bytes.is_empty() => String::from_utf8_lossy(&bytes).to_string(),
                _ => {
                    done(Err("the mail server closed the connection".into()));
                    return;
                }
            };
            if !smtp::accepted(last_line(&reply)) {
                done(Err(format!("the mail server refused: {}", reply.trim())));
                return;
            }
            let next = if steps.is_empty() {
                // Everything said; the body goes last and ends the exchange.
                body.clone()
            } else {
                format!("{}\r\n", steps.remove(0))
            };
            let finished = steps.is_empty() && next == body;
            let bytes = glib::Bytes::from_owned(next.into_bytes());
            let input_again = input.clone();
            let output_again = output.clone();
            output.write_all_async(
                bytes,
                glib::Priority::DEFAULT,
                gio::Cancellable::NONE,
                move |written| {
                    if written.is_err() {
                        done(Err("the connection closed while sending".into()));
                        return;
                    }
                    if finished {
                        // One last reply: whether the message was accepted.
                        input_again.clone().read_bytes_async(
                            4096,
                            glib::Priority::DEFAULT,
                            gio::Cancellable::NONE,
                            move |read| {
                                let reply = read
                                    .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
                                    .unwrap_or_default();
                                done(if smtp::accepted(last_line(&reply)) {
                                    Ok(())
                                } else {
                                    Err(format!("the message was not accepted: {}", reply.trim()))
                                });
                            },
                        );
                        return;
                    }
                    speak(input_again, output_again, steps, body, done);
                },
            );
        },
    );
}

/// SMTP replies may be several lines; the last one carries the verdict.
fn last_line(reply: &str) -> &str {
    reply.trim_end().lines().last().unwrap_or(reply)
}

/// `subject=Roof quote` out of an argv, joining the words that follow it until
/// the next `key=`.
fn value_of(args: &[String], key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    let start = args.iter().position(|word| word.starts_with(&prefix))?;
    let first = args[start][prefix.len()..].to_string();
    let mut collected = vec![first];
    for word in &args[start + 1..] {
        if word.contains('=') && word.split('=').next().is_some_and(is_a_key) {
            break;
        }
        collected.push(word.clone());
    }
    Some(collected.join(" ").trim_matches('"').to_string())
}

fn is_a_key(word: &str) -> bool {
    matches!(word, "to" | "subject" | "body" | "cc" | "in")
}

fn now() -> String {
    chrono::Local::now()
        .format("%a, %d %b %Y %H:%M:%S %z")
        .to_string()
}

fn message_id() -> String {
    format!(
        "<{}.familiar@localhost>",
        chrono::Utc::now().timestamp_micros()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn a_folder_is_read_out_of_the_arguments_or_defaults_to_the_inbox() {
        assert_eq!(mailbox_of(&args("unread in:Archive")), "Archive");
        assert_eq!(mailbox_of(&args("unread")), "INBOX");
    }

    #[test]
    fn the_ids_a_filing_verb_acts_on_are_capped() {
        let many: Vec<String> = (1..=100).map(|n| n.to_string()).collect();
        assert_eq!(ids_in(&many).len(), email::MAX_TOUCHED);
        assert_eq!(ids_in(&args("42 +Invoices")), [42]);
    }

    #[test]
    fn a_label_cannot_set_a_system_flag() {
        // `\Deleted` through the labelling verb would be a delete that never
        // asked. Anything starting with a backslash is dropped.
        assert_eq!(labels_in(&args("42 +Invoices")), ["Invoices"]);
        assert_eq!(labels_in(&args("42 \\Deleted")), ["\\Flagged"]);
        assert_eq!(labels_in(&args("42 +Needs Reply")), ["Needs", "Reply"]);
    }

    #[test]
    fn a_send_reads_its_parts_out_of_the_argv() {
        let sent = args("to=ada@prins.example subject=Roof quote body=Could you send it?");
        assert_eq!(value_of(&sent, "to").as_deref(), Some("ada@prins.example"));
        assert_eq!(value_of(&sent, "subject").as_deref(), Some("Roof quote"));
        assert_eq!(
            value_of(&sent, "body").as_deref(),
            Some("Could you send it?")
        );
        assert_eq!(value_of(&sent, "cc"), None);
    }

    #[test]
    fn a_search_query_keeps_only_what_is_a_search_term() {
        assert_eq!(strip_keys("unread in:Archive 42"), "unread");
        assert_eq!(strip_keys("from:ada roof"), "from:ada roof");
    }

    #[test]
    fn an_smtp_verdict_is_read_off_the_last_line() {
        assert!(smtp::accepted(last_line("250-STARTTLS\r\n250 OK\r\n")));
        assert!(!smtp::accepted(last_line("250-OK\r\n535 nope\r\n")));
    }

    #[test]
    fn an_account_with_nothing_in_it_is_not_configured() {
        let empty = Account {
            host: "  ".into(),
            port: 993,
            user: String::new(),
            password: String::new(),
            tls: true,
            from: String::new(),
            smtp_host: String::new(),
            smtp_port: 465,
        };
        assert!(!empty.is_configured());
        assert!(Account {
            host: "mail.example".into(),
            user: "matthew".into(),
            ..empty
        }
        .is_configured());
    }
}
