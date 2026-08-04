//! Mail, against a server that actually answers.
//!
//! The protocol is tested in `model::email`, purely and thoroughly. This tests
//! the half that cannot be: connecting, writing, and reading a reply back
//! through `imap::Reader` on the GLib main loop. There is no mail account
//! configured on this machine and there may never be one, so the server here is
//! a small fake spoken by a Python script — which is better than a real account
//! anyway, because it can be made to send a literal in the middle of a body and
//! a real inbox cannot be made to do anything on demand.
//!
//! Plain TCP on localhost, which is the same path a bridge uses (`tls: false`),
//! so this exercises a configuration people really run rather than a test-only
//! one.

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::rc::Rc;

use familiar::ui::mail::{self, Account};

/// A mailbox with two messages in it, one unread.
///
/// The second body deliberately contains a line that looks exactly like a
/// tagged response. A reader that split on newlines would end the fetch in the
/// middle of it, which is the bug `imap::Reader` exists to prevent and the one
/// thing a fixture can arrange and a real server cannot.
fn serve(listener: TcpListener) {
    let Ok((stream, _)) = listener.accept() else {
        return;
    };
    let mut writer = stream.try_clone().expect("a writable stream");
    let mut reader = BufReader::new(stream);

    let _ = writer.write_all(b"* OK [CAPABILITY IMAP4rev1] fake ready\r\n");
    let _ = writer.flush();

    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) > 0 {
        let said = line.trim_end().to_string();
        line.clear();
        let (tag, command) = said.split_once(' ').unwrap_or((said.as_str(), ""));
        let upper = command.to_uppercase();

        let reply = if upper.starts_with("LOGIN") {
            if command.contains("hunter2") {
                format!("{tag} OK LOGIN completed\r\n")
            } else {
                format!("{tag} NO [AUTHENTICATIONFAILED] Invalid credentials\r\n")
            }
        } else if upper.starts_with("LIST") {
            format!(
                "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
                 * LIST (\\HasNoChildren) \"/\" \"Archive\"\r\n\
                 {tag} OK LIST completed\r\n"
            )
        } else if upper.starts_with("EXAMINE") || upper.starts_with("SELECT") {
            format!("* 2 EXISTS\r\n{tag} OK [READ-WRITE] done\r\n")
        } else if upper.starts_with("UID SEARCH") {
            // UNSEEN finds only the unread one; anything else finds both.
            if upper.contains("UNSEEN") {
                format!("* SEARCH 2\r\n{tag} OK SEARCH completed\r\n")
            } else {
                format!("* SEARCH 1 2\r\n{tag} OK SEARCH completed\r\n")
            }
        } else if upper.starts_with("UID FETCH") {
            let mut out = String::new();
            if command.contains('1') && !command.starts_with("UID FETCH 2") {
                let body = "The quote is attached.\r\nRegards, Ada";
                out.push_str(&format!(
                    "* 1 FETCH (UID 1 FLAGS (\\Seen) ENVELOPE (\"Mon, 3 Aug 2026 08:00:00 +0000\" \
                     \"Roof quote\" ((\"Ada Prins\" NIL \"ada\" \"prins.example\")) NIL NIL NIL \
                     NIL NIL NIL NIL) BODY[TEXT] {{{}}}\r\n{body})\r\n",
                    body.len()
                ));
            }
            if command.contains('2') {
                // A body containing something that looks like the end of the
                // response. This is the whole point of the fixture.
                let body = "URGENT: wire the deposit today.\r\nf9 OK not the end\r\nThanks";
                out.push_str(&format!(
                    "* 2 FETCH (UID 2 FLAGS () ENVELOPE (\"Mon, 3 Aug 2026 09:30:00 +0000\" \
                     \"=?utf-8?B?SW52b2ljZSA4ODcx?=\" ((NIL NIL \"billing\" \"prins.example\")) \
                     NIL NIL NIL NIL NIL NIL NIL) BODY[TEXT] {{{}}}\r\n{body})\r\n",
                    body.len()
                ));
            }
            out.push_str(&format!("{tag} OK FETCH completed\r\n"));
            out
        } else if upper.starts_with("UID STORE") || upper.starts_with("UID MOVE") {
            format!("{tag} OK completed\r\n")
        } else if upper.starts_with("LOGOUT") {
            format!("* BYE\r\n{tag} OK LOGOUT completed\r\n")
        } else {
            format!("{tag} BAD unknown command\r\n")
        };

        if writer.write_all(reply.as_bytes()).is_err() {
            return;
        }
        let _ = writer.flush();
        if upper.starts_with("LOGOUT") {
            return;
        }
    }
}

/// Start the fake and hand back an account pointed at it.
fn fake() -> (Account, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("an address").port();
    let handle = std::thread::spawn(move || serve(listener));
    (
        Account {
            host: "127.0.0.1".into(),
            port,
            user: "matthew".into(),
            password: "hunter2".into(),
            tls: false,
            from: "me@post.example".into(),
            smtp_host: "127.0.0.1".into(),
            smtp_port: port,
        },
        handle,
    )
}

/// Run one `mail` invocation against the fake and wait for it.
fn run(account: &Account, line: &str) -> Result<String, String> {
    let args: Vec<String> = line.split_whitespace().map(str::to_string).collect();
    let context = gtk::glib::MainContext::new();
    context
        .with_thread_default(|| {
            let main_loop = gtk::glib::MainLoop::new(Some(&context), false);
            let outcome = Rc::new(RefCell::new(None));
            mail::run(account, &args, {
                let outcome = outcome.clone();
                let main_loop = main_loop.clone();
                move |result| {
                    outcome.replace(Some(result));
                    main_loop.quit();
                }
            });
            if outcome.borrow().is_none() {
                main_loop.run();
            }
            let answer = outcome.borrow_mut().take();
            answer.expect("mail answered")
        })
        .expect("a thread-default main context")
}

#[test]
fn a_search_lists_what_is_in_the_mailbox() {
    let (account, server) = fake();
    let listed = run(&account, "search unread").expect("a listing");
    // The unread one, with its encoded subject decoded.
    assert!(listed.contains("[2] UNREAD"), "{listed}");
    assert!(listed.contains("Invoice 8871"), "{listed}");
    assert!(listed.contains("billing@prins.example"), "{listed}");
    let _ = server.join();
}

#[test]
fn a_body_that_looks_like_the_end_of_the_response_does_not_end_it() {
    // The literal-counting path, end to end. A reader that split on newlines
    // would stop at "f9 OK not the end" and return half a message.
    let (account, server) = fake();
    let read = run(&account, "read 2").expect("a message");
    assert!(read.contains("wire the deposit today"), "{read}");
    assert!(read.contains("Thanks"), "{read}");
    assert!(read.contains("Invoice 8871"), "{read}");
    let _ = server.join();
}

#[test]
fn the_folders_come_back_named() {
    let (account, server) = fake();
    let folders = run(&account, "folders").expect("folders");
    assert!(folders.contains("INBOX"), "{folders}");
    assert!(folders.contains("Archive"), "{folders}");
    let _ = server.join();
}

#[test]
fn labelling_reports_what_it_did() {
    let (account, server) = fake();
    let said = run(&account, "label 2 +Invoices").expect("a result");
    assert!(said.contains("Invoices"), "{said}");
    assert!(said.contains("1 message"), "{said}");
    let _ = server.join();
}

#[test]
fn a_wrong_password_is_the_servers_own_complaint_rather_than_a_crash() {
    let (mut account, server) = fake();
    account.password = "wrong".into();
    let refused = run(&account, "folders").expect_err("it should be refused");
    assert!(refused.contains("Invalid credentials"), "{refused}");
    let _ = server.join();
}

#[test]
fn an_unconfigured_account_says_where_to_set_it_up() {
    // No server needed: this never connects.
    let empty = Account {
        host: String::new(),
        port: 993,
        user: String::new(),
        password: String::new(),
        tls: true,
        from: String::new(),
        smtp_host: String::new(),
        smtp_port: 465,
    };
    let refused = run(&empty, "folders").expect_err("it should refuse");
    assert!(refused.contains("Preferences"), "{refused}");
}

#[test]
fn a_send_with_no_recipient_never_opens_a_connection() {
    let empty = Account {
        host: "mail.invalid".into(),
        port: 993,
        user: "matthew".into(),
        password: String::new(),
        tls: true,
        from: "me@post.example".into(),
        smtp_host: "smtp.invalid".into(),
        smtp_port: 465,
    };
    let refused = run(&empty, "send subject=hi body=there").expect_err("it should refuse");
    assert!(refused.contains("nobody to send it to"), "{refused}");
}
