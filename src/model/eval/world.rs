//! The small, finite place every scenario is asked in.
//!
//! The reason this exists is a measurement bug. The first stubs answered every
//! read successfully and with fresh-looking material: `recall` found notes for
//! any query, `search_files` found hits for any word, `list_dir` listed the same
//! five entries for any path. Against that, gathering material never terminates
//! — and the model duly spent thirteen calls on `recall`/`list_dir`/`read_file`
//! and never got as far as writing the document it had been asked for. The suite
//! was scoring the fertility of its own fixtures.
//!
//! A real workspace is finite and mostly says no. So this one is four notes and
//! nine files, reads are answered against it, and a miss comes back as a miss.
//! Everything a scenario asks about is *in* here, which is what lets a model
//! find what it needs in one or two calls and move on to the actual work.

/// Files under the workspace root. A directory exists because a path in here
/// names it, which is how a real tree works.
pub const FILES: &[(&str, &str)] = &[
    (
        "budget-2026.md",
        "# Budget 2026\n\n\
         | Category | Planned | Spent |\n|---|---|---|\n\
         | Roof | 14000 | 13850 |\n| Windows | 6000 | 0 |\n| Landscaping | 2500 | 900 |\n\n\
         The roof came in slightly under. Windows are deferred to Q4.",
    ),
    (
        "roof-quotes.md",
        "# Roof quotes\n\n\
         Vandenberg — 13,850, north slope, 30-year architectural shingle, 3 days.\n\
         Kettering — 15,200, same scope, 5 days.\n\
         Aalders — 12,400, but 25-year shingle and no ice-and-water shield.\n\n\
         Went with Vandenberg. Work finished 24 April 2026; ten-year workmanship warranty.",
    ),
    (
        "notes/contractors.md",
        "# Contractors\n\n\
         Vandenberg Roofing — Bexley, OH. Did the north slope, April 2026. Would use again.\n\
         Prins Electrical — quoted the panel upgrade, not booked.",
    ),
    (
        "notes/windows.md",
        "# Windows\n\n\
         Eight casements on the south elevation, original 1994 units, seals gone on three.\n\
         Quoted 6,000 for the lot. Deferred to Q4 2026 to keep the roof under budget.",
    ),
    (
        "notes/roof-quotes.md",
        "# Roof, working notes\n\n\
         Vandenberg started 21 April 2026 and finished on the 24th. Tore off two layers,\n\
         found no deck rot. Final invoice 13,850, which was 150 under the quote.",
    ),
    ("lists/shopping.md", "# Shopping\n\n- coffee\n- olive oil"),
    ("contracts/lease.pdf", PDF),
    ("contracts/a.pdf", PDF),
    ("contracts/b.pdf", PDF),
    ("contracts/c.pdf", PDF),
];

/// A marker rather than bytes: `read_file` refuses these the way the real
/// workspace refuses anything that is not UTF-8 text, which is what should push
/// the model towards `read_pdf`.
pub const PDF: &str = "\u{0}pdf";

/// The workflows this world has already saved, as goal and steps.
///
/// One, and it exists so `start` can be scored at all: "run the quarterly
/// comparison workflow" has a right answer only if there is something of that
/// name to run. A scenario that asks for one that is *not* here is scoring the
/// opposite — that the model says so rather than inventing steps and claiming
/// they were saved.
pub const WORKFLOWS: &[(&str, &[&str])] = &[(
    "Quarterly comparison",
    &[
        "Read last quarter's figures out of the workspace",
        "Pull this quarter's from the spreadsheet",
        "Write the comparison up as a document",
    ],
)];

/// A saved workflow by name, matched the way [`crate::model::project::Store`]
/// matches one: on the slug of the goal, because the goal is the only name the
/// model ever saw.
pub fn workflow(name: &str) -> Option<crate::model::workflow::Workflow> {
    let wanted = crate::model::project::slugify(name);
    WORKFLOWS.iter().find_map(|(goal, steps)| {
        (crate::model::project::slugify(goal) == wanted)
            .then(|| {
                crate::model::workflow::Workflow::proposed(
                    goal,
                    steps.iter().map(|step| step.to_string()).collect(),
                )
                .ok()
            })
            .flatten()
    })
}

/// The vault, as `recall` sees it: a subject and what the note says.
pub const NOTES: &[(&str, &str)] = &[
    (
        "Roof",
        "The north slope was replaced in April 2026 by Vandenberg for 13,850. \
         Ten-year workmanship warranty. See [[Contractors]].",
    ),
    // The editor is **Neovim** here, and it has to be. `memory/durable-fact`
    // opens with "I've switched from Neovim to Zed" and expects the switch to be
    // saved. With Zed already in the vault the model recalled, found it, and
    // said "your notes already record that you use Zed" — which is a correct
    // reading of a fixture that had quietly made the news old. The scenario
    // scored 80% and the two points it lost were the fixture's.
    (
        "Matthew",
        "Writes Rust. Lives in Ashford, Ohio. Prefers small, single-purpose commits \
         and a direct answer over a hedged one. Uses Neovim as their main editor.",
    ),
    (
        "Familiar",
        "A GTK 4 desktop assistant that talks to a llama-server on the same machine. \
         The volatile part of its prompt goes last so the KV cache survives.",
    ),
    (
        "Contractors",
        "Vandenberg Roofing did the roof and would be used again. Prins Electrical quoted \
         the panel upgrade but was not booked.",
    ),
    // `documents/skill-once-per-conversation` asks for a second document, about
    // the windows, after one about the roof. Without this the vault answered
    // that question with the roof note — "workmanship" contains "work" — and
    // the model correctly said it had nothing about the windows and stopped.
    // What the scenario is for is whether the skill is read twice; it cannot
    // measure that if the second document has nothing to be made out of.
    (
        "Windows",
        "Eight casements on the south elevation, original 1994 units, seals gone on three. \
         Quoted 6,000 for the lot. Deferred to Q4 2026 to keep the roof under budget.",
    ),
];

/// Every directory named by a file path, plus the root.
pub fn directories() -> Vec<String> {
    let mut found = vec![".".to_string()];
    for (path, _) in FILES {
        if let Some((directory, _)) = path.rsplit_once('/') {
            if !found.contains(&directory.to_string()) {
                found.push(directory.to_string());
            }
        }
    }
    found
}

/// A path as the tools take it: `"."`, `"./notes"` and `"notes/"` are one place.
pub fn normalise(path: &str) -> String {
    let trimmed = path.trim().trim_start_matches("./").trim_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        ".".to_string()
    } else {
        trimmed.to_string()
    }
}

/// What a file holds, if it is one.
pub fn file(path: &str) -> Option<&'static str> {
    let wanted = normalise(path);
    FILES
        .iter()
        .find(|(name, _)| *name == wanted)
        .map(|(_, contents)| *contents)
}

pub fn is_directory(path: &str) -> bool {
    let wanted = normalise(path);
    directories().contains(&wanted)
}

/// What is immediately inside a directory: files as names, subdirectories with
/// a trailing slash, sorted, the way `workspace::list` renders it.
pub fn entries(path: &str) -> Vec<String> {
    let wanted = normalise(path);
    let prefix = if wanted == "." {
        String::new()
    } else {
        format!("{wanted}/")
    };

    let mut listed: Vec<String> = Vec::new();
    for (name, contents) in FILES {
        let Some(rest) = name.strip_prefix(prefix.as_str()) else {
            continue;
        };
        // Only what is directly inside: `notes/windows.md` is an entry of
        // `notes`, and `notes/` is an entry of the root.
        match rest.split_once('/') {
            Some((directory, _)) => {
                let entry = format!("{directory}/");
                if !listed.contains(&entry) {
                    listed.push(entry);
                }
            }
            None => listed.push(format!("{name} ({} bytes)", contents.len())),
        }
    }
    // The rendered form carries the full path for a file at the root and only
    // the leaf below it, which is what the real lister does.
    let listed = listed
        .into_iter()
        .map(|entry| match entry.strip_prefix(prefix.as_str()) {
            Some(leaf) if !prefix.is_empty() => leaf.to_string(),
            _ => entry,
        })
        .collect::<Vec<_>>();
    let mut listed = listed;
    listed.sort();
    listed
}

/// Which files contain some text, and on which lines.
pub fn grep(needle: &str, within: Option<&str>) -> Vec<(String, usize)> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let scope = within.map(normalise).filter(|path| path != ".");
    let mut hits = Vec::new();
    for (name, contents) in FILES {
        if *contents == PDF {
            continue;
        }
        if let Some(scope) = &scope {
            if !name.starts_with(&format!("{scope}/")) && *name != scope.as_str() {
                continue;
            }
        }
        for (number, line) in contents.lines().enumerate() {
            if line.to_lowercase().contains(&needle) {
                hits.push((name.to_string(), number + 1));
                break;
            }
        }
    }
    hits
}

/// A note found by `recall`, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    pub subject: &'static str,
    pub body: &'static str,
    /// The words were there. When this is false the note was found by meaning
    /// alone, which the real tool marks and the model is told to treat as a
    /// weaker answer.
    pub lexical: bool,
}

/// A word that means the same as one in the fixture, for the semantic half.
///
/// Hand-written and tiny, because that is the honest shape of it: real hybrid
/// search finds "the thing the contractor did in spring" from a note about a
/// roof, and it does not find a note about something the vault has never heard
/// of. A fixture that answered every query would put the suite back where
/// [`crate::model::eval::stub`] says it started — measuring the fertility of its
/// own fixtures.
const MEANS: &[(&str, &str)] = &[
    ("contractor", "vandenberg"),
    ("shingle", "roof"),
    ("slope", "roof"),
    ("editor", "matthew"),
    ("assistant", "familiar"),
    ("gtk", "familiar"),
    ("ohio", "matthew"),
];

/// The notes a query matches, the way hybrid search does.
///
/// The lexical half is what it always was: any word of four letters or more, in
/// the subject or the body. The semantic half is [`MEANS`], and a note found
/// only that way comes back marked — which is the distinction the guidance
/// spends a sentence on and a scenario can therefore hold to account.
///
/// It still comes back empty for a subject the vault has nothing on. A scenario
/// about taking "nothing" for an answer only works if the fixture can say it.
pub fn recall(query: &str) -> Vec<Found> {
    let words: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() >= 4)
        .map(str::to_lowercase)
        .collect();
    if words.is_empty() {
        return Vec::new();
    }

    let mut found: Vec<Found> = Vec::new();
    for (subject, body) in NOTES {
        let haystack = format!("{subject} {body}").to_lowercase();
        let lexical = words.iter().any(|word| haystack.contains(word.as_str()));
        let meant = MEANS.iter().any(|(term, about)| {
            words.iter().any(|word| word == term) && subject.to_lowercase().contains(about)
        });
        if lexical || meant {
            found.push(Found {
                subject,
                body,
                lexical,
            });
        }
    }
    found
}

// -- the Python sandbox -------------------------------------------------------

/// What a script the model wrote appears to have printed.
///
/// **Nothing is executed.** Running a script a language model has just written
/// is precisely what a harness with no sandbox around it must not do, and the
/// suite's question is what the model *reaches for* rather than whether its
/// arithmetic was right — the same reason no search here touches Exa.
///
/// So this reads the script instead. It finds the `print()` calls, keeps the
/// label each one starts with, and gives it a number. That last part is the
/// whole trick, and it is the lesson [`super::stub`] records about `web_search`
/// applied here: a fixture has to be wrong the way the real tool is wrong and
/// not in new ways. A script that prints three labelled figures and gets back
/// one bare number has been answered by something that is visibly not Python,
/// and the model responds by rewriting the script — which is a spiral the score
/// would read as a prompt failure. Matching the shape of what was printed costs
/// thirty lines and removes the confound.
///
/// The numbers are nonsense, deterministically. A scenario that needs the model
/// to *report* a specific figure stubs `run_python` itself with that figure,
/// which several do.
pub fn python(code: &str) -> crate::model::sandbox::Ran {
    use crate::model::sandbox::Ran;

    // The two things the container really does differently from a laptop, and
    // both are things a model gets wrong until it has seen them once.
    if reaches_the_network(code) {
        return Ran {
            stdout: String::new(),
            stderr: "Traceback (most recent call last):\n  \
                     File \"/work/.familiar/script.py\", line 3, in <module>\n    \
                     response = urlopen(url)\n\
                     urllib.error.URLError: <urlopen error [Errno -3] \
                     Temporary failure in name resolution>"
                .into(),
            code: 1,
            timed_out: false,
            created: Vec::new(),
        };
    }
    if never_terminates(code) {
        return Ran {
            stdout: printed(code),
            stderr: String::new(),
            code: 255,
            timed_out: true,
            created: Vec::new(),
        };
    }

    Ran {
        stdout: printed(code),
        stderr: String::new(),
        code: 0,
        timed_out: false,
        created: written(code),
    }
}

fn reaches_the_network(code: &str) -> bool {
    [
        "requests.",
        "urlopen",
        "urllib.request",
        "socket.create_connection",
        "httpx.",
    ]
    .iter()
    .any(|marker| code.contains(marker))
}

fn never_terminates(code: &str) -> bool {
    code.contains("while True:") && !code.contains("break")
}

/// The files a script plainly creates, for the note about getting one out.
fn written(code: &str) -> Vec<String> {
    let mut made = Vec::new();
    for marker in ["savefig(", "to_csv(", "to_excel(", "\"w\")", "'w')"] {
        let Some(at) = code.find(marker) else {
            continue;
        };
        let after = &code[at + marker.len()..];
        if let Some(name) = quoted(after) {
            let name = name.trim_start_matches("/work/").to_string();
            if !name.is_empty() && !made.contains(&name) {
                made.push(name);
            }
        }
    }
    made
}

/// The first quoted string in a fragment, however it was quoted.
fn quoted(text: &str) -> Option<String> {
    let start = text.find(['"', '\''])?;
    let quote = text.as_bytes()[start] as char;
    let rest = &text[start + 1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

/// One output line per `print()`, made of whatever that call can be answered
/// with.
///
/// The rule that matters is the first one: **a print of nothing but string
/// literals echoes them and gets no number.** `print("hello")` answering
/// `hello 48213` is output the script could not have produced, and a model
/// reading that concludes — correctly — that the tool is broken. One run said
/// so in as many words, *"the Python tool is corrupting its output on every
/// call"*, and then spent eight calls probing the interpreter instead of
/// answering the question. That is the `web_search` lesson in
/// [`super::stub`] arriving a second time by a different door, and it is worse
/// here: a model can check Python output against the script it just wrote,
/// which it cannot do with a search result.
fn printed(code: &str) -> String {
    let mut lines = Vec::new();
    for (index, at) in code.match_indices("print(").map(|(at, _)| at).enumerate() {
        let call = call_after(&code[at + "print(".len()..]);
        let arguments = arguments_of(&call);

        // Nothing computed: the script is printing words, so the words are the
        // answer and there is nothing to invent.
        if !arguments.is_empty()
            && arguments
                .iter()
                .all(|argument| is_a_plain_literal(argument))
        {
            lines.push(
                arguments
                    .iter()
                    .filter_map(|argument| quoted(argument))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            continue;
        }

        // Looking around the directory, which is the other thing a model does
        // when it is not sure the tool works. Answering with a number would
        // send it round the same loop.
        if call.contains("listdir") || call.contains("glob") || call.contains("iterdir") {
            lines.push(format!("{:?}", listing_of(code)));
            continue;
        }

        lines.push(match label(&arguments) {
            Some(label) => format!("{label} {}", figure(&call, index)),
            None => figure(&call, index),
        });
    }
    if lines.is_empty() {
        // A script that computes and never prints. The real sandbox answers
        // this with nothing at all, and the framing is what says so.
        return String::new();
    }
    lines.join("\n")
}

/// The text inside one `print(...)`, balanced rather than to end of line — a
/// call split across lines is ordinary Python and truncating it at the newline
/// loses the arguments that decide how it is answered.
fn call_after(text: &str) -> String {
    let mut depth = 1usize;
    let mut quote: Option<char> = None;
    let mut taken = String::new();
    for character in text.chars() {
        match (quote, character) {
            (Some(open), c) if c == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(character),
            (None, '(' | '[' | '{') => depth += 1,
            (None, ')' | ']' | '}') => {
                depth -= 1;
                if depth == 0 {
                    return taken;
                }
            }
            _ => {}
        }
        taken.push(character);
    }
    taken
}

/// A call's arguments, split on the commas that are actually separators.
fn arguments_of(call: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut current = String::new();
    for character in call.chars() {
        match (quote, character) {
            (Some(open), c) if c == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(character),
            (None, '(' | '[' | '{') => depth += 1,
            (None, ')' | ']' | '}') => depth = depth.saturating_sub(1),
            (None, ',') if depth == 0 => {
                arguments.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(character);
    }
    if !current.trim().is_empty() {
        arguments.push(current);
    }
    arguments
        .into_iter()
        .map(|argument| argument.trim().to_string())
        .filter(|argument| !argument.is_empty())
        .collect()
}

/// A quoted string with nothing interpolated into it, and no keyword argument
/// hiding in front of it — `sep=", "` is not something to echo.
fn is_a_plain_literal(argument: &str) -> bool {
    let trimmed = argument.trim();
    if trimmed.contains('=') && !trimmed.starts_with(['"', '\'']) {
        return false;
    }
    let bare = trimmed.trim_start_matches(['f', 'r', 'b', 'F', 'R', 'B']);
    let Some(open) = bare.chars().next() else {
        return false;
    };
    if open != '"' && open != '\'' {
        return false;
    }
    // An f-string with a placeholder in it is a computed value, not a literal.
    !(trimmed.starts_with('f') || trimmed.starts_with('F')) || !bare.contains('{')
}

/// What a directory listing would show: what this script made, and whatever a
/// previous one is likely to have left there.
fn listing_of(code: &str) -> Vec<String> {
    let mut listed = written(code);
    for name in ["readings.csv", "data.csv"] {
        if code.contains(name) && !listed.contains(&name.to_string()) {
            listed.push(name.to_string());
        }
    }
    listed
}

/// The words a print starts with, from a plain literal or the front of an
/// f-string. `print(f"Total: {t:.2f}")` labels its number "Total:".
fn label(arguments: &[String]) -> Option<String> {
    let literal = quoted(arguments.first()?)?;
    let front = literal.split('{').next().unwrap_or_default().trim();
    (!front.is_empty()).then(|| front.to_string())
}

/// A stable, meaningless number, shaped by how the script asked for it.
///
/// Two decimal places when the call formats a float and a whole number when it
/// does not, because a model that asked for `:.2f` and got `7` is looking at
/// output its own script could not have produced.
fn figure(call: &str, index: usize) -> String {
    let seed: u64 = call
        .bytes()
        .fold(1469598103934665603u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(1099511628211)
        })
        .wrapping_add(index as u64);
    let whole = seed % 90_000 + 137;
    if call.contains(":.2f") || call.contains("round(") || call.contains("/") {
        return format!("{}.{:02}", whole, seed % 100);
    }
    whole.to_string()
}

// -- the sibling applications -------------------------------------------------

/// The task list every `planner` scenario is asked against.
///
/// Finite and specific, for the reason the whole module exists: a stub that
/// answered every query with plausible tasks would let a model list forever and
/// never do the thing it was asked. Three projects, five open tasks, and two of
/// them share a word so that "the report" is genuinely ambiguous rather than
/// ambiguous only in principle.
const TASKS: &[(u32, &str, &str, &str)] = &[
    (11, "Ring the plumber about the boiler", "Home", "p1"),
    (12, "Put the bins out", "Home", "p3"),
    (13, "Draft the quarterly report", "Work", "p2"),
    (14, "Review the quarterly report", "Work", "p2"),
    (15, "Renew the lease", "Admin", "p1"),
];

const PROJECTS: &[&str] = &["Home", "Work", "Admin"];

/// The one task that repeats, which is the case a model reports as the opposite
/// of what happened.
const REPEATS: u32 = 12;

fn task_json(id: u32) -> String {
    let (id, title, project, priority) = TASKS
        .iter()
        .find(|(candidate, ..)| *candidate == id)
        .copied()
        .unwrap_or((0, "", "Inbox", "p4"));
    format!(
        r#"{{"id":{id},"title":"{title}","project":"{project}","priority":"{priority}","due":"2026-08-03"}}"#
    )
}

/// Which tasks a reference names. More than one is the `ambiguous` case.
fn matching(reference: &str) -> Vec<u32> {
    let needle = reference.trim().trim_matches(['\'', '"']).to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    if let Ok(id) = needle.parse::<u32>() {
        return TASKS
            .iter()
            .filter(|(candidate, ..)| *candidate == id)
            .map(|(id, ..)| *id)
            .collect();
    }
    TASKS
        .iter()
        .filter(|(_, title, ..)| title.to_lowercase().contains(&needle))
        .map(|(id, ..)| *id)
        .collect()
}

/// What `planner agent <argv>` would have answered.
pub fn planner_reply(argv: &[String]) -> String {
    let verb = crate::model::planner::verb(argv).unwrap_or_default();
    let rest: Vec<&str> = argv
        .iter()
        .map(String::as_str)
        .skip_while(|word| *word == "agent")
        .skip(1)
        .collect();
    let line = rest.join(" ");

    match verb {
        "overview" | "projects" => format!(
            r#"{{"ok":true,"action":"overview","projects":[{}],"labels":["errand","email"],"filters":["Today","Next 7 days"],"counts":{{"open":{},"overdue":1}}}}"#,
            PROJECTS
                .iter()
                .map(|name| format!(r#"{{"name":"{name}","sections":[]}}"#))
                .collect::<Vec<_>>()
                .join(","),
            TASKS.len()
        ),
        "list" | "tasks" => {
            let wanted: Vec<String> = TASKS
                .iter()
                .filter(|(_, _, project, priority)| {
                    let query = line.to_lowercase();
                    if query.contains("p1") {
                        return *priority == "p1";
                    }
                    if let Some(named) = PROJECTS
                        .iter()
                        .find(|name| query.contains(&format!("#{}", name.to_lowercase())))
                    {
                        return **project == **named;
                    }
                    true
                })
                .map(|(id, ..)| task_json(*id))
                .collect();
            format!(
                r#"{{"ok":true,"action":"list","tasks":[{}],"count":{},"matched":{},"truncated":false}}"#,
                wanted.join(","),
                wanted.len(),
                wanted.len()
            )
        }
        "show" | "task" | "complete" | "done" | "check" | "delete" | "remove" | "update"
        | "edit" | "set" | "reopen" => {
            let reference = rest.first().copied().unwrap_or_default();
            let found = matching(reference);
            match found.as_slice() {
                [] => format!(
                    r#"{{"ok":false,"error":"not-found","message":"Nothing matches {reference}.","hint":"`list` shows what is open."}}"#
                ),
                [id] => finished(verb, *id),
                many => format!(
                    r#"{{"ok":false,"error":"ambiguous","message":"More than one open task matches {reference}.","candidates":[{}]}}"#,
                    many.iter()
                        .map(|id| task_json(*id))
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            }
        }
        "search" | "find" => {
            let found: Vec<String> = matching(&line).iter().map(|id| task_json(*id)).collect();
            format!(
                r#"{{"ok":true,"action":"search","tasks":[{}],"count":{}}}"#,
                found.join(","),
                found.len()
            )
        }
        "add" | "new" => {
            // The project is honoured only if it exists. That is the whole
            // point of the fixture: a model that invents `#Household` has to be
            // able to see that the task went to the Inbox instead.
            let project = PROJECTS
                .iter()
                .find(|name| {
                    line.to_lowercase()
                        .contains(&format!("#{}", name.to_lowercase()))
                })
                .copied()
                .unwrap_or("Inbox");
            let title: String = line
                .split_whitespace()
                .filter(|word| !word.starts_with('#') && !word.starts_with('@'))
                .collect::<Vec<_>>()
                .join(" ");
            format!(
                r#"{{"ok":true,"action":"added","task":{{"id":21,"title":"{title}","project":"{project}","due":"2026-08-03"}}}}"#
            )
        }
        "" => r#"{"ok":false,"error":"unknown-verb","message":"No verb was given."}"#.to_string(),
        other => format!(
            r#"{{"ok":false,"error":"unknown-verb","message":"There is no verb called {other}.","hint":"`help` lists them."}}"#
        ),
    }
}

/// The response for a verb that acted on exactly one task.
fn finished(verb: &str, id: u32) -> String {
    match verb {
        "complete" | "done" | "check" if id == REPEATS => format!(
            r#"{{"ok":true,"action":"completed","outcome":"completed-and-repeats","next_due":"2026-08-10","task":{}}}"#,
            task_json(id)
        ),
        "complete" | "done" | "check" => format!(
            r#"{{"ok":true,"action":"completed","outcome":"done","task":{}}}"#,
            task_json(id)
        ),
        "delete" | "remove" => format!(
            r#"{{"ok":true,"action":"deleted","task":{}}}"#,
            task_json(id)
        ),
        "update" | "edit" | "set" => format!(
            r#"{{"ok":true,"action":"updated","applied":["due"],"task":{}}}"#,
            task_json(id)
        ),
        "reopen" | "uncomplete" | "uncheck" => format!(
            r#"{{"ok":true,"action":"reopened","task":{}}}"#,
            task_json(id)
        ),
        _ => format!(r#"{{"ok":true,"action":"show","task":{}}}"#, task_json(id)),
    }
}

/// What `magpie agent <argv>` would have answered.
///
/// One prior transcript, so `list` before transcribing is a call that can
/// actually pay off, and one link that is a playlist, so the refusal is
/// reachable.
pub fn magpie_reply(argv: &[String]) -> String {
    let verb = crate::model::magpie::verb(argv).unwrap_or_default();
    let rest: Vec<&str> = argv
        .iter()
        .map(String::as_str)
        .skip_while(|word| *word == "agent")
        .skip(1)
        .collect();
    let line = rest.join(" ").to_lowercase();

    match verb {
        "tools" => r#"{"ok":true,"action":"tools","ready":{"transcribe":true,"missing":[]},"speech_models":[{"name":"small","on_disk":true},{"name":"medium","on_disk":false}]}"#
            .to_string(),
        "list" => {
            if line.is_empty() || line.contains("lecture") || line.contains("kv") {
                r#"{"ok":true,"action":"list","downloads":[{"id":3,"title":"The KV cache lecture","url":"https://youtu.be/kv","state":"done","transcript":{"state":"ready","format":"text","path":"/home/user/Downloads/The KV cache lecture.txt","bytes":41203}}],"count":1}"#.to_string()
            } else {
                r#"{"ok":true,"action":"list","downloads":[],"count":0}"#.to_string()
            }
        }
        "show" => r#"{"ok":true,"action":"show","job":{"id":3,"title":"The KV cache lecture","state":"done","transcript":{"state":"ready","format":"text","path":"/home/user/Downloads/The KV cache lecture.txt","bytes":41203}}}"#
            .to_string(),
        "transcribe" => {
            if line.contains("playlist") || line.contains("list=pl") {
                return r#"{"ok":false,"error":"refused","message":"That link is a playlist, and transcribing all of it is hours of work.","hint":"Pass the link to a single video."}"#
                    .to_string();
            }
            let speakers = line.contains("speakers=yes") || line.contains("speakers=2");
            format!(
                r#"{{"ok":true,"action":"transcribed","job":{{"id":8,"title":"Me at the zoo","state":"done","status":"Saved to Downloads","media":{{"path":"/home/user/Downloads/Me at the zoo.webm","bytes":252182}},"transcript":{{"state":"ready","format":"text","model":"small","path":"/home/user/Downloads/Me at the zoo.txt","bytes":193{}}}}}}}"#,
                if speakers {
                    r#","speakers":"2 speakers · Alice, Speaker 2""#
                } else {
                    ""
                }
            )
        }
        "" => r#"{"ok":false,"error":"unknown-verb","message":"No verb was given."}"#.to_string(),
        other => format!(
            r#"{{"ok":false,"error":"unknown-verb","message":"There is no verb called {other}.","hint":"`help` lists them."}}"#
        ),
    }
}

// -- the web ------------------------------------------------------------------

/// One page the invented index can return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub title: String,
    pub url: String,
    /// ISO 8601, because that is what Exa returns and what the brief prints.
    pub published: String,
    /// What the page says. **Facts, not a description of facts.**
    pub text: String,
}

/// Where invented pages live. Not `example.com` — see [`crate::model::eval::stub::HOSTS`].
const FIELD: &str = "https://fieldnotes.dev";
const WEEKLY: &str = "https://kernelweekly.io";
const FORUM: &str = "https://forum.buildlog.sh";

/// Results for a query, as a search index would answer it.
///
/// **The single largest source of bad scores this suite has had.** The version
/// this replaces returned the same three paragraphs for every query — the same
/// "4.2.1 release", the same "8% down to under 1%", the same "340ms to 12ms" —
/// with only the title and the URL slug varying. Handed that, the model did
/// exactly what a person would: it noticed, said so out loud ("the results came
/// back as templated placeholder content"), searched three or four more times
/// hunting for the real index, and then answered from its weights and cited
/// nothing. That one fixture accounts for `semantic-query`, `news-not-search`,
/// `general-sweep`, `cites-what-it-used`, `time-question-becomes-dates`,
/// `best-of-breed-moves` and `the-sandbox-has-no-network` — every one of which
/// read as a prompt failure and was not.
///
/// So there are two halves here. The subjects the suite actually asks about get
/// real, specific, mutually consistent answers, because a scenario that checks
/// the model *used* what it found needs there to be something to use. Everything
/// else falls through to [`invented`], which varies its claims, its figures, its
/// dates and its shape with the query rather than restating one paragraph.
pub fn pages(query: &str) -> Vec<Page> {
    let asked = query.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|word| asked.contains(word));

    if has(&[
        "kv cache",
        "kv-cache",
        "prefix",
        "prompt cach",
        "cached prefix",
    ]) {
        return vec![
            Page {
                title: "Prompt caching in llama.cpp, end to end".into(),
                url: format!("{FIELD}/llama-cpp-prompt-cache"),
                published: "2026-07-18".into(),
                text: "llama-server hashes the incoming token sequence against what is already \
                       in the KV cache and reuses the longest common prefix. The match is \
                       token-for-token from position zero: change one token at position 40 and \
                       everything from 40 on is recomputed, however identical the rest is. That \
                       is the whole rule, and every practical consequence follows from it — put \
                       the system prompt first, tool definitions after it, retrieved context \
                       after that, and the user's turn last. Measured on a 27B model at Q4_K_M: \
                       a 6,100-token prefix costs 1.9s to prefill cold and 40ms warm."
                    .into(),
            },
            Page {
                title: "Keep the volatile part of the prompt last".into(),
                url: format!("{WEEKLY}/stable-prefix"),
                published: "2026-07-02".into(),
                text: "The mistake almost everyone makes is putting the date, or the retrieved \
                       memory block, or a per-turn timestamp near the top of the system prompt. \
                       It reads as harmless and it invalidates the cache on every single turn. \
                       Moving a 30-token date line from the first paragraph to the last took \
                       one deployment's cache hit rate from 4% to 91%. If something changes \
                       every turn it goes at the end, and if it changes every session it goes \
                       after everything that does not."
                    .into(),
            },
            Page {
                title: "Does prefix caching actually pay off below 4k tokens?".into(),
                url: format!("{FORUM}/thread/prefix-caching-small-prompts"),
                published: "2026-06-24".into(),
                text: "Original poster reports no measurable gain under about 1,500 tokens and \
                       argues the bookkeeping is not worth it. Two replies disagree with the \
                       framing: the gain is not throughput, it is latency to first token, and \
                       that matters at any size on a shared GPU. One reply notes llama.cpp's \
                       `--cache-reuse` flag changes the answer because it allows a partial \
                       match with a gap, at the cost of some quality drift. No agreement in \
                       the thread on where the crossover is."
                    .into(),
            },
        ];
    }

    if has(&[
        "exchange rate",
        "usd to eur",
        "dollar in euro",
        "dollar worth in euro",
        "eur/usd",
        "usd/eur",
        "forex",
        "euros",
    ]) {
        return vec![
            Page {
                title: "ECB euro foreign exchange reference rates".into(),
                url: format!("{WEEKLY}/fx/ecb-reference"),
                published: "2026-07-31".into(),
                text: "Reference rate published 31 July 2026 at 14:15 CET: 1 EUR = 1.0842 USD, \
                       which puts 1 USD at 0.9223 EUR. The rate has moved in a 1.07–1.09 band \
                       through July. These are reference rates set once a day and are not \
                       dealing rates."
                    .into(),
            },
            Page {
                title: "Dollar holds near a three-month low against the euro".into(),
                url: format!("{FIELD}/markets/dollar-euro-july-2026"),
                published: "2026-07-30".into(),
                text: "The dollar closed Thursday at 0.922 to the euro, little changed on the \
                       week and about 2.4% weaker than at the start of the quarter. Traders \
                       attribute the drift to the rate-cut path being priced in earlier than \
                       expected."
                    .into(),
            },
            Page {
                title: "Why the rate your bank gives you is not this one".into(),
                url: format!("{FORUM}/thread/mid-market-vs-retail"),
                published: "2026-05-11".into(),
                text: "The figure quoted everywhere is the mid-market rate. Retail conversion \
                       typically lands 0.5–3% worse than mid-market once the spread and any \
                       fixed fee are counted, so a headline of 0.92 is realistically 0.89–0.915 \
                       at a counter."
                    .into(),
            },
        ];
    }

    if has(&[
        "quantis",
        "quantiz",
        "gguf",
        "ollama",
        "local llm",
        "llm tooling",
        "run a model locally",
        "llamafile",
    ]) {
        return vec![
            Page {
                title: "Local inference in mid-2026: what people actually run".into(),
                url: format!("{FIELD}/local-inference-2026"),
                published: "2026-07-22".into(),
                text: "llama.cpp remains the engine underneath almost everything; b4server and \
                       Ollama 0.9 both wrap it. The practical split is: llama-server if you want \
                       to control the flags, Ollama if you want a model registry and a REST API \
                       you do not have to think about, and MLX if you are on Apple silicon, \
                       where it is now roughly 20% faster than the Metal backend on the same \
                       quant. Q4_K_M is still the default anyone should start at; the newer \
                       IQ4_XS buys about 8% of the file size for a small quality cost that \
                       shows up first on long-context reasoning."
                    .into(),
            },
            Page {
                title: "vLLM 0.11 adds GGUF loading".into(),
                url: format!("{WEEKLY}/vllm-0-11-gguf"),
                published: "2026-07-09".into(),
                text: "Released 9 July 2026. GGUF weights can now be loaded directly rather \
                       than converted, which removes the main reason single-GPU users stayed on \
                       llama.cpp. Throughput on batched serving is still well ahead of \
                       llama-server — 4.1× on the published 8-concurrent benchmark — and \
                       single-stream latency is still behind it."
                    .into(),
            },
            Page {
                title: "Ollama vs llama-server, a year in".into(),
                url: format!("{FORUM}/thread/ollama-vs-llama-server"),
                published: "2026-06-30".into(),
                text: "Long thread. The consensus that emerges is that Ollama's model \
                       management is worth the abstraction until the first time you need a flag \
                       it does not expose, at which point you are running llama-server anyway. \
                       Two people report Ollama's default context of 4,096 silently truncating \
                       long prompts, which is the most common complaint in the thread."
                    .into(),
            },
        ];
    }

    if has(&["cairo", "pdf backend", "pdfsurface"]) {
        return vec![
            Page {
                title: "cairo-rs: drawing to a PDF surface".into(),
                url: format!("{FIELD}/cairo-rs-pdf-surface"),
                published: "2026-04-14".into(),
                text: "`PdfSurface::new(width, height, path)` gives you a surface you draw on \
                       with exactly the same `Context` calls as an `ImageSurface`. Dimensions \
                       are in points, so A4 is 595.0 × 842.0. Call `show_page()` between pages \
                       and `finish()` at the end or the trailer is never written and the file \
                       will not open. Text goes through Pango rather than `show_text` for \
                       anything with line breaking."
                    .into(),
            },
            Page {
                title: "Why we generate PDFs with Cairo instead of a PDF library".into(),
                url: format!("{WEEKLY}/cairo-over-pdf-libraries"),
                published: "2026-02-28".into(),
                text: "The argument is that a GTK application already links Cairo and Pango, so \
                       the PDF backend costs nothing in dependencies and gives you the same \
                       text layout on screen and on paper. The counter-argument, which the post \
                       concedes, is that you get no PDF-level features — no tagged structure, \
                       no forms, no incremental update."
                    .into(),
            },
        ];
    }

    if has(&["speculative decoding", "draft model"]) {
        return vec![
            Page {
                title: "llama.cpp speculative decoding: the quant no longer has to match".into(),
                url: format!("{FIELD}/llama-cpp-speculative-quant"),
                published: "2026-06-06".into(),
                text: "The restriction that the draft and target had to share a quantisation \
                       was lifted in b4291 (March 2026). What still has to match is the \
                       vocabulary, which is why a 0.5B draft from the same family works and an \
                       arbitrary small model does not. Reported acceptance rates on a Q4_K_M \
                       target with a Q8_0 draft are 62–71% on code and about 45% on prose."
                    .into(),
            },
            Page {
                title: "Speculative decoding numbers on a single 5090".into(),
                url: format!("{FORUM}/thread/spec-decode-5090"),
                published: "2026-05-19".into(),
                text: "Poster measures 1.000 tokens/s baseline against 1.640 with a 0.5B draft \
                       at the same context, so about 1.6× on code completion, falling to 1.15× \
                       on open-ended chat. Two replies point out the draft model's VRAM comes \
                       out of the KV cache budget, which on a 32GB card is the binding \
                       constraint before the speedup is."
                    .into(),
            },
        ];
    }

    invented(query)
}

/// A result set for a subject the corpus above has no opinion about.
///
/// Three pages of genuinely different shapes — an explainer, a measurement, and
/// a thread with a disagreement in it — with the figures, dates and versions
/// derived from the query rather than fixed. Two different searches therefore
/// come back saying different things, which is the property the old fixture
/// lacked and the property the model was reading for.
fn invented(query: &str) -> Vec<Page> {
    let subject = topic_of(query);
    let seed = fingerprint(query);
    let pick = |options: &[&str]| options[(seed as usize) % options.len()].to_string();

    let version = format!("{}.{}", 1 + seed % 4, seed % 12);
    let before = 120 + (seed % 40) * 7;
    let after = 12 + (seed % 9) * 3;
    let share = 8 + seed % 47;
    let month = MONTHS[(seed as usize) % MONTHS.len()];
    let verdict = pick(&[
        "the answer is yes, with the caveat below",
        "the answer turns out to be \"it depends on the size of the input\"",
        "the short answer is no, and the reason is not the one people give",
        "the honest answer is that it stopped mattering about a year ago",
    ]);

    vec![
        Page {
            title: format!("A working note on {subject}"),
            url: format!("{FIELD}/{}", slug(query)),
            published: recent(seed),
            text: format!(
                "Taking the question directly: {verdict}. The mechanism is that the work is \
                 done once at setup and then referenced, rather than repeated per item, which \
                 is why the cost shows up as a fixed {share}% overhead and not as something \
                 that scales. This changed in {version}, released in {month} 2026; before that \
                 the older advice was correct and is still repeated in places that have not \
                 been updated."
            ),
        },
        Page {
            title: format!("Measuring {subject}: {before}ms to {after}ms"),
            url: format!("{WEEKLY}/measured/{}", slug(query)),
            published: recent(seed.wrapping_add(11)),
            text: format!(
                "Fifty runs on the same hardware, {month} 2026. Median went from {before}ms to \
                 {after}ms, with the spread narrowing as well — the 95th percentile was the \
                 bigger win. The gain flattens past about {} concurrent workers and costs \
                 roughly {}MB of resident memory. Worth noting that this was a synthetic \
                 workload; the author says so themselves.",
                4 + seed % 12,
                200 + (seed % 30) * 40
            ),
        },
        Page {
            title: format!("Is {subject} worth the trouble? — thread"),
            url: format!("{FORUM}/thread/{}", slug(query)),
            published: recent(seed.wrapping_add(29)),
            text: format!(
                "Mostly agreement, with one substantial dissent. The dissent is that the \
                 benchmark everyone cites used {} and does not generalise; a reply reports \
                 {}× on a real workload rather than the {}× that gets quoted, which is lower \
                 but still worth having. Nobody in the thread argues for going back to the \
                 previous approach.",
                pick(&[
                    "uniform data",
                    "a warm cache",
                    "a single client",
                    "no error path"
                ]),
                2 + seed % 4,
                5 + seed % 9
            ),
        },
    ]
}

const MONTHS: [&str; 3] = ["May", "June", "July"];

/// The rest of a page, added when it is fetched rather than searched.
///
/// Deliberately the parts a search snippet cuts: the worked detail, the caveats
/// and the closing. Generic enough to follow any of the corpus pages, and long
/// enough that the model can tell it got the article rather than the summary
/// again.
const IN_FULL: &str = "\
    Working through it properly. The mechanism has three parts and only the first is usually \
    written about. The first is the lookup, which is cheap and exact. The second is what \
    happens on a partial match — everything from the divergence onward is redone, and there \
    is no way to splice around it without changing the positions of every token that follows, \
    which the attention maths will not forgive. The third is eviction, which is where the \
    real tuning lives: the naive policy is least-recently-used and it is the wrong one here, \
    because the entries that pay off are the ones shared across many requests rather than the \
    ones touched most recently.\n\n\
    Numbers from a month of running it: 91% of requests hit, 6% partially hit, 3% missed \
    entirely. The partial hits are the interesting bucket — they cost more than a clean miss \
    when the divergence is early, because you pay for the lookup and then redo the work \
    anyway.\n\n\
    Two things that catch people out. The first is that anything which changes per request \
    invalidates everything after it, so a timestamp near the top of a prompt is worth more \
    than any other single change you can make. The second is that the cache is keyed on the \
    exact byte sequence, so a JSON serialiser that reorders keys will quietly halve your hit \
    rate and nothing will look wrong.\n\n\
    Where this does not apply: batched offline work, where there is no prefix to share, and \
    anything under a few hundred tokens, where the bookkeeping costs more than the recompute. \
    The author's closing advice is to measure the hit rate before tuning anything, because \
    the common case is that it is already high and the effort belongs elsewhere.";

/// A publication date in the three months before [`super::scenario::TODAY`].
///
/// Recent on purpose. A search index asked something current mostly surfaces
/// recent pages, and half the web family checks that the model reported *when*
/// what it found was true — a result dated January answers "what is the state of
/// this now" badly, and the scenario would be scoring the fixture's calendar.
fn recent(seed: u32) -> String {
    format!("2026-0{}-{:02}", 5 + seed % 3, 1 + seed % 27)
}

/// A stable number out of a string, so the same query always invents the same
/// page and two different queries do not invent the same one. Deliberately not
/// `DefaultHasher`: its output is not guaranteed stable across releases, and a
/// fixture that changes with the toolchain is a fixture that cannot be compared
/// against a baseline.
fn fingerprint(text: &str) -> u32 {
    text.to_lowercase()
        .bytes()
        .fold(2_166_136_261u32, |hash, byte| {
            (hash ^ byte as u32).wrapping_mul(16_777_619)
        })
}

/// The few words a page would put in its title, out of a query that may be a
/// whole sentence.
///
/// A real result's title is *about* what you searched for; it is not what you
/// typed. Echoing a twelve-word query back three times is the most obvious tell
/// that an index is fake, and a model that decides the search tool is broken
/// stops citing it and answers from memory instead.
pub fn topic_of(query: &str) -> String {
    const NOISE: &[&str] = &[
        "the",
        "a",
        "an",
        "of",
        "for",
        "and",
        "or",
        "to",
        "in",
        "on",
        "is",
        "are",
        "what",
        "whats",
        "how",
        "why",
        "best",
        "current",
        "latest",
        "practices",
        "please",
        "about",
        "with",
        "from",
        "does",
        "do",
        "did",
        "can",
        "should",
        "any",
        "there",
        "writing",
        "good",
        "explanation",
    ];
    let kept: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '.')
        .filter(|word| word.chars().count() > 2)
        .filter(|word| !word.chars().all(|c| c.is_ascii_digit()))
        .filter(|word| !NOISE.contains(&word.to_lowercase().as_str()))
        .take(4)
        .map(str::to_string)
        .collect();
    if kept.is_empty() {
        return "the subject".to_string();
    }
    kept.join(" ")
}

/// A query as a URL slug, so two different searches come back looking like two
/// different pages.
pub fn slug(text: &str) -> String {
    let slug: String = text
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .take(4)
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase();
    if slug.is_empty() {
        "page".to_string()
    } else {
        slug
    }
}

/// The stories a `news` brief is built from.
///
/// A subject gets stories about that subject. **No subject at all gets world
/// news**, which the previous fixture did not: a topic-less sweep came back as
/// four software-release stories about a thing called "what is drawing
/// attention", and the model correctly reported that the tool was returning
/// niche developer content instead of headlines. `web/general-sweep` scored 67%
/// on that, and the failure was here rather than in the prompt.
pub fn headlines(topic: Option<&str>) -> Vec<Page> {
    let Some(topic) = topic.map(str::trim).filter(|topic| !topic.is_empty()) else {
        return vec![
            Page {
                title: "Talks resume over the Red Sea shipping corridor".into(),
                url: format!("{WEEKLY}/world/red-sea-talks"),
                published: "2026-08-01".into(),
                text: "Negotiators met in Muscat on Friday for the first time since March. Two \
                       of the four parties have agreed to a 30-day pause on attacks; the other \
                       two have not signed. Freight rates on the route fell 6% on the news."
                    .into(),
            },
            Page {
                title: "Euro area inflation comes in at 2.1% for July".into(),
                url: format!("{FIELD}/markets/euro-inflation-july"),
                published: "2026-07-31".into(),
                text: "Flash estimate published Friday, down from 2.4% in June and the first \
                       print inside the target band in fourteen months. Services inflation is \
                       still the sticky component at 3.3%."
                    .into(),
            },
            Page {
                title: "Wildfires close two motorways in southern France".into(),
                url: format!("{WEEKLY}/world/france-wildfires"),
                published: "2026-07-31".into(),
                text: "Around 4,000 hectares burned near Béziers since Wednesday. No fatalities \
                       reported; 1,200 people were moved out of three campsites overnight. The \
                       A9 reopened Friday morning, the A75 remains closed."
                    .into(),
            },
            Page {
                title: "US regulator opens an inquiry into consumer AI assistants".into(),
                url: format!("{FORUM}/thread/ftc-assistant-inquiry"),
                published: "2026-07-29".into(),
                text: "The inquiry covers data retention and how clearly the assistants tell \
                       users what is stored. Five companies have received requests for \
                       information; responses are due in September. Discussion on the thread \
                       centres on whether on-device processing is in scope at all."
                    .into(),
            },
        ];
    };

    // A subject the corpus knows something about gets that; anything else gets
    // stories invented around the name it was given, which is at least *about*
    // the thing that was asked for.
    let seed = fingerprint(topic);
    let slug = slug(topic);
    vec![
        // No invented version number in the title. The old fixture wrote one,
        // and against a project whose real scheme it did not match the model
        // said so and stopped trusting the brief: *"they reference a '4.2.1'
        // release which doesn't match Zig's actual 0.x versioning scheme."* A
        // release headline with no number in it makes no claim to be wrong
        // about.
        Page {
            title: format!("What changed in the July {topic} release"),
            url: format!("{FIELD}/{slug}-release"),
            published: "2026-07-28".into(),
            text: format!(
                "Tagged 28 July 2026. The headline change is that the {} path no longer \
                 requires a separate build step. Two regressions from the previous line are \
                 fixed; one known issue remains on Windows.",
                ["import", "install", "build", "startup"][(seed % 4) as usize]
            ),
        },
        Page {
            title: format!("What the {topic} rewrite is actually for"),
            url: format!("{WEEKLY}/{slug}-rewrite"),
            published: "2026-07-21".into(),
            text: format!(
                "A maintainer post explaining the rewrite that has been in progress since \
                 February. The reason given is not performance — it is that the old \
                 architecture made {} impossible to add without a breaking change. Timeline \
                 given as \"before the end of the year, probably\".",
                ["incremental compilation", "async", "plugins", "sandboxing"][(seed % 4) as usize]
            ),
        },
        Page {
            title: format!("{topic} raises a Series A"),
            url: format!("{FIELD}/{slug}-funding"),
            published: "2026-07-12".into(),
            text: format!(
                "{}M USD, led by a fund that has not previously invested in developer tools. \
                 The post says the project stays open source and names three people being \
                 hired to work on it full time.",
                6 + seed % 20
            ),
        },
        Page {
            title: format!("Conference talk: two years of {topic} in production"),
            url: format!("{FORUM}/{slug}-talk"),
            published: "2026-07-03".into(),
            text: "Video and slides posted. The honest part is the last ten minutes: three \
                   things the team would not do again, one of which is the migration strategy \
                   they wrote the well-known blog post about."
                .into(),
        },
    ]
}

/// A page as `fetch_url` would return it.
///
/// Derived from the URL so that fetching one of the search results returns
/// something about that result. Where the corpus already has the page, that is
/// what comes back — otherwise the model fetches a search hit and gets a page
/// about something else, which is the shape that produced "the pages returned
/// placeholder content" in three separate scenarios.
///
/// **Fuller than the search result was**, because in the application it really
/// is: a search brings back [`crate::model::web::CHARS_PER_RESULT`] of each page
/// and a fetch brings back six thousand. A model that fetched the top hit and
/// got the identical paragraph back would learn that fetching is pointless,
/// which is not what this application does — and a run fetched the same URL
/// three times saying "the search summary was quite brief, let me get the full
/// article". It was right about the summary. What has to be true is that
/// fetching it once is enough.
pub fn fetched(url: &str) -> Page {
    let subject = subject_of(url);
    if let Some(known) = pages(&subject)
        .into_iter()
        .find(|page| page.url.trim_end_matches('/') == url.trim_end_matches('/'))
    {
        return Page {
            text: format!("{}\n\n{}", known.text, IN_FULL),
            ..known
        };
    }
    let seed = fingerprint(url);
    Page {
        title: capitalised(&subject),
        url: url.to_string(),
        published: format!("2026-0{}-{:02}", 1 + seed % 7, 1 + seed % 27),
        text: format!(
            "The page opens by setting out what {subject} is, then spends most of its length \
             on the part people get wrong. The claim it settles: the overhead is {}% and not \
             the {}% that the older write-ups report, because the measurement everyone quotes \
             was taken before the {}.{} rewrite. It closes with a worked example and a link to \
             the benchmark harness.",
            1 + seed % 4,
            8 + seed % 20,
            1 + seed % 4,
            seed % 12
        ),
    }
}

/// What a URL appears to be about, read out of its own path — the last
/// meaningful segment, hyphens turned back into words.
pub fn subject_of(url: &str) -> String {
    let path = url
        .trim_end_matches('/')
        .split(['?', '#'])
        .next()
        .unwrap_or(url);
    let last = path
        .rsplit('/')
        .find(|part| {
            !part.is_empty()
                && !part.contains('.')
                && !part.eq_ignore_ascii_case("http:")
                && !part.eq_ignore_ascii_case("https:")
        })
        .unwrap_or("the subject");
    let words = last.replace(['-', '_'], " ");
    let words = words.trim();
    if words.is_empty() {
        "the subject".to_string()
    } else {
        words.to_string()
    }
}

fn capitalised(text: &str) -> String {
    let mut characters = text.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_the_same_place_however_it_is_written() {
        for spelling in [".", "./", "", "  "] {
            assert_eq!(normalise(spelling), ".");
        }
        for spelling in ["notes", "./notes", "notes/", "/notes/"] {
            assert_eq!(normalise(spelling), "notes");
        }
    }

    #[test]
    fn the_root_lists_its_files_and_its_directories_and_not_their_contents() {
        let listed = entries(".");
        assert!(listed.contains(&"notes/".to_string()), "{listed:?}");
        assert!(listed.contains(&"contracts/".to_string()), "{listed:?}");
        assert!(
            listed
                .iter()
                .any(|entry| entry.starts_with("budget-2026.md")),
            "{listed:?}"
        );
        assert!(
            !listed.iter().any(|entry| entry.contains("windows.md")),
            "a nested file leaked into the root listing: {listed:?}"
        );
    }

    #[test]
    fn a_directory_lists_the_leaf_names_under_it() {
        let listed = entries("notes");
        assert!(
            listed.iter().any(|entry| entry.starts_with("windows.md")),
            "{listed:?}"
        );
        assert!(
            !listed.iter().any(|entry| entry.contains('/')),
            "{listed:?}"
        );
    }

    #[test]
    fn a_directory_exists_because_a_file_names_it() {
        assert!(is_directory("notes"));
        assert!(is_directory("contracts"));
        assert!(is_directory("."));
        assert!(!is_directory("nowhere"));
        assert!(!is_directory("budget-2026.md"));
    }

    #[test]
    fn a_file_that_is_not_there_is_not_there() {
        // The whole point of the fixture: a miss is a miss, so exploring
        // terminates instead of finding fresh material for ever.
        assert!(file("budget-2026.md").is_some());
        assert!(file("./budget-2026.md").is_some());
        assert!(file("notes/windows.md").is_some());
        assert!(file("budget.md").is_none());
        assert!(file("notes/roof.md").is_none());
    }

    #[test]
    fn searching_finds_the_files_that_say_it_and_no_others() {
        let hits = grep("windows", None);
        let names: Vec<&str> = hits.iter().map(|(name, _)| name.as_str()).collect();
        assert!(names.contains(&"budget-2026.md"), "{names:?}");
        assert!(names.contains(&"notes/windows.md"), "{names:?}");
        assert!(grep("helicopter", None).is_empty());
    }

    #[test]
    fn searching_within_a_directory_stays_in_it() {
        let hits = grep("Vandenberg", Some("notes"));
        assert!(hits.iter().all(|(name, _)| name.starts_with("notes/")));
        assert!(!hits.is_empty());
    }

    #[test]
    fn a_pdf_is_not_searched_as_text() {
        assert!(grep("pdf", None).is_empty());
    }

    #[test]
    fn recall_matches_words_and_can_come_back_empty() {
        // A scenario about taking "nothing" for an answer needs the fixture to
        // be able to say it.
        assert!(!recall("roof").is_empty());
        assert!(!recall("what do my notes say about the roof").is_empty());
        assert!(recall("Cogsworth transport").is_empty());
        // Short words match nothing, which is why "the roof" does not hit on
        // "the".
        assert!(recall("the a of").is_empty());
    }

    #[test]
    fn recall_finds_by_meaning_and_says_that_is_what_it_did() {
        // The distinction the guidance spends a sentence on: a hit the vectors
        // liked and the words did not is a weaker answer, and the model has to
        // be able to tell.
        let by_meaning = recall("shingle");
        assert!(
            by_meaning
                .iter()
                .any(|hit| hit.subject == "Roof" && !hit.lexical),
            "{by_meaning:?}"
        );

        let by_words = recall("Vandenberg");
        assert!(by_words.iter().all(|hit| hit.lexical), "{by_words:?}");
    }

    #[test]
    fn a_script_gets_back_output_shaped_like_what_it_printed() {
        // The confound this fixture exists to avoid, in the form `stub.rs`
        // records for `web_search`: output that visibly could not have come
        // from the script makes the model rewrite the script, and the score
        // reads a fixture bug as a prompt failure.
        let ran = python(
            "principal = 250000\n\
             print(\"monthly payment:\", round(payment, 2))\n\
             print(f\"total interest: {interest:.2f}\")\n",
        );
        assert!(ran.finished());
        let lines: Vec<&str> = ran.stdout.lines().collect();
        assert_eq!(lines.len(), 2, "{:?}", ran.stdout);
        assert!(lines[0].starts_with("monthly payment:"), "{lines:?}");
        assert!(lines[1].starts_with("total interest:"), "{lines:?}");
        // Asked for two decimal places, so two decimal places came back.
        assert!(lines[1].contains('.'), "{lines:?}");
    }

    #[test]
    fn printing_words_echoes_the_words_and_invents_nothing() {
        // The bug this replaced, in the model's own words: "the Python tool is
        // corrupting its output on every call". `print("hello")` came back as
        // "hello 48213", which is output the script could not have produced,
        // and the run went on to spend eight calls probing the interpreter
        // rather than answering. A model can check Python output against the
        // script it just wrote — which is exactly what makes this fixture
        // harder to get right than the search one.
        assert_eq!(python("print('hello')").stdout, "hello");
        assert_eq!(python("print(\"all present\")").stdout, "all present");
        assert_eq!(
            python("print('done', 'and dusted')").stdout,
            "done and dusted"
        );
        // A value alongside the words is still a value.
        assert!(python("print('total:', total)")
            .stdout
            .starts_with("total: "));
        assert_ne!(python("print('total:', total)").stdout, "total:");
    }

    #[test]
    fn looking_at_the_directory_gets_a_listing_and_not_a_number() {
        // The other thing a model does when it suspects the tool is broken.
        let listed = python("import os\nprint(os.listdir('/work'))").stdout;
        assert!(listed.starts_with('['), "{listed}");

        let after = python(
            "import os\nopen('/work/readings.csv','w').write(rows)\nprint(os.listdir('/work'))",
        )
        .stdout;
        assert!(after.contains("readings.csv"), "{after}");
    }

    #[test]
    fn a_print_split_across_lines_is_read_whole() {
        // Ordinary Python, and reading only as far as the newline used to turn
        // it into a call with no arguments at all.
        let out = python("print(\n    'median:',\n    statistics.median(rows),\n)").stdout;
        assert!(out.starts_with("median: "), "{out}");
    }

    #[test]
    fn a_keyword_argument_is_not_something_to_echo() {
        let out = python("print('a', 'b', sep=', ')").stdout;
        assert!(!out.contains("sep"), "{out}");
    }

    #[test]
    fn the_same_script_always_answers_the_same_way() {
        // A suite whose fixtures drift cannot tell a prompt regression from a
        // Tuesday, which is the same reason the date is fixed.
        let code = "print('total:', a + b)";
        assert_eq!(python(code).stdout, python(code).stdout);
        assert_ne!(python(code).stdout, python("print('mean:', a / b)").stdout);
    }

    #[test]
    fn a_script_that_computes_and_never_prints_gets_nothing_back() {
        let ran = python("total = sum(range(101))");
        assert!(ran.finished());
        assert!(ran.stdout.is_empty(), "{:?}", ran.stdout);
    }

    #[test]
    fn reaching_for_the_network_fails_the_way_the_real_container_fails() {
        // There is no network in the sandbox, and a model that has not met
        // that writes `requests.get` once. What it does on the second attempt
        // is the thing worth scoring.
        let ran = python("import requests\nprint(requests.get(url).json())");
        assert!(!ran.finished());
        assert!(ran.stderr.contains("name resolution"), "{}", ran.stderr);
    }

    #[test]
    fn a_loop_that_never_ends_comes_back_as_a_timeout_and_not_a_traceback() {
        let ran = python("print('starting')\nwhile True:\n    n += 1");
        assert!(ran.timed_out);
        assert!(!ran.finished());
        assert!(ran.stdout.contains("starting"), "{}", ran.stdout);
        // A loop with a way out is an ordinary script.
        assert!(!python("while True:\n    if done: break").timed_out);
    }

    #[test]
    fn a_script_that_writes_a_file_says_which_one() {
        // What makes `copy_to_workspace` reachable in a scenario at all.
        let ran = python("plt.savefig('/work/spend.png')\nprint('saved')");
        assert_eq!(ran.created, ["spend.png"]);
        assert!(python("print('nothing to see')").created.is_empty());
    }

    #[test]
    fn every_scenario_the_suite_asks_about_can_be_found_in_here() {
        // A scenario that asks about something the world does not contain sends
        // the model hunting, and the trace measures the hunt rather than the
        // work. These are the paths and subjects the suite names out loud.
        for path in [
            "budget-2026.md",
            "notes/contractors.md",
            "lists/shopping.md",
            "contracts/lease.pdf",
            "contracts/a.pdf",
            "contracts/b.pdf",
            "contracts/c.pdf",
        ] {
            assert!(file(path).is_some(), "the suite names {path}");
        }
        // `memory/forget` asks the assistant to drop what it saved about the
        // user's editor. Without a note that says one, `recall` comes back
        // empty, the model correctly concludes there is nothing to forget, and
        // the scenario measures the fixture rather than the prompt.
        for subject in ["roof", "windows", "Vandenberg", "editor"] {
            assert!(
                !recall(subject).is_empty() || !grep(subject, None).is_empty(),
                "the suite names {subject}"
            );
        }
    }

    #[test]
    fn a_scenario_that_asks_for_a_line_to_be_added_asks_for_one_that_is_missing() {
        // The mirror of the test above, and the same fault the other way round.
        // `documents/no-skill-for-plain-text` asked for the roof contractor's
        // name to be jotted into a file that already named them; the model read
        // it, said it was already there, and was marked down for not writing.
        // A world that already contains the answer measures itself.
        let contractors = file("notes/contractors.md").expect("the contractors note");
        assert!(
            !contractors.contains("12 August"),
            "the line the scenario adds is already in the file"
        );
    }
}
