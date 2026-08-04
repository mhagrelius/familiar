//! Searching the web with Exa.
//!
//! Exa is a *semantic* search engine: it embeds the query and returns the
//! nearest pages, so it is not a keyword engine wearing a different hat.
//! Describing the page you want finds things; typing the fact you want does
//! not. That distinction is most of the difference between a useful search and
//! a useless one, so it is written into the tool description where the model
//! will actually read it — the same job Claude Code's Exa *skill* does, minus
//! the subagent orchestration a 27B model has no room for.
//!
//! One request does the work of two: `/search` with `contents.text` returns the
//! page text alongside each result, so the model reads and synthesises in the
//! same round rather than searching, then fetching, then answering.

use serde::{Deserialize, Serialize};

/// Exa's REST endpoint. The hosted MCP server is the other way in; this is the
/// one that does not need an MCP client.
pub const SEARCH_URL: &str = "https://api.exa.ai/search";
pub const CONTENTS_URL: &str = "https://api.exa.ai/contents";

/// Never more than this, whatever the model asks for. Each result carries page
/// text, and twenty of those will crowd out the conversation.
pub const MAX_RESULTS: usize = 8;

/// How much text to keep per result. Enough to answer from, short enough that
/// several results still fit.
pub const CHARS_PER_RESULT: usize = 1200;

/// What the model is told about searching well.
///
/// Lifted from Exa's own guidance because it is right and because a local model
/// will otherwise type the question into the box: describe the *page*, use a
/// category, run different angles rather than synonyms, and never expect
/// boolean operators to mean anything.
pub const SEARCH_GUIDANCE: &str = "\
How to use it: search once, then answer. The results come back with the page \
text in them, so after one good search you almost always have what you need. \
Write the reply from it.

Only search again if you can name the specific fact you are still missing, and \
say what it is before you do. Three searches in a turn is the hard ceiling, and \
you will rarely want the third. Do not plan a sweep of the topic and do not \
fire several searches at once — you cannot read results you have not received, \
and a turn that ends in a pile of searches and no answer has failed the user \
however thorough it looked.

Exa is a semantic search engine, not a keyword one. Describe the page you are \
looking for, as a natural phrase — \"detailed blog post explaining X by \
someone who built it\" finds things, \"X\" does not. Prefix with \
`category:company`, `category:news`, `category:publication`, \
`category:people` or `category:personal site` when you want one kind of page. \
Boolean operators and quotes are just words to it. One- or two-word queries \
return scattered results.

Assume what you remember has gone stale. Your weights stopped changing on a \
date that has passed; the world did not. Anything carrying a version number, a \
price, a release, a benchmark, whoever currently holds a post, or the word \
\"latest\", \"current\" or \"best\" has moved since — and you cannot tell from \
the inside that it has, because a stale fact feels exactly like a fresh one. \
Look those up instead of answering from memory. Check the user's premise too \
when they state one, because they are often remembering the same out-of-date \
thing you are. You can always check, so give the user the answer you found \
rather than a caveat about how far back your own knowledge goes — that is a \
question the search already settled.

What does not move, you already know. Definitions, mathematics, how a protocol \
or a language works, what an error code means, anything settled years ago — \
answer those straight out. Searching for them wastes the user's time and buries \
the answer.

If a search comes back empty or irrelevant, make the query longer and more \
specific, or try a genuinely different angle; a synonym is not a different \
angle. If three angles find nothing, say so — the topic has little coverage, \
and telling the user that is a finished answer. Searching a fourth time is not.

When the question involves time, work out the actual dates from today's date \
first and put them in the query as words (\"published in July 2026\"), and give \
the date of what you report so the user can see how fresh it is.

Do not fetch the URLs a search gave you — the text under them is the text you \
would get back. Cite what you used by title and by pasting its URL, so the user \
can open it.";

/// What a turn is allowed to spend on looking things up.
///
/// The prompt says three searches is the ceiling, and the prompt is not what
/// holds it. Four wordings of that sentence were measured — plain prose, a
/// numeric ceiling, a procedural *search once then answer* at the head of the
/// note, and the same rule copied into the tool description — and against an
/// open-ended question the model ran twelve to seventeen searches and ended the
/// turn with nothing written, every time. That is the published result too:
/// telling a model its budget does not make it keep one, because it cannot
/// count what it has spent.
///
/// So the count lives out here, where it is arithmetic. When it runs out the
/// tool does not run: the call comes back having spent nothing, saying the
/// budget is gone and the answer is now due. The model reads that in the same
/// position it reads a result, which is the one place guidance has been shown to
/// work in this app.
///
/// This is the rule, not the enforcement — [`Budget::refuse`] is a string and
/// [`Budget::SEARCHES_PER_TURN`] is a number, so the application and the eval
/// harness can hold the same line without either owning it.
pub struct Budget;

impl Budget {
    /// Searches — `web_search` and `news` together — one turn may run.
    ///
    /// Three, matching what the guidance says out loud, so the model is never
    /// refused something it was told it could have. Nearly every question is
    /// answered by the first; the third exists for the genuine second angle.
    pub const SEARCHES_PER_TURN: usize = 3;

    /// Whether a search may run, given how many have already gone out this turn.
    pub fn allows(spent: usize) -> bool {
        spent < Self::SEARCHES_PER_TURN
    }

    /// What comes back instead of results when the budget is gone.
    ///
    /// Phrased as a finished state rather than a failure. The decline note in
    /// the system prompt teaches the model to read a refusal as an answer and
    /// carry on, and this has to land the same way — a turn that ends here has
    /// three searches of material in it and is not short of anything except a
    /// reply.
    pub fn refuse(spent: usize) -> String {
        format!(
            "Not run: this turn has already used its {spent} searches, which is all it gets. \
             You are not missing anything you could have found — write the user's answer now \
             from the results already above, cite the URLs you used, and say plainly if some \
             part of it went unanswered.\n\n\
             This is a limit on looking things up, not on `web_search` in particular. Do not \
             go after the same fact with `fetch_url`, `gh`, or anything else — that is the \
             same search wearing a different hat, and it is how a turn spends nine calls and \
             answers nothing. \"I could not confirm the exact figure\" is a finished answer; \
             a ninth lookup is not."
        )
    }
}

/// A search, as Exa wants it.
///
/// The filters below are all `None` for an ordinary `web_search` and are
/// skipped when they are, so that tool's request is exactly what it always was.
/// They exist for [`news`](super::news), which is the same endpoint asked a
/// narrower question — one struct rather than two that would drift.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(rename = "numResults")]
    pub num_results: usize,
    /// Exa's own summarisation is not asked for: the model is the summariser,
    /// and a summary of a summary loses the detail the answer needed.
    pub contents: Contents,
    /// Only pages published after this, ISO 8601. The whole reason a news
    /// search is not a web search.
    #[serde(rename = "startPublishedDate", skip_serializing_if = "Option::is_none")]
    pub start_published_date: Option<String>,
    #[serde(rename = "endPublishedDate", skip_serializing_if = "Option::is_none")]
    pub end_published_date: Option<String>,
    /// One of Exa's own page classifications — `news`, `company`,
    /// `publication`, `people`, `personal site`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Restrict the search to these hosts.
    #[serde(rename = "includeDomains", skip_serializing_if = "Option::is_none")]
    pub include_domains: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Contents {
    pub text: TextOptions,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TextOptions {
    #[serde(rename = "maxCharacters")]
    pub max_characters: usize,
    #[serde(rename = "includeHtmlTags")]
    pub include_html_tags: bool,
}

impl SearchRequest {
    pub fn new(query: &str, results: usize) -> Self {
        Self {
            query: query.trim().to_string(),
            num_results: results.clamp(1, MAX_RESULTS),
            contents: Contents {
                text: TextOptions {
                    max_characters: CHARS_PER_RESULT,
                    include_html_tags: false,
                },
            },
            start_published_date: None,
            end_published_date: None,
            category: None,
            include_domains: None,
        }
    }
}

/// Reading known URLs, for when a search result is not enough.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContentsRequest {
    pub urls: Vec<String>,
    pub text: TextOptions,
}

impl ContentsRequest {
    pub fn new(url: &str) -> Self {
        Self {
            urls: vec![url.trim().to_string()],
            text: TextOptions {
                // A page being read on purpose is worth more room than one of
                // eight search results.
                max_characters: 6000,
                include_html_tags: false,
            },
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct SearchResponse {
    #[serde(default)]
    pub results: Vec<Found>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Found {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default, rename = "publishedDate")]
    pub published: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
}

impl SearchResponse {
    /// What goes back to the model.
    ///
    /// Numbered, with the source on its own line, because an answer that cites
    /// "the second result" is not a citation and an answer that cites a bare
    /// URL is not readable.
    pub fn for_model(&self, query: &str) -> String {
        if self.results.is_empty() {
            // The encouragement used to stand on its own, and a run of empty
            // searches then had no floor: the model was told to try another
            // angle, tried one, was told again, and spent the turn on it. An
            // empty result is the one place a stop condition matters most.
            return format!(
                "No pages found for {query:?}. Try a longer, more specific query, or a \
                 genuinely different angle — a synonym will return this same nothing. If two \
                 or three different angles have already come back empty, that is your answer: \
                 tell the user the topic has little coverage and stop there. Saying so is a \
                 complete reply, and a better one than a fourth search."
            );
        }

        let mut out = format!("{} result(s) for {query:?}:\n", self.results.len());
        for (at, found) in self.results.iter().enumerate() {
            let title = found.title.as_deref().unwrap_or("untitled");
            out.push_str(&format!("\n{}. {title}\n   {}\n", at + 1, found.url));
            if let Some(published) = &found.published {
                out.push_str(&format!("   published {published}\n"));
            }
            if let Some(text) = &found.text {
                let text = text.trim();
                if !text.is_empty() {
                    out.push_str("   ");
                    out.push_str(&squash(text));
                    out.push('\n');
                }
            }
        }
        out.push_str(CLOSING_LINE);
        out
    }
}

/// What every non-empty search result ends with.
///
/// This is here rather than in the system prompt because the system prompt lost.
/// Four wordings of "three searches is the ceiling" were measured, and against
/// an open-ended question — "what's the current thinking on X" — the model ran
/// twelve to seventeen searches and ended the turn with no reply every time. A
/// rule read once, thousands of tokens ago, does not compete with the pull of
/// one more query.
///
/// This sentence is the last thing in the context at the moment the decision is
/// made, and it is there again after every search. That position is the whole
/// point of it.
pub const CLOSING_LINE: &str = "\n\
    The page text above is what a search gives you; there is no more detail to \
    be had by running the same search differently. If what they asked for is \
    not in there, say which part you could not confirm — that is a finished \
    answer, not a failure — and do not go after it with `fetch_url`, `gh` or \
    another search, which is the same search wearing a different hat.\n\
    Write the user's answer now, and paste the URL of each result you used.\n";

/// Page text as one paragraph. The layout of a scraped page is noise here, and
/// blank lines triple the token cost of saying the same thing.
fn squash(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut spaced = true;
    for character in text.chars() {
        if character.is_whitespace() {
            if !spaced {
                out.push(' ');
                spaced = true;
            }
        } else {
            out.push(character);
            spaced = false;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_gets_the_number_of_searches_the_guidance_promises_it() {
        // If these two ever disagree the model is refused something it was
        // told it could have, which reads as a broken tool rather than a
        // budget — and a model that thinks the tool is broken keeps trying.
        assert!(
            SEARCH_GUIDANCE.contains("Three searches in a turn is the hard ceiling"),
            "the guidance no longer says what the budget enforces"
        );
        assert_eq!(Budget::SEARCHES_PER_TURN, 3);

        assert!(Budget::allows(0));
        assert!(Budget::allows(2));
        assert!(!Budget::allows(3));
        assert!(!Budget::allows(9));
    }

    #[test]
    fn a_spent_budget_asks_for_the_answer_rather_than_reporting_a_failure() {
        let refused = Budget::refuse(3);
        assert!(refused.contains('3'));
        // The words that matter: it must not read as something to retry or
        // route around, which is how the model treats a failure.
        assert!(refused.contains("write the user's answer now"));
        for panic_word in ["Error", "failed", "unavailable", "try again"] {
            assert!(
                !refused.contains(panic_word),
                "a spent budget reads as {panic_word:?}, which invites a retry"
            );
        }
        // And it has to close the other doors by name. A budget the model
        // reads as "this tool is closed" is one it walks around: a measured
        // run spent its three searches, then a `fetch_url`, then three
        // `gh release list` calls after the same fact, and answered nothing.
        for door in ["fetch_url", "gh"] {
            assert!(
                refused.contains(door),
                "a spent budget never mentions {door}, which is the next thing tried"
            );
        }
    }

    #[test]
    fn a_search_asks_for_page_text_and_a_sane_number_of_results() {
        let json = serde_json::to_string(&SearchRequest::new("  a phrase  ", 5)).expect("json");
        assert!(json.contains(r#""query":"a phrase""#), "{json}");
        assert!(json.contains(r#""numResults":5"#), "{json}");
        assert!(json.contains("maxCharacters"), "{json}");
        // An ordinary search sends exactly what it always sent: the news
        // filters share this struct and must be invisible when unset.
        assert!(!json.contains("startPublishedDate"), "{json}");
        assert!(!json.contains("category"), "{json}");
        assert!(!json.contains("includeDomains"), "{json}");
    }

    #[test]
    fn the_number_of_results_is_clamped_however_it_is_asked_for() {
        // Each result carries page text; twenty would crowd out the
        // conversation it is meant to inform.
        assert_eq!(SearchRequest::new("x", 50).num_results, MAX_RESULTS);
        assert_eq!(SearchRequest::new("x", 0).num_results, 1);
    }

    #[test]
    fn results_come_back_numbered_and_cited() {
        let response = SearchResponse {
            results: vec![Found {
                title: Some("How Exa works".into()),
                url: "https://exa.ai/how".into(),
                text: Some("Embeddings,\n\n  not keywords.".into()),
                published: Some("2026-07-01".into()),
                author: None,
            }],
        };
        let shown = response.for_model("how does exa work");
        assert!(shown.contains("1. How Exa works"), "{shown}");
        assert!(shown.contains("https://exa.ai/how"), "{shown}");
        assert!(shown.contains("published 2026-07-01"), "{shown}");
        // Page whitespace is squashed: the layout is noise and blank lines are
        // paid for in tokens.
        assert!(shown.contains("Embeddings, not keywords."), "{shown}");
    }

    #[test]
    fn finding_nothing_says_what_to_try_instead() {
        let shown = SearchResponse::default().for_model("obscure thing");
        assert!(shown.contains("No pages found"), "{shown}");
        assert!(shown.contains("different angle"), "{shown}");
    }

    #[test]
    fn the_guidance_teaches_the_thing_that_actually_matters() {
        // A local model will type the question into the box unless told not to.
        assert!(SEARCH_GUIDANCE.contains("Describe the page"));
        assert!(SEARCH_GUIDANCE.contains("category:"));
    }
}
