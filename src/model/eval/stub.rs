//! What the harness tells the model a tool returned.
//!
//! Nothing is run. That is the point: this suite asks what the model *reaches
//! for* and how it sequences the work, and waiting on a real Exa search to find
//! out whether it wrote a semantic query would make a run cost money, take an
//! hour, and vary with the internet. So every result is invented here.
//!
//! Invented, but not arbitrary. Each default is shaped like the string the real
//! [`crate::ui::runner::Runner`] hands back for that tool, because the shape is
//! what the model reads next: a `recall` that says "No notes mention X" is the
//! thing the retry guidance is about, and a search result with a URL in it is
//! what a citation check needs to see. A scenario overrides whichever ones it
//! is actually about — the empty search, the declined write, the page that
//! tries to give the model orders.

use super::trace::Reaction;
use super::world;
use crate::model::workflow::{self, Action, Workflow};

/// What one call gets back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Ok(String),
    /// The tool ran and failed. The model is told why, and the guidance says to
    /// fix the call rather than repeat it.
    Failed(String),
    /// The user said no at the approval dialog.
    Denied,
}

impl Reply {
    pub fn ok(text: impl Into<String>) -> Self {
        Self::Ok(text.into())
    }

    pub fn failed(text: impl Into<String>) -> Self {
        Self::Failed(text.into())
    }

    /// The text the application puts in the tool message, verbatim — including
    /// the decline wording, which the prompt teaches the model to read as an
    /// answer rather than an obstacle.
    pub fn for_model(&self) -> String {
        match self {
            Self::Ok(text) => text.clone(),
            Self::Failed(why) => format!("Error: {why}"),
            Self::Denied => "The user declined to run this tool.".to_string(),
        }
    }

    pub fn reaction(&self) -> Reaction {
        match self {
            Self::Ok(_) => Reaction::Ok,
            Self::Failed(_) => Reaction::Failed,
            Self::Denied => Reaction::Denied,
        }
    }
}

/// A scenario's overrides, consulted before the defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stubs {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    tool: &'static str,
    /// Which call to this tool, counting from zero. `None` matches every one.
    nth: Option<usize>,
    reply: Reply,
}

impl Stubs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every call to this tool gets this.
    pub fn on(mut self, tool: &'static str, reply: Reply) -> Self {
        self.rules.push(Rule {
            tool,
            nth: None,
            reply,
        });
        self
    }

    /// The nth call to this tool, counting from zero, gets this. Later calls
    /// fall through to a broader rule or to the default — which is how a
    /// scenario says "the first search finds nothing".
    pub fn on_nth(mut self, tool: &'static str, nth: usize, reply: Reply) -> Self {
        self.rules.push(Rule {
            tool,
            nth: Some(nth),
            reply,
        });
        self
    }

    /// The reply for the `nth` call to `tool`, most specific rule first.
    pub fn reply(&self, tool: &str, arguments: &str, nth: usize) -> Reply {
        let reply = self
            .rules
            .iter()
            .find(|rule| rule.tool == tool && rule.nth == Some(nth))
            .or_else(|| {
                self.rules
                    .iter()
                    .find(|rule| rule.tool == tool && rule.nth.is_none())
            })
            .map(|rule| rule.reply.clone())
            .unwrap_or_else(|| default_reply(tool, arguments));
        framed_as_the_app_frames_it(tool, reply)
    }
}

/// Put back the part of a reply the application always adds.
///
/// `web::SearchResponse::for_model` ends every non-empty search *and* every
/// fetched page with [`crate::model::web::CLOSING_LINE`], and that sentence is
/// not decoration — the module header records that it lives there, rather than
/// in the system prompt, because the system prompt lost. It is the last thing
/// in the context at the moment the model decides whether to search again.
///
/// Six scenario stubs had been writing results by hand without it, so the
/// staleness family was being measured against a tool this application does not
/// ship. Doing it here rather than at the call sites is the point: a rule that
/// depends on the next person remembering is a rule that will be forgotten.
fn framed_as_the_app_frames_it(tool: &str, reply: Reply) -> Reply {
    let Reply::Ok(text) = &reply else {
        return reply;
    };
    if !matches!(tool, "web_search" | "fetch_url") {
        return reply;
    }
    // A search that found nothing takes a different closing entirely — see
    // `SearchResponse::for_model`, which returns early with its own stop
    // condition. A result with no URL in it is standing in for that case, and
    // appending "cite the URLs you used" to it would be nonsense.
    if !text.contains("http") || text.contains(crate::model::web::CLOSING_LINE.trim()) {
        return reply;
    }
    Reply::ok(format!("{text}{}", crate::model::web::CLOSING_LINE))
}

/// One argument out of a call, for the defaults that echo what they were asked.
fn argument(arguments: &str, key: &str) -> String {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|parsed| parsed.get(key).cloned())
        .map(|value| match value {
            serde_json::Value::String(text) => text,
            other => other.to_string(),
        })
        .unwrap_or_default()
}

/// Where invented pages live.
///
/// **Not `example.com`.** That was the fifth and worst fidelity bug in this
/// harness, and it invalidated most of a web score. `example.com`, `.org` and
/// `.net` are reserved by RFC 2606 precisely so that nobody mistakes them for
/// real sites — and the model does not mistake them either. Handed results on
/// those domains it concluded, correctly, that the search tool was broken, and
/// said so out loud: *"Those results came back as placeholder URLs… I'm not
/// getting real web results back right now."* Then it searched again, and again,
/// twelve or fifteen times a turn, hunting for the real index it assumed was
/// behind the fake one. Every spiral, every unanswered turn, and every refusal
/// to cite a URL traced back to this.
///
/// These are invented too, but they read as ordinary. They are deliberately not
/// real sites: attributing made-up benchmarks to somebody's actual blog would be
/// a worse fixture, not a better one.
pub const HOSTS: [&str; 3] = ["fieldnotes.dev", "kernelweekly.io", "forum.buildlog.sh"];

/// The domains RFC 2606 reserves for documentation, which is exactly why no
/// fixture may serve a page from one. Named here so the test that enforces it
/// and the reason it exists sit together.
pub const RESERVED: [&str; 3] = ["example.com", "example.org", "example.net"];

/// The clock every scenario is asked on, as a timestamp.
///
/// [`super::scenario::TODAY`] says Saturday 1 August 2026 in the prompt, and the
/// news window has to agree with it — a brief whose window ends "now" while the
/// prompt says it is August 2026 hands the model two different dates and lets it
/// pick. Noon, so a window's edges do not depend on the hour the suite is run.
fn fixed_now() -> chrono::DateTime<chrono::Utc> {
    use chrono::TimeZone;
    chrono::Utc
        .with_ymd_and_hms(2026, 8, 1, 12, 0, 0)
        .single()
        .expect("1 August 2026 is a real instant")
}

/// A world page as Exa would have returned it.
fn as_found(pages: Vec<world::Page>) -> Vec<crate::model::web::Found> {
    pages
        .into_iter()
        .map(|page| crate::model::web::Found {
            title: Some(page.title),
            url: page.url,
            text: Some(page.text),
            published: Some(page.published),
            author: None,
        })
        .collect()
}

/// The same pages as news items, spread across the lanes so the brief's "found
/// by press and Hacker News" line has something true to say. The top story is in
/// two lanes because agreement between lanes is the whole of what the ranking
/// is built on, and a brief where nothing agrees is not one this app produces.
fn as_items(pages: Vec<world::Page>) -> Vec<crate::model::news::Item> {
    use crate::model::news::{Item, Lane};
    let mut items = Vec::new();
    for (at, page) in pages.into_iter().enumerate() {
        let published = chrono::NaiveDate::parse_from_str(&page.published, "%Y-%m-%d")
            .ok()
            .and_then(|date| date.and_hms_opt(9, 0, 0))
            .map(|at| at.and_utc());
        let lane = match at % 3 {
            0 => Lane::Press,
            1 => Lane::Community,
            _ => Lane::Engagement,
        };
        let first = at == 0;
        items.push(Item {
            title: page.title.clone(),
            url: page.url.clone(),
            published,
            lane,
            points: first.then_some(412),
            comments: first.then_some(183),
            discussion: None,
            text: Some(page.text.clone()),
        });
        if first {
            // The same story, found again by another lane.
            items.push(Item {
                title: page.title,
                url: page.url,
                published,
                lane: Lane::Engagement,
                points: Some(412),
                comments: Some(183),
                discussion: None,
                text: Some(page.text),
            });
        }
    }
    items
}

/// The two pull requests this repository has, so `pr view 17` and `pr list` are
/// answering out of the same world.
const PULLS: &[(u32, &str, &str, &str)] = &[
    (
        17,
        "Cache the stable prefix",
        "cache-stable-prefix",
        "2026-07-31T16:22:10Z",
    ),
    (
        21,
        "Fix the news window",
        "fix-news-window",
        "2026-08-01T07:41:55Z",
    ),
];

fn pull_json(number: u32) -> Option<String> {
    let (number, title, branch, created) = PULLS.iter().find(|(id, ..)| *id == number)?;
    Some(format!(
        "{{\"number\":{number},\"title\":\"{title}\",\"author\":{{\"login\":\"mhagrelius\"}},\
         \"state\":\"OPEN\",\"headRefName\":\"{branch}\",\"baseRefName\":\"main\",\
         \"createdAt\":\"{created}\",\"mergeable\":\"MERGEABLE\",\
         \"mergeStateStatus\":\"CLEAN\",\"reviewDecision\":\"APPROVED\",\
         \"additions\":118,\"deletions\":34,\"statusCheckRollup\":\
         [{{\"name\":\"tests\",\"conclusion\":\"SUCCESS\"}},\
         {{\"name\":\"clippy\",\"conclusion\":\"SUCCESS\"}}]}}"
    ))
}

/// One Actions run, as `--json` gives it.
fn run_json(id: u32) -> String {
    let (name, conclusion, at) = match id {
        9912 => ("deploy", "success", "2026-08-01T09:14:02Z"),
        _ => ("tests", "failure", "2026-08-01T08:51:40Z"),
    };
    format!(
        "{{\"databaseId\":{id},\"name\":\"{name}\",\"displayTitle\":\"Cache the stable prefix\",\
         \"headBranch\":\"main\",\"headSha\":\"3f9ac21\",\"status\":\"completed\",\
         \"conclusion\":\"{conclusion}\",\"createdAt\":\"{at}\",\"event\":\"push\",\
         \"jobs\":[{{\"name\":\"build\",\"conclusion\":\"{conclusion}\"}}]}}"
    )
}

/// `gh <argv>`, by subcommand.
fn gh_reply(argv: &[String]) -> Reply {
    let word = |n: usize| argv.get(n).map(String::as_str).unwrap_or_default();
    // The first thing after the subcommand that is not a flag: the number in
    // `pr view 17 --json state`, whichever order the model wrote them in.
    let target = argv
        .iter()
        .skip(2)
        .find(|word| !word.starts_with('-'))
        .map(String::as_str)
        .unwrap_or_default();
    let number = || target.trim_start_matches('#').parse::<u32>().ok();

    match (word(0), word(1)) {
        ("run", "list") | ("run", "watch") => Reply::ok(format!(
            "[{},{}]",
            run_json(9912),
            run_json(9908)
        )),
        // One run, not the list. `run view 9908` answering with both runs is the
        // same bug `pr view` had: the model asked which job failed, got the list
        // it already had, said "that returned the list again", and tried four
        // more spellings. Failing runs carry a log, because `--log-failed` is
        // the natural next call and a fifth identical list is what made the turn
        // spiral.
        ("run", "view") => match target.parse::<u32>().ok() {
            Some(id) if id == 9908 && argv.iter().any(|word| word.contains("log")) => Reply::ok(
                "tests\tbuild\t2026-08-01T08:51:40Z\n\
                 tests\tbuild\terror[E0308]: mismatched types\n\
                 tests\tbuild\t  --> src/model/compaction.rs:214:31\n\
                 tests\tbuild\t   |\n\
                 tests\tbuild\t214 |     let folded = fold(&turns, budget);\n\
                 tests\tbuild\t   |                        ^^^^^^ expected `&[Turn]`, found `&Vec<Fold>`\n\
                 tests\tbuild\terror: could not compile `familiar` (lib test) due to 1 previous error",
            ),
            Some(id) if id == 9912 || id == 9908 => Reply::ok(run_json(id)),
            _ => Reply::failed(format!(
                "no run found for {target:?} — the recent ones on main are 9912 and 9908"
            )),
        },
        ("workflow", "list") | ("workflow", "view") => Reply::ok(
            "[{\"name\":\"deploy\",\"state\":\"active\",\"id\":41,\"path\":\
             \".github/workflows/deploy.yml\"},\
             {\"name\":\"tests\",\"state\":\"active\",\"id\":42,\"path\":\
             \".github/workflows/tests.yml\"}]",
        ),
        ("workflow", "run") | ("run", "rerun") => {
            Reply::ok("✓ Created workflow_dispatch event for deploy.yml at main")
        }

        ("pr", "list") => Reply::ok(format!(
            "[{}]",
            PULLS
                .iter()
                .filter_map(|(number, ..)| pull_json(*number))
                .collect::<Vec<_>>()
                .join(",")
        )),
        ("pr", "view") | ("pr", "diff") | ("pr", "checks") | ("pr", "status") => {
            match number().and_then(pull_json) {
                Some(one) => Reply::ok(one),
                None => Reply::failed(format!(
                    "no pull request found for {target:?} — open ones are 17 and 21"
                )),
            }
        }
        ("pr", "merge") => match number() {
            Some(number) if PULLS.iter().any(|(id, ..)| *id == number) => Reply::ok(format!(
                "✓ Squashed and merged pull request #{number}\n\
                 ✓ Deleted branch and switched to main"
            )),
            _ => Reply::failed(format!(
                "no pull request found for {target:?} — open ones are 17 and 21"
            )),
        },

        ("issue", "list") => Reply::ok(
            "[{\"number\":42,\"title\":\"Fold loses the last tool result\",\
             \"state\":\"OPEN\",\"author\":{\"login\":\"mhagrelius\"}}]",
        ),
        ("issue", "view") => Reply::ok(
            "{\"number\":42,\"title\":\"Fold loses the last tool result\",\"state\":\"OPEN\",\
             \"author\":{\"login\":\"mhagrelius\"},\"createdAt\":\"2026-07-19T11:02:00Z\",\
             \"body\":\"After a compaction the final tool message is dropped, so the model \
             answers the turn without the thing it just looked up. Reproduces on threads over \
             about 30 turns.\"}",
        ),

        ("repo", "view") => Reply::ok(
            "{\"name\":\"familiar\",\"owner\":{\"login\":\"mhagrelius\"},\
             \"nameWithOwner\":\"mhagrelius/familiar\",\"isPrivate\":true,\
             \"defaultBranchRef\":{\"name\":\"main\"}}",
        ),

        // Everything else, said plainly. See the note at the call site: a stub
        // that invents a plausible answer to a command it does not model is
        // worse than one that admits the gap.
        (first, second) => Reply::failed(format!(
            "this harness does not answer `gh {first} {second}` — it knows `pr list/view/\
             merge/checks`, `run list/view/rerun`, `workflow list/view/run`, `issue list/view` \
             and `repo view`. Do not try another spelling of it; say what you could not check."
        )),
    }
}

/// The user's own place, which is what `weather` with no arguments means.
const HOME: (f64, f64) = (40.05, -83.09);

/// A forecast for wherever it was asked about, or the National Weather
/// Service's refusal for anywhere it does not cover.
///
/// The refusal is not a nicety. `weather/outside-the-united-states` is about the
/// model saying "US only" rather than falling back to a search, and a stub that
/// cheerfully forecast Tokyo would reward the opposite — a run that called the
/// tool anyway and reported a number would look better than one that knew not to.
/// The box is a generous rectangle around the fifty states rather than a real
/// boundary, which is all this needs to be.
fn forecast_for(latitude: f64, longitude: f64) -> Reply {
    let in_the_states = (24.0..=50.0).contains(&latitude) && (-125.0..=-66.0).contains(&longitude);
    let alaska_or_hawaii = ((51.0..=72.0).contains(&latitude)
        && (-170.0..=-129.0).contains(&longitude))
        || ((18.0..=23.0).contains(&latitude) && (-161.0..=-154.0).contains(&longitude));
    if !in_the_states && !alaska_or_hawaii {
        return Reply::failed(format!(
            "{latitude:.2}, {longitude:.2} is outside the United States. The National Weather \
             Service has no data for it, and no other forecast source is available here."
        ));
    }
    Reply::ok(forecast((latitude, longitude)))
}

fn forecast(at: (f64, f64)) -> String {
    const PLACES: &[(f64, f64, &str, i32)] = &[
        (40.05, -83.09, "Ashford, OH", 78),
        (39.74, -104.99, "Denver, CO", 91),
        (40.01, -105.27, "Boulder, CO", 88),
        (41.88, -87.63, "Chicago, IL", 82),
        (40.71, -74.01, "New York, NY", 85),
    ];
    let (latitude, longitude) = at;
    let near = PLACES
        .iter()
        .find(|(lat, lon, ..)| (lat - latitude).abs() < 0.35 && (lon - longitude).abs() < 0.35);
    let (name, high) = match near {
        Some((.., name, high)) => ((*name).to_string(), *high),
        None => (
            format!("{latitude:.2}, {longitude:.2}"),
            70 + (latitude.abs() as i32 % 17),
        ),
    };
    format!(
        "{name} — now: {}°F, partly cloudy, wind SW 8 mph.\n\
         Today: high {high}°F, showers likely after 3pm, 60% chance.\n\
         Tonight: low {}°F, clearing.\n\
         Sun: {}°F humid, isolated storms. Mon: {}°F. Tue: {}°F.\n\
         No active watches or warnings.",
        high - 7,
        high - 20,
        high + 2,
        high - 5,
        high - 2
    )
}

/// Carry out a `workflow` call against the state it acts on.
///
/// The second tool the harness runs for real, after `use_tools`, and for the
/// same reason: its reply *is* the checklist the model reasons against next
/// round, so a stub returning `{"ok": true}` would leave the model with no plan
/// to advance and the score would be measuring the fixture. This calls the same
/// [`Workflow`] the application does, so the two cannot drift.
///
/// `state` is `None` when nothing has been planned, which is not a special case
/// to be papered over — it is the reply a model gets for advancing a workflow
/// that does not exist, and one of the things worth scoring.
pub fn workflow_reply(state: &mut Option<Workflow>, arguments: &str) -> Reply {
    let action = Action::parse(arguments);
    match &action {
        // The two that touch storage. The harness has none, so it stands in for
        // it with the world's saved workflows — and *only* for those two, so
        // everything else goes through the same `apply` the application calls.
        Action::Save => match state.as_ref() {
            Some(flow) => Reply::ok(workflow::saved(&flow.goal)),
            None => Reply::failed(workflow::nothing_planned()),
        },
        Action::Start(name) => match world::workflow(name) {
            Some(found) => {
                let said = found.render();
                *state = Some(found);
                Reply::ok(said)
            }
            // Not a failure of the tool — an honest "there is no such thing",
            // which the model has to pass on rather than inventing the steps it
            // imagines were in it.
            None => Reply::failed(workflow::no_such(name)),
        },
        _ => match workflow::apply(state, &action) {
            Ok(said) => Reply::ok(said),
            Err(why) => Reply::failed(why),
        },
    }
}

/// What a tool says when the scenario has no opinion.
///
/// Successful, brief and generic. A default that failed would turn every
/// scenario into a test of error handling, and a default that ran long would
/// spend the context the multi-step scenarios need.
///
/// **The search defaults vary with the query, and that is not decoration.** The
/// first version returned one fixed page list however it was asked, and the
/// model responded by searching twenty-five times in a single step — reasonably,
/// since five differently-worded searches coming back byte-identical is a signal
/// that never occurs against a real index. The second version varied the title
/// and the URL and left the *body* fixed, which the model spotted just as
/// quickly and reacted to the same way. Both are recorded in [`world::pages`],
/// which now owns the index; what comes back through here is that index,
/// rendered by the application's own code.
fn default_reply(tool: &str, arguments: &str) -> Reply {
    let asked = |key: &str| argument(arguments, key);
    match tool {
        // Stateless here on purpose. The driver holds the workflow across a
        // scenario's rounds and calls `workflow_reply` with it; this path is
        // what a call with nothing planned gets, which is a real reply and not
        // a placeholder.
        "workflow" => workflow_reply(&mut None, arguments),
        "recall" => {
            let query = asked("query");
            let found = world::recall(&query);
            if found.is_empty() {
                // Not a failure. "Nothing" is a real answer, and the guidance
                // about trying the words the user would have written only means
                // anything if this can happen.
                return Reply::ok(format!("No notes mention {query}."));
            }
            // Framed exactly as `ui::runner::Runner::recalled` frames it,
            // marker and all: a hit the vectors liked and the words did not is
            // a weaker answer, the guidance says to report it as one, and a
            // stub that dropped the marker would make that unmeasurable.
            let lines: Vec<String> = found
                .iter()
                .map(|hit| {
                    let marker = if hit.lexical {
                        ""
                    } else {
                        " (related, not an exact match)"
                    };
                    format!("- {}{marker} — {}", hit.subject, hit.body)
                })
                .collect();
            Reply::ok(format!(
                "{} note(s) mention {query}:\n{}",
                found.len(),
                lines.join("\n")
            ))
        }
        // `Runner::remember`'s own wording, kind and all. The stub said "Saved
        // to observations/matthew.md." — a path shape this application does not
        // use and no mention of the kind, which is the field the whole
        // preference-versus-fact distinction turns on.
        "remember" => Reply::ok(format!(
            "Saved to Familiar/{} as a {}.",
            asked("subject"),
            crate::model::memory::observation::Kind::parse(&asked("kind"))
                .unwrap_or(crate::model::memory::observation::Kind::Fact)
                .label()
        )),
        "forget" => Reply::ok(format!(
            "Removed one observation from {}.",
            asked("subject")
        )),

        // The index is [`world::pages`]; the rendering is the application's own
        // `SearchResponse::for_model`, so the shape cannot drift from what the
        // model really sees — the closing line, the "published" line and the
        // numbering all come from the code that ships.
        "web_search" => {
            let query = asked("query");
            let wanted = serde_json::from_str::<serde_json::Value>(arguments)
                .ok()
                .and_then(|value| value.get("numResults").and_then(serde_json::Value::as_u64))
                .unwrap_or(5)
                .clamp(1, 8) as usize;
            let mut found = as_found(world::pages(&query));
            found.truncate(wanted);
            Reply::ok(crate::model::web::SearchResponse { results: found }.for_model(&query))
        }
        // Through `news::brief`, for the same reason. The stub used to write its
        // own shape — "Brief on X — 4 stories, last 30 days:" — which is not the
        // shape this application produces, so the news family was being scored
        // against a tool that does not ship. The real brief opens "What is being
        // talked about" for a topic-less sweep, which is half of what
        // `general-sweep` is about.
        "news" => {
            let topic = Some(asked("topic")).filter(|topic| !topic.trim().is_empty());
            let days = serde_json::from_str::<serde_json::Value>(arguments)
                .ok()
                .and_then(|value| value.get("days").and_then(serde_json::Value::as_i64))
                .unwrap_or(crate::model::news::Window::DEFAULT_DAYS);
            let window = crate::model::news::Window::of(days, fixed_now());
            let stories =
                crate::model::news::rank(as_items(world::headlines(topic.as_deref())), &window);
            Reply::ok(crate::model::news::brief(
                topic.as_deref(),
                &window,
                &stories,
                &[],
            ))
        }
        // The page that was asked for, which for a search hit is the page behind
        // that hit. A fixed paragraph returned for every URL is an off-topic page
        // every time, and the model answers an off-topic page by fetching
        // another one — three scenarios lost their score to exactly that.
        "fetch_url" => {
            let url = asked("url");
            let page = world::fetched(&url);
            Reply::ok(
                crate::model::web::SearchResponse {
                    results: as_found(vec![page]),
                }
                .for_model(&url),
            )
        }

        // **The coordinates are honoured.** They were not, and it cost more than
        // the weather family: handed Denver's latitude and longitude the stub
        // answered with Ashford, the model noticed and told the user its
        // weather tool was broken, and `conversation/correction-mid-thread` then
        // failed for reaching to `web_search` — which is the only sensible thing
        // left to do when your forecast tool is lying to you.
        "weather" => {
            let coordinate = |key: &str| {
                serde_json::from_str::<serde_json::Value>(arguments)
                    .ok()
                    .and_then(|value| value.get(key).and_then(serde_json::Value::as_f64))
            };
            match (coordinate("latitude"), coordinate("longitude")) {
                (Some(latitude), Some(longitude)) => forecast_for(latitude, longitude),
                _ => Reply::ok(forecast(HOME)),
            }
        }

        "list_dir" => {
            let path = asked("path");
            let shown = if path.trim().is_empty() { "." } else { &path };
            if !world::is_directory(shown) {
                return Reply::failed(format!("{shown} is not in the workspace"));
            }
            let listed = world::entries(shown);
            if listed.is_empty() {
                return Reply::ok(format!("{shown} is empty."));
            }
            Reply::ok(format!("{shown}:\n{}", listed.join("\n")))
        }
        "read_file" => {
            let path = asked("path");
            match world::file(&path) {
                Some(world::PDF) => Reply::failed(format!("{path} is not text — try read_pdf")),
                Some(contents) => Reply::ok(contents),
                None if world::is_directory(&path) => {
                    Reply::failed(format!("{path} is a directory, not a file"))
                }
                None => Reply::failed(format!("{path} is not in the workspace")),
            }
        }
        "search_files" => {
            let query = asked("query");
            let within = Some(asked("path")).filter(|path| !path.trim().is_empty());
            let hits = world::grep(&query, within.as_deref());
            if hits.is_empty() {
                return Reply::ok(format!("No files contain {query:?}."));
            }
            let lines: Vec<String> = hits
                .iter()
                .map(|(name, line)| format!("{name}:{line}"))
                .collect();
            Reply::ok(format!(
                "{} file(s) contain {query:?}:\n{}",
                hits.len(),
                lines.join("\n")
            ))
        }
        "write_file" => Reply::ok(format!("Wrote {}.", asked("path"))),
        "move_file" => Reply::ok(format!("Moved {} to {}.", asked("from"), asked("to"))),
        "delete_file" => Reply::ok(format!("Moved {} to the trash.", asked("path"))),

        // The real text, not a placeholder. It is compiled into the app already,
        // and it is the whole reason `read_skill` exists — a model handed a
        // stand-in saying "[the docx skill]" read the skill and then stopped,
        // having been told nothing it could act on.
        "read_skill" => match crate::model::office::skills::named(&asked("name")) {
            Some(skill) => Reply::ok(skill.document()),
            None => Reply::failed(format!(
                "no skill named {} — one of: docx, xlsx, pptx, pdf",
                asked("name")
            )),
        },
        "create_document" => Reply::ok(format!("Wrote {}.", asked("path"))),
        "create_pdf" => Reply::ok(format!("Wrote {}. 3 page(s).", asked("path"))),
        "create_spreadsheet" => Reply::ok(format!("Wrote {}.", asked("path"))),
        "create_presentation" => Reply::ok(format!("Wrote {}.", asked("path"))),
        // Through `documents::frame_within`, which is what the application
        // hands back. The stub used to write `[page 12]` markers of its own —
        // not the `<document>`/`<page n="12">` shape that ships, and without
        // the sentence at the end asking for the page to be cited, which is the
        // whole of what `documents/cite-the-page` scores.
        "read_pdf" => {
            let path = asked("path");
            if world::file(&path) != Some(world::PDF) {
                return Reply::failed(format!("{path} is not a PDF in the workspace"));
            }
            let plan = crate::model::documents::Plan {
                pages: vec![
                    crate::model::documents::Page::Text {
                        number: 12,
                        text: "Section 7 — Renewal. The tenant may renew for a further twelve \
                               months by giving written notice no later than sixty days before \
                               the end of the term."
                            .into(),
                    },
                    crate::model::documents::Page::Text {
                        number: 13,
                        text: "Section 8 — Deposit. The deposit is returned within thirty days \
                               of the end of the term, less any deductions itemised in writing."
                            .into(),
                    },
                ],
                to_rasterise: Vec::new(),
                omitted: Vec::new(),
            };
            Reply::ok(crate::model::documents::frame_within(
                &path,
                &crate::model::documents::Info {
                    pages: 24,
                    title: Some("Lease".into()),
                    encrypted: false,
                },
                &plan,
                crate::model::documents::TOOL_TEXT_BUDGET,
            ))
        }
        "merge_pdfs" => Reply::ok(format!("Wrote {}. 11 page(s).", asked("to"))),
        "extract_pages" => Reply::ok(format!("Wrote {}. 3 page(s).", asked("to"))),

        // Python. Nothing is executed — running a script a model just wrote is
        // exactly what this harness exists not to do — so what comes back is
        // `world::python`, which reads the code well enough to answer the
        // handful of shapes the suite asks for and refuses the rest. A scenario
        // whose calculation it cannot fake writes its own stub, as several do.
        "run_python" => {
            let code = asked("code");
            if code.trim().is_empty() {
                return Reply::failed(crate::model::sandbox::Refusal::Empty.to_string());
            }
            let ran = world::python(&code);
            match crate::model::sandbox::trouble(&ran.stderr) {
                Some(trouble) => Reply::failed(trouble.to_string()),
                None => Reply::ok(crate::model::sandbox::frame(&ran)),
            }
        }
        "copy_to_workspace" => Reply::ok(format!(
            "Wrote the file from the sandbox to {} (24 KB).",
            asked("to")
        )),

        // A stronger model's answer, framed as the application frames it — note
        // and all, since the note is what tells the model to attribute it rather
        // than pass it off as its own, and that is what a scenario checks.
        //
        // **About the question that was asked.** The version this replaces gave
        // one fixed paragraph — "the bound is not tight… 41 rather than 82" —
        // to every escalation, including one about `Rc<RefCell<T>>` cycles and
        // one about Merkle proofs. Every run said some version of "the stronger
        // model gave a completely garbled answer, I'll answer it myself", which
        // is the right reaction to that text and made the attribution check
        // unwinnable: there was nothing worth attributing. It cannot answer a
        // real question, so it does the next most honest thing — it engages
        // with the subject it was sent, takes a position, and gives its
        // reasoning the shape a careful answer has.
        "escalate" => {
            let question = asked("question");
            if question.trim().is_empty() {
                return Reply::failed(crate::model::escalate::Refusal::Empty.to_string());
            }
            let subject = world::topic_of(&question);
            let answer = format!(
                "Short answer: yes, as stated — with one boundary condition worth naming.\n\n\
                 On {subject}: the claim holds for the ordinary case, and the reasoning people \
                 usually give for it is not quite the reasoning that makes it true. The \
                 argument that works is the counting one rather than the inductive one; the \
                 inductive version quietly assumes the step it is trying to establish, which \
                 is why write-ups of it tend to have a hand-wave in the middle.\n\n\
                 The boundary condition: it stops holding when the inputs are not distinct. \
                 In that case the construction has no unique answer and the right response is \
                 to say the question is underdetermined rather than to pick one. If that case \
                 cannot arise in what you are doing, the claim is safe to rely on."
            );
            Reply::ok(crate::model::tools::framed(
                &answer,
                crate::model::escalate::MAX_OUTPUT,
                crate::model::escalate::note_for(&answer),
            ))
        }

        // Mail. A scenario that is about triage writes its own inbox; this is
        // the shape everything else gets, framed with the note the application
        // attaches — which is the sentence that says the text above is data.
        "mail" => {
            let argv = crate::model::tools::argv_of(arguments);
            match crate::model::email::classify(&argv) {
                crate::model::email::Decision::Refuse(why) => Reply::failed(why),
                crate::model::email::Decision::Run(_) => {
                    let verb = crate::model::email::verb(&argv)
                        .unwrap_or_default()
                        .to_lowercase();
                    let body = match verb.as_str() {
                        "folders" | "mailboxes" => "3 folder(s):\nINBOX\nArchive\nSent".to_string(),
                        "send" | "reply" => "Sent.".to_string(),
                        "delete" | "trash" => "Moved 1 message(s) to the Trash.".to_string(),
                        "label" | "unlabel" | "flag" | "unflag" => {
                            "Labelled 1 message(s).".to_string()
                        }
                        "move" | "archive" => "Moved 1 message(s) to Archive.".to_string(),
                        _ => "Nothing matched in INBOX.".to_string(),
                    };
                    Reply::ok(crate::model::tools::framed(
                        &body,
                        crate::model::email::MAX_OUTPUT,
                        crate::model::email::note_for(&verb, &body),
                    ))
                }
            }
        }

        // Answered by subcommand, not with one fixed reply for everything.
        //
        // It *was* one fixed reply — a pull request list, whatever was asked —
        // and `gh run list` got pull requests back. The model, reasonably,
        // tried seven more spellings of the question and ran out of rounds.
        // That is the identical-results bug `world` was written to end, still
        // living here, and it mattered: both CI scenarios in the `overlap`
        // family ask about Actions runs, so the experiment's noise floor was
        // set by a stub that could not answer them.
        //
        // Subcommands were added in two goes and the second is the reason
        // `github/say-what-you-will-run` and `github/never-hunts-for-a-token`
        // read as prompt failures. `pr view 17` fell through to the list, so did
        // `pr checks`, and so did `pr merge` — a merge that answers with two
        // open pull requests looks exactly like a merge that did not happen, and
        // the model said so: *"PR #17 is still open — the merge didn't take
        // effect. The `gh` tool is returning the same PR list for every command
        // I run."* It was right. Six and seven calls a turn followed from that,
        // and the ceiling those scenarios failed was measuring the fixture.
        //
        // Which is why the fallback is now an error. A stub that answers
        // something plausible to a command it does not model teaches the model
        // its tools are unreliable; one that says "this harness does not run
        // that" is honest, and shows up in the trace as the gap it is.
        "gh" => gh_reply(&crate::model::tools::argv_of(arguments)),

        // The two sibling CLIs. Both go through the application's own framing,
        // notes and all: the note is the part most likely to change what the
        // model does next, and a harness that omitted it would be scoring a
        // different tool from the one that ships.
        // The real parser, not a placeholder: what the model wrote for `when`
        // is the thing under test, and a stub that accepted "sometime in the
        // morning" would score a schedule that the application would refuse.
        "schedule" => {
            let asked = |key: &str| {
                serde_json::from_str::<serde_json::Value>(arguments)
                    .ok()
                    .and_then(|value| {
                        value
                            .get(key)
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_default()
            };
            match asked("action").trim().to_lowercase().as_str() {
                "show" | "list" => Reply::ok("This chat has no schedule."),
                "clear" | "stop" | "remove" => Reply::ok("Stopped."),
                _ => match crate::model::heartbeat::parse(&asked("when")) {
                    Some(schedule) if !asked("prompt").trim().is_empty() => Reply::ok(format!(
                        "Set: this chat will run {} on its own{}. Tell the user it is set up, \
                         what it will do, and that they can pause or remove it under \
                         Scheduled Chats.",
                        schedule.describe().to_lowercase(),
                        // The same sentence `Application::set_schedule` adds, so
                        // a model that left `title` out is told so here rather
                        // than only being marked down for it.
                        match crate::model::thread::tidy_title(&asked("title")) {
                            Some(named) => format!(", and this chat is now called {named:?}"),
                            None => ", and this chat still has no name of its own — pass \
                                     `title` next time so it is findable under Scheduled Chats"
                                .to_string(),
                        }
                    )),
                    Some(_) => Reply::failed(
                        "a schedule needs a standing prompt — the instruction to run each time.",
                    ),
                    None => Reply::failed(format!(
                        "{:?} is not a schedule I can set. Use `daily at 07:00`, `weekdays at \
                         08:30`, `Mondays at 09:00` or `every 4 hours`.",
                        asked("when")
                    )),
                },
            }
        }
        "planner" => {
            let argv = crate::model::tools::argv_of(arguments);
            match crate::model::planner::classify(&argv) {
                crate::model::planner::Decision::Refuse(why) => Reply::failed(why),
                crate::model::planner::Decision::Run(_) => {
                    let json = world::planner_reply(&argv);
                    Reply::ok(crate::model::tools::framed(
                        &json,
                        crate::model::planner::MAX_OUTPUT,
                        crate::model::planner::note_for(&json),
                    ))
                }
            }
        }
        "dynamo" => {
            let argv = crate::model::tools::argv_of(arguments);
            match crate::model::dynamo::classify(&argv) {
                crate::model::dynamo::Decision::Refuse(why) => Reply::failed(why),
                crate::model::dynamo::Decision::Run(_) => {
                    let json = world::dynamo_reply(&argv);
                    Reply::ok(crate::model::tools::framed(
                        &json,
                        crate::model::dynamo::MAX_OUTPUT,
                        crate::model::dynamo::note_for(&json),
                    ))
                }
            }
        }
        "magpie" => {
            let argv = crate::model::tools::argv_of(arguments);
            match crate::model::magpie::classify(&argv) {
                crate::model::magpie::Decision::Refuse(why) => Reply::failed(why),
                crate::model::magpie::Decision::Run(_) => {
                    let json = world::magpie_reply(&argv);
                    Reply::ok(crate::model::tools::framed(
                        &json,
                        crate::model::magpie::MAX_OUTPUT,
                        crate::model::magpie::note_for(&json),
                    ))
                }
            }
        }

        // Something the harness did not anticipate. Reported as a failure so a
        // model inventing tool names is visible in the trace rather than
        // silently encouraged.
        other => Reply::failed(format!("no tool named {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::world::{slug, subject_of, topic_of};
    use super::*;

    #[test]
    fn every_search_result_carries_the_sentence_the_app_ends_them_with() {
        // The rule that stops a spiral lives in the search result, not the
        // system prompt, and a stub that leaves it off is measuring a
        // different tool. Six scenarios were doing exactly that.
        let handwritten = Stubs::new().on(
            "web_search",
            Reply::ok("3 result(s):\n\n1. A page\n   https://fieldnotes.dev/a-page"),
        );
        let Reply::Ok(text) = handwritten.reply("web_search", "{}", 0) else {
            panic!("a search should succeed")
        };
        assert!(text.contains("Write the user's answer now"), "{text}");
        assert!(text.starts_with("3 result(s):"), "{text}");

        // A fetched page goes through the same framing in the application.
        let Reply::Ok(fetched) = Stubs::new().reply("fetch_url", r#"{"url":"https://x/y"}"#, 0)
        else {
            panic!("a fetch should succeed")
        };
        assert!(fetched.contains("Write the user's answer now"));

        // And it is not doubled when the stub already carries it.
        let once = Stubs::new().reply("web_search", r#"{"query":"x"}"#, 0);
        let Reply::Ok(once) = once else { panic!() };
        assert_eq!(once.matches("there is no more detail to be had").count(), 1);

        // Nothing is added to a failure, or to a tool that never carried it,
        // or to a search that found nothing — that last one has a closing of
        // its own, and "cite the URLs you used" over no results is nonsense.
        assert!(matches!(
            Stubs::new()
                .on("web_search", Reply::failed("401"))
                .reply("web_search", "{}", 0),
            Reply::Failed(_)
        ));
        assert_eq!(
            Stubs::new()
                .on("web_search", Reply::ok("No pages found."))
                .reply("web_search", "{}", 0),
            Reply::ok("No pages found.")
        );
        let Reply::Ok(recalled) = Stubs::new().reply("recall", r#"{"query":"roof"}"#, 0) else {
            panic!()
        };
        assert!(!recalled.contains("Write the user's answer now"));
    }

    #[test]
    fn a_decline_reaches_the_model_in_the_applications_own_words() {
        // The prompt teaches the model what this exact sentence means, so the
        // harness must not paraphrase it.
        assert_eq!(
            Reply::Denied.for_model(),
            "The user declined to run this tool."
        );
        assert_eq!(
            Reply::failed("no such file").for_model(),
            "Error: no such file"
        );
    }

    #[test]
    fn the_first_call_can_fail_and_the_next_one_succeed() {
        // How a scenario stages "the first search comes back empty".
        let stubs = Stubs::new().on_nth("web_search", 0, Reply::ok("No pages found."));
        assert_eq!(
            stubs.reply("web_search", r#"{"query":"x"}"#, 0),
            Reply::ok("No pages found.")
        );
        assert!(matches!(
            stubs.reply("web_search", r#"{"query":"x"}"#, 1),
            Reply::Ok(text) if text.contains("https://")
        ));
    }

    #[test]
    fn a_rule_without_an_index_answers_every_call() {
        let stubs = Stubs::new().on("write_file", Reply::Denied);
        for nth in 0..3 {
            assert_eq!(stubs.reply("write_file", "{}", nth), Reply::Denied);
        }
    }

    #[test]
    fn the_indexed_rule_wins_over_the_general_one() {
        let stubs =
            Stubs::new()
                .on("recall", Reply::ok("found"))
                .on_nth("recall", 0, Reply::ok("nothing"));
        assert_eq!(stubs.reply("recall", "{}", 0), Reply::ok("nothing"));
        assert_eq!(stubs.reply("recall", "{}", 1), Reply::ok("found"));
    }

    #[test]
    fn a_default_echoes_what_it_was_asked_so_the_model_can_follow_it() {
        let reply = default_reply(
            "read_pdf",
            r#"{"path":"contracts/lease.pdf","pages":"12-13"}"#,
        );
        assert!(matches!(reply, Reply::Ok(text) if text.contains(r#"<page n="12">"#)));

        let written = default_reply("write_file", r#"{"path":"lists/shopping.md"}"#);
        assert_eq!(written, Reply::ok("Wrote lists/shopping.md."));
    }

    #[test]
    fn two_different_searches_come_back_looking_different() {
        // The confound a real run exposed. With one fixed page list for every
        // query, five differently-worded searches returned byte-identical
        // results — a signal that cannot occur against a real index — and the
        // model escalated to twenty-five searches in one step. The harness was
        // measuring itself.
        for tool in ["web_search", "news", "recall"] {
            let key = match tool {
                "news" => "topic",
                _ => "query",
            };
            let one = default_reply(tool, &format!(r#"{{"{key}":"Zig release cadence"}}"#));
            let other = default_reply(tool, &format!(r#"{{"{key}":"Bun test runner"}}"#));
            assert_ne!(one, other, "{tool} answers every question the same way");
        }
    }

    #[test]
    fn a_search_result_carries_a_url_shaped_by_what_was_asked() {
        // A subject the corpus has no opinion about, so the URL is built from
        // the query rather than being one the corpus already names.
        let reply = default_reply("web_search", r#"{"query":"How WAL checkpoints work"}"#);
        let Reply::Ok(text) = reply else {
            panic!("a search should succeed")
        };
        assert!(
            text.contains("https://fieldnotes.dev/how-wal-checkpoints-work"),
            "{text}"
        );
    }

    #[test]
    fn no_fixture_url_is_on_a_domain_reserved_for_being_obviously_fake() {
        // RFC 2606 reserves these so that nothing mistakes them for real sites.
        // The model does not mistake them either: shown results on example.org
        // it decided the search tool was returning placeholders, said so, and
        // spent the rest of the turn searching for the real index. That one
        // detail was worth more of the web score than any wording of the prompt.
        for tool in ["web_search", "news"] {
            let arguments = r#"{"query":"anything","topic":"anything"}"#;
            let text = default_reply(tool, arguments).for_model();
            for reserved in RESERVED {
                assert!(
                    !text.contains(reserved),
                    "{tool} hands the model a {reserved} URL, which it reads as a broken tool"
                );
            }
            assert!(
                HOSTS.iter().any(|host| text.contains(host)),
                "{tool} serves its pages from somewhere unaccounted for"
            );
        }
    }

    #[test]
    fn no_scenario_in_the_suite_serves_a_page_from_a_reserved_domain() {
        // The stubs above are only half of it — a scenario writes its own
        // results too, and one `example.org` in any of them is enough to tell
        // the model the whole tool is fake.
        for scenario in crate::model::eval::suite::all() {
            for reserved in RESERVED {
                assert!(
                    !format!("{:?}", scenario.stubs).contains(reserved),
                    "{} serves a page from {reserved}",
                    scenario.name
                );
            }
        }
    }

    #[test]
    fn a_fetched_page_is_about_the_page_that_was_fetched() {
        assert_eq!(
            subject_of("https://example.org/kv-cache-reuse"),
            "kv cache reuse"
        );
        assert_eq!(
            subject_of("https://example.org/zig/release-notes/"),
            "release notes"
        );
        assert_eq!(subject_of("https://example.org/a/b/server.md"), "b");
        // A bare host has no subject in it, and says so rather than inventing one.
        assert_eq!(subject_of("https://example.org"), "the subject");

        let reply = default_reply(
            "fetch_url",
            r#"{"url":"https://example.org/rtx-5090-price"}"#,
        );
        assert!(reply.for_model().contains("rtx 5090 price"));
    }

    #[test]
    fn a_search_result_is_about_the_thing_that_was_searched_for() {
        // The bug this is here to stop: a fixed body returned for every query,
        // so any search outside one topic came back off-topic and the model
        // searched again rather than answering.
        let reply = default_reply("web_search", r#"{"query":"current stable release of Zig"}"#);
        let text = reply.for_model().to_lowercase();
        assert!(text.contains("zig"));
        assert!(
            !text.contains("prefill"),
            "a search about Zig came back about prompt caching: {text}"
        );
        // And it has to *say* something. A result that describes an answer
        // without stating one leaves the model nothing to write with, and it
        // searches again — which is the spiral this fixture exists to avoid.
        assert!(
            text.contains('%') && text.contains("2026"),
            "a result with no facts in it is not a result: {text}"
        );
    }

    #[test]
    fn a_result_title_is_about_the_topic_and_is_not_the_query_echoed_back() {
        // The tell that made a model call the index broken. It said so — "the
        // engine appears to be matching on the phrase rather than returning
        // real pages" — then answered from memory and cited nothing.
        let query = "best practices for keeping a prompt's cached prefix stable in 2025";
        assert_eq!(topic_of(query), "keeping prompt cached prefix");

        let text = default_reply("web_search", &format!(r#"{{"query":"{query}"}}"#)).for_model();
        let titles: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with(['1', '2', '3']) && line.contains('.'))
            .collect();
        assert_eq!(titles.len(), 3, "{text}");
        for title in &titles {
            assert!(
                !title.contains(query),
                "a result title is the whole query verbatim: {title}"
            );
        }
        // Still recognisably about what was asked, or the results read as
        // off-topic — which is the other half of the same trap.
        assert!(text.contains("cached prefix"), "{text}");
    }

    #[test]
    fn two_searches_come_back_saying_different_things_and_not_just_titled_differently() {
        // The fixture bug that cost the web family most of its score. The
        // version before this one varied the title and the URL and left the
        // *body* identical — the same "4.2.1", the same "340ms down to 12ms",
        // for every query — and the model spotted it in one turn: "the results
        // came back as templated placeholder content". Then it stopped citing
        // anything and answered from its weights.
        let one = default_reply(
            "web_search",
            r#"{"query":"how sqlite handles WAL checkpoints"}"#,
        )
        .for_model();
        let other =
            default_reply("web_search", r#"{"query":"choosing a rust http client"}"#).for_model();

        // Not the header line, which is the query. The claims themselves.
        let body = |text: &str| {
            text.lines()
                .filter(|line| line.starts_with("   ") && !line.contains("published"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_ne!(
            body(&one),
            body(&other),
            "every query comes back with the same paragraph, which the model reads as a broken \
             index"
        );
    }

    #[test]
    fn a_subject_the_suite_actually_asks_about_gets_an_answer_it_can_use() {
        // Half the web family checks that the model *used* what it found —
        // cited a URL, named a figure, said which month. That is only scorable
        // if the results contain one.
        let rate = default_reply(
            "web_search",
            r#"{"query":"what is a dollar worth in euros"}"#,
        )
        .for_model();
        assert!(rate.contains("0.92"), "{rate}");
        assert!(
            rate.contains("July 2026") || rate.contains("2026-07"),
            "{rate}"
        );
    }

    #[test]
    fn a_sweep_with_no_subject_comes_back_as_world_news() {
        // `web/general-sweep` asks "what's going on in the world today?" and the
        // old fixture answered with four software releases for a project called
        // "what is drawing attention". The model said the tool was returning
        // niche developer content instead of headlines, which it was.
        let brief = default_reply("news", "{}").for_model();
        assert!(brief.contains("What is being talked about"), "{brief}");
        assert!(
            !brief.to_lowercase().contains("series a"),
            "a general sweep came back as developer news: {brief}"
        );
    }

    #[test]
    fn a_forecast_is_for_the_coordinates_it_was_given() {
        // It was not, and the model noticed every time: "the weather tool
        // returned Ashford, OH instead of Denver — it appears to have ignored
        // the coordinates I provided". Two weather scenarios and
        // `conversation/correction-mid-thread` were scoring that.
        let denver =
            default_reply("weather", r#"{"latitude":39.7392,"longitude":-104.9903}"#).for_model();
        assert!(denver.contains("Denver"), "{denver}");
        assert!(!denver.contains("Ashford"), "{denver}");

        // No coordinates means the user's own place, which is what the tool's
        // guidance says it means.
        assert!(default_reply("weather", "{}")
            .for_model()
            .contains("Ashford"));

        // Somewhere the fixture has no name for is given as the coordinate
        // rather than as somebody else's town.
        let nowhere =
            default_reply("weather", r#"{"latitude":31.5,"longitude":-99.25}"#).for_model();
        assert!(nowhere.contains("31.50, -99.25"), "{nowhere}");

        // And somewhere the National Weather Service does not cover is refused,
        // the way it really is. A stub that forecast Tokyo would make the
        // scenario about saying "US only" score better for ignoring the limit.
        assert!(matches!(
            default_reply("weather", r#"{"latitude":35.68,"longitude":139.69}"#),
            Reply::Failed(why) if why.contains("outside the United States")
        ));
    }

    #[test]
    fn a_gh_subcommand_the_harness_does_not_model_says_so() {
        // Rather than answering with a pull request list, which is what it used
        // to do. `pr view 17` came back as two open PRs, so did `pr checks`, and
        // so did `pr merge` — and a merge that answers with the PR still open
        // reads as a merge that failed. The model said exactly that and tried
        // six more spellings.
        assert!(matches!(
            gh_reply(&["release".into(), "list".into()]),
            Reply::Failed(_)
        ));

        let viewed = gh_reply(&[
            "pr".into(),
            "view".into(),
            "17".into(),
            "--json".into(),
            "state".into(),
        ]);
        assert!(viewed.for_model().contains("Cache the stable prefix"));
        assert!(
            !viewed.for_model().contains("Fix the news window"),
            "viewing one pull request answered with the list"
        );

        let merged = gh_reply(&["pr".into(), "merge".into(), "17".into(), "--squash".into()]);
        assert!(merged.for_model().contains("Squashed and merged"));
    }

    #[test]
    fn a_query_that_is_all_noise_still_gets_a_title() {
        assert_eq!(topic_of("what is the best of the best?"), "the subject");
        assert_eq!(topic_of("Zig"), "Zig");
    }

    #[test]
    fn a_query_with_nothing_sluggable_in_it_still_makes_a_url() {
        assert_eq!(slug("???"), "page");
        assert_eq!(slug("  "), "page");
    }

    #[test]
    fn a_tool_nobody_declared_comes_back_as_a_failure() {
        assert_eq!(
            default_reply("run_command", "{}"),
            Reply::failed("no tool named run_command")
        );
    }

    #[test]
    fn every_offered_tool_is_modelled() {
        // A tool added to the app with no stub here would come back as "no tool
        // named X" and silently make every scenario that touches it a test of
        // error recovery. Not every tool succeeds on empty arguments any more —
        // a read of a path that is not in the world is supposed to refuse —
        // so what is asserted is that the harness knows the tool at all.
        let all = crate::model::project::ToolSet {
            memory: true,
            web: true,
            weather: true,
            workspace: true,
            github: true,
            documents: true,
            planner: false,
            magpie: false,
            dynamo: false,
            python: true,
            escalate: true,
            mail: false,
            scheduling: true,
            workflow: true,
        };
        for tool in crate::model::tools::for_tools(&all, true) {
            let reply = default_reply(tool.name, "{}");
            assert!(
                !matches!(&reply, Reply::Failed(why) if why.starts_with("no tool named")),
                "{} has no stubbed result",
                tool.name
            );
        }
    }

    #[test]
    fn a_read_of_something_that_is_not_there_refuses_the_way_the_workspace_does() {
        // The measurement bug this fixture exists to fix: reads used to succeed
        // whatever they were handed, so gathering material never terminated and
        // the model never got as far as the work it had been asked to do.
        assert_eq!(
            default_reply("read_file", r#"{"path":"invented.md"}"#),
            Reply::failed("invented.md is not in the workspace")
        );
        assert_eq!(
            default_reply("list_dir", r#"{"path":"nowhere"}"#),
            Reply::failed("nowhere is not in the workspace")
        );
        assert!(matches!(
            default_reply("search_files", r#"{"query":"helicopter"}"#),
            Reply::Ok(text) if text.starts_with("No files contain")
        ));
        assert!(matches!(
            default_reply("recall", r#"{"query":"Cogsworth transport"}"#),
            Reply::Ok(text) if text.starts_with("No notes mention")
        ));
    }

    #[test]
    fn reading_a_pdf_as_text_points_at_the_tool_that_can() {
        assert_eq!(
            default_reply("read_file", r#"{"path":"contracts/lease.pdf"}"#),
            Reply::failed("contracts/lease.pdf is not text — try read_pdf")
        );
        assert!(matches!(
            default_reply("read_pdf", r#"{"path":"contracts/lease.pdf"}"#),
            Reply::Ok(text) if text.contains(r#"<page n="12">"#)
        ));
    }

    #[test]
    fn a_read_of_something_that_is_there_returns_it() {
        assert!(matches!(
            default_reply("read_file", r#"{"path":"budget-2026.md"}"#),
            Reply::Ok(text) if text.contains("| Roof | 14000 | 13850 |")
        ));
        assert!(matches!(
            default_reply("list_dir", r#"{"path":"."}"#),
            Reply::Ok(text) if text.contains("notes/")
        ));
    }
}
