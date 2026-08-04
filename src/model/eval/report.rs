//! Scoring a run, and saying whether a prompt change actually helped.
//!
//! A score on its own is close to meaningless — sampling moves it, and a suite
//! this size moves a point or two between identical runs. What is meaningful is
//! the same suite against two prompts, per scenario, with the flaky ones marked
//! as flaky rather than counted as a result. So a report serialises whole, and
//! [`Report::against`] is the thing you actually read after changing a sentence
//! in the prompt.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One antipattern sighting, flattened for the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flaw {
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// One check that did not hold, and what happened instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub check: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// Every check, whether or not it held.
///
/// `failures` is what the text report reads and is a subset of this. Both are
/// kept because they answer different questions: the terminal wants what went
/// wrong, and a person reviewing whether the *expectations themselves* are
/// right needs to see the ones that passed just as much — an assertion nobody
/// ever sees is how a suite comes to encode something nobody agreed to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerdictReport {
    pub check: String,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// One tool call, as it was actually made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallReport {
    pub name: String,
    /// The JSON the model wrote, capped — see [`ARGUMENT_CAP`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub arguments: String,
    /// `ok`, `failed` or `denied`, as the harness answered it.
    pub reaction: String,
    /// The application would have stopped for the user's approval here. The
    /// harness never does, so without this a gated call is indistinguishable in
    /// a trace from one that runs unattended.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gated: bool,
    /// What the harness handed back, capped — see [`RESULT_CAP`].
    ///
    /// The half of a trace that was missing. Every scenario in this suite is
    /// scored on what the model did *next*, and what it did next is a reaction
    /// to this text; a reviewer asked whether an expectation is fair cannot
    /// answer without seeing it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub result: String,
}

/// One turn of one run: what was asked, what was called, what came back.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepReport {
    pub user: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<CallReport>,
    /// Anything said before or between calls.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub preamble: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub answer: String,
}

/// What a scenario asks, and what it asserts about the answer.
///
/// In the report rather than only in the source because the whole point of the
/// HTML artifact is that somebody can disagree with an expectation without
/// reading Rust.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskReport {
    pub user: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
}

/// How much of a call's arguments or an answer is kept.
///
/// A whole suite's traces at three repeats is a few megabytes, which is fine
/// for a file on disk and not fine for one the browser has to lay out. The cap
/// is well above every real call — the longest are a document's Markdown and a
/// Python script — and what it truncates it marks.
pub const ARGUMENT_CAP: usize = 2_000;
pub const ANSWER_CAP: usize = 4_000;
/// Tool results are capped harder than arguments because a handful of them —
/// `read_skill` hands back a whole page of instructions — would otherwise be
/// most of the file. Enough to see the shape of what came back and judge it.
pub const RESULT_CAP: usize = 1_500;

/// Cut to a cap, saying so when it cut.
pub fn capped(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let kept: String = text.chars().take(cap).collect();
    format!("{kept}\n… [{} characters more]", text.chars().count() - cap)
}

/// One scenario, run once.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub passed: usize,
    pub total: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<Failure>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flaws: Vec<Flaw>,
    /// The tools it reached for, in order, across every step. The single most
    /// useful line when reading why a scenario failed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<String>,
    /// Every check and how it went, for the reviewer rather than the terminal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verdicts: Vec<VerdictReport>,
    /// What actually happened, turn by turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<StepReport>,
    pub rounds: usize,
    pub elapsed_ms: u64,
    pub generated_tokens: u32,
    /// The run never completed — the server refused or the connection died. It
    /// is excluded from the score rather than counted as zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broken: Option<String>,
}

impl Run {
    pub fn clean(&self) -> bool {
        self.broken.is_none() && self.passed == self.total && self.flaws.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScenarioResult {
    pub name: String,
    pub about: String,
    /// Which capabilities were switched on, so a reviewer can see that a
    /// `NeverCalls` had something to never call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// What was asked and what was asserted, ask by ask.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asks: Vec<AskReport>,
    pub runs: Vec<Run>,
}

impl ScenarioResult {
    pub fn family(&self) -> &str {
        self.name.split('/').next().unwrap_or(&self.name)
    }

    fn scored(&self) -> impl Iterator<Item = &Run> {
        self.runs.iter().filter(|run| run.broken.is_none())
    }

    /// Checks passed over checks attempted, across every run that completed.
    /// `None` when nothing completed.
    pub fn rate(&self) -> Option<f64> {
        let (passed, total) = self.scored().fold((0usize, 0usize), |(p, t), run| {
            (p + run.passed, t + run.total)
        });
        (total > 0).then(|| passed as f64 / total as f64)
    }

    /// How many completed runs passed every check.
    pub fn perfect(&self) -> usize {
        self.scored().filter(|run| run.passed == run.total).count()
    }

    pub fn completed(&self) -> usize {
        self.scored().count()
    }

    /// The model did it one way sometimes and another way other times. Worth
    /// separating from a straight failure: a flaky scenario is usually a prompt
    /// that is quiet on the point rather than one that is wrong about it.
    pub fn flaky(&self) -> bool {
        let perfect = self.perfect();
        perfect > 0 && perfect < self.completed()
    }

    /// Every check that failed at least once, most frequent first.
    pub fn weak_checks(&self) -> Vec<(String, usize)> {
        let mut tally: BTreeMap<String, usize> = BTreeMap::new();
        for run in self.scored() {
            for failure in &run.failures {
                *tally.entry(failure.check.clone()).or_default() += 1;
            }
        }
        let mut ranked: Vec<(String, usize)> = tally.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked
    }
}

/// A whole pass of the suite.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// What was being tested — a prompt variant's name, or a git revision.
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Which summarizer the run folded with between turns, or `off`.
    ///
    /// Recorded because two reports at different arms are not comparable and
    /// nothing else in the file would say so — the scenarios, the persona and
    /// the prompt digest are all identical between them. Absent in reports
    /// written before the arms existed, which is the same thing as `off`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<String>,
    pub repeats: usize,
    /// Every distinct piece of prompt text the suite put in front of the model
    /// — the persona and each capability note. Kept because a report six weeks
    /// old is only comparable if you can see what it was measuring, and most of
    /// what governs tool use is guidance compiled in from `tools.rs`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt_digest: String,
    pub scenarios: Vec<ScenarioResult>,
}

impl Report {
    /// Checks passed over checks attempted, across the suite.
    pub fn rate(&self) -> f64 {
        let (passed, total) = self
            .scenarios
            .iter()
            .flat_map(|scenario| scenario.runs.iter())
            .filter(|run| run.broken.is_none())
            .fold((0usize, 0usize), |(p, t), run| {
                (p + run.passed, t + run.total)
            });
        if total == 0 {
            0.0
        } else {
            passed as f64 / total as f64
        }
    }

    /// The compaction arm, with an unrecorded one read as `off` — which is what
    /// every report written before the arms existed was.
    pub fn arm(&self) -> &str {
        self.compaction.as_deref().unwrap_or("off")
    }

    pub fn broken(&self) -> usize {
        self.scenarios
            .iter()
            .flat_map(|scenario| scenario.runs.iter())
            .filter(|run| run.broken.is_some())
            .count()
    }

    /// The score per family, so a change that helps one capability and hurts
    /// another does not average out to "no difference".
    ///
    /// `None` where nothing in the family completed. A server that died halfway
    /// through takes a whole family with it, and reporting that as 0% reads as
    /// the prompt having collapsed on exactly the capability the run never
    /// measured.
    pub fn families(&self) -> Vec<(String, Option<f64>)> {
        let mut tally: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for scenario in &self.scenarios {
            let entry = tally.entry(scenario.family().to_string()).or_default();
            for run in scenario.runs.iter().filter(|run| run.broken.is_none()) {
                entry.0 += run.passed;
                entry.1 += run.total;
            }
        }
        tally
            .into_iter()
            .map(|(family, (passed, total))| {
                (family, (total > 0).then(|| passed as f64 / total as f64))
            })
            .collect()
    }

    /// Scenarios no run ever completed, so nothing can be said about them.
    pub fn unmeasured(&self) -> Vec<&ScenarioResult> {
        self.scenarios
            .iter()
            .filter(|scenario| scenario.completed() == 0)
            .collect()
    }

    /// Every antipattern seen, most frequent first. The axis nobody had to
    /// predict.
    pub fn flaws(&self) -> Vec<(String, usize)> {
        let mut tally: BTreeMap<String, usize> = BTreeMap::new();
        for scenario in &self.scenarios {
            for run in &scenario.runs {
                for flaw in &run.flaws {
                    *tally.entry(flaw.kind.clone()).or_default() += 1;
                }
            }
        }
        let mut ranked: Vec<(String, usize)> = tally.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked
    }

    /// The scenarios worth working on: measured, and never clean, worst first.
    pub fn worst(&self, how_many: usize) -> Vec<&ScenarioResult> {
        let mut ranked: Vec<&ScenarioResult> = self
            .scenarios
            .iter()
            .filter(|scenario| {
                scenario.completed() > 0 && scenario.perfect() < scenario.completed()
            })
            .collect();
        ranked.sort_by(|a, b| {
            a.rate()
                .unwrap_or(0.0)
                .partial_cmp(&b.rate().unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.name.cmp(&b.name))
        });
        ranked.truncate(how_many);
        ranked
    }

    pub fn find(&self, name: &str) -> Option<&ScenarioResult> {
        self.scenarios.iter().find(|scenario| scenario.name == name)
    }

    /// This report against an earlier one, scenario by scenario.
    pub fn against<'a>(&'a self, baseline: &'a Report) -> Comparison<'a> {
        let mut moved = Vec::new();
        for scenario in &self.scenarios {
            let (Some(now), Some(before)) = (
                scenario.rate(),
                baseline.find(&scenario.name).and_then(ScenarioResult::rate),
            ) else {
                continue;
            };
            if (now - before).abs() > f64::EPSILON {
                moved.push(Movement {
                    name: &scenario.name,
                    before,
                    now,
                });
            }
        }
        moved.sort_by(|a, b| {
            a.delta()
                .partial_cmp(&b.delta())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let missing = self
            .scenarios
            .iter()
            .filter(|scenario| baseline.find(&scenario.name).is_none())
            .map(|scenario| scenario.name.as_str())
            .collect();

        Comparison {
            baseline,
            current: self,
            moved,
            missing,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Movement<'a> {
    pub name: &'a str,
    pub before: f64,
    pub now: f64,
}

impl Movement<'_> {
    pub fn delta(&self) -> f64 {
        self.now - self.before
    }
}

/// Two reports, side by side.
#[derive(Debug, Clone)]
pub struct Comparison<'a> {
    pub baseline: &'a Report,
    pub current: &'a Report,
    /// Scenarios whose rate changed, regressions first.
    pub moved: Vec<Movement<'a>>,
    /// Scenarios the baseline never ran, so nothing can be said about them.
    pub missing: Vec<&'a str>,
}

impl Comparison<'_> {
    pub fn regressions(&self) -> impl Iterator<Item = &Movement<'_>> {
        self.moved.iter().filter(|movement| movement.delta() < 0.0)
    }

    pub fn improvements(&self) -> impl Iterator<Item = &Movement<'_>> {
        self.moved.iter().filter(|movement| movement.delta() > 0.0)
    }

    pub fn delta(&self) -> f64 {
        self.current.rate() - self.baseline.rate()
    }
}

// -- what you read -----------------------------------------------------------

/// The report as one self-contained HTML file.
///
/// It exists because the terminal report answers "did it get worse" and cannot
/// answer the more important question: **are these the right expectations?**
/// That one needs the ask, the assertion, and what the model actually did, side
/// by side and for every scenario rather than only the failing ones — and
/// reading it out of the Rust is not a review, it is an archaeology dig.
///
/// One file, no network, no build step: the whole report is embedded as JSON
/// and the page is drawn from it in the browser. A reviewer can open it from a
/// USB stick on a machine that has never heard of Rust, and their notes stay in
/// their own browser until they choose to hand them back.
pub fn render_html(report: &Report) -> String {
    let data = serde_json::to_string(report).unwrap_or_else(|_| "{}".into());
    // The payload sits in a `<script>`, so the only sequence that could end it
    // early is `</`. Escaping it keeps a scenario that quotes HTML — and the
    // suite has prompt-injection fixtures that do — from breaking the page.
    let data = data.replace("</", "<\\/");
    include_str!("report.html").replace("__DATA__", &data)
}

/// The report, as text.
pub fn render(report: &Report) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} — {:.0}% of {} checks, {} scenarios × {} run(s)",
        report.label,
        report.rate() * 100.0,
        total_checks(report),
        report.scenarios.len(),
        report.repeats
    ));
    if let Some(model) = &report.model {
        out.push_str(&format!(" on {model}"));
    }
    if let Some(compaction) = report.compaction.as_deref().filter(|mode| *mode != "off") {
        out.push_str(&format!(", folding with {compaction}"));
    }
    if report.broken() > 0 {
        out.push_str(&format!(" — {} run(s) did not complete", report.broken()));
    }
    out.push_str("\n\n");

    out.push_str("By capability\n");
    for (family, rate) in report.families() {
        match rate {
            Some(rate) => out.push_str(&format!("  {family:<14} {:>4.0}%\n", rate * 100.0)),
            None => out.push_str(&format!("  {family:<14}    —  nothing completed\n")),
        }
    }

    let flaws = report.flaws();
    out.push_str("\nAntipatterns\n");
    if flaws.is_empty() {
        out.push_str("  none\n");
    }
    for (kind, count) in flaws {
        out.push_str(&format!("  {count:>3}×  {kind}\n"));
    }

    let worst = report.worst(12);
    if !worst.is_empty() {
        out.push_str("\nWorth fixing\n");
        for scenario in worst {
            let rate = scenario.rate().map(|r| r * 100.0).unwrap_or(0.0);
            let flaky = if scenario.flaky() { " (flaky)" } else { "" };
            out.push_str(&format!(
                "  {:>4.0}%  {}{flaky}\n         {}\n",
                rate, scenario.name, scenario.about
            ));
            for (check, count) in scenario.weak_checks().iter().take(3) {
                out.push_str(&format!("         ✗ {check} ({count}×)\n"));
            }
        }
    }

    let unmeasured = report.unmeasured();
    if !unmeasured.is_empty() {
        out.push_str("\nNot measured — every run failed to complete\n");
        for scenario in unmeasured {
            let why = scenario
                .runs
                .iter()
                .find_map(|run| run.broken.as_deref())
                .unwrap_or("no runs");
            out.push_str(&format!("  {}\n         {why}\n", scenario.name));
        }
    }
    out
}

/// A comparison, as text.
pub fn render_comparison(comparison: &Comparison) -> String {
    let mut out = format!(
        "{} → {}: {:.0}% → {:.0}% ({:+.1} points)\n",
        comparison.baseline.label,
        comparison.current.label,
        comparison.baseline.rate() * 100.0,
        comparison.current.rate() * 100.0,
        comparison.delta() * 100.0
    );

    // Two arms differing is the summarizer measurement rather than a mistake,
    // so this says which is which instead of refusing. It matters either way:
    // read as a prompt A/B, the same two numbers would be a lie.
    if comparison.baseline.arm() != comparison.current.arm() {
        out.push_str(&format!(
            "compaction {} → {}: the difference is what folding cost, not a prompt change\n",
            comparison.baseline.arm(),
            comparison.current.arm()
        ));
    }

    let regressions: Vec<&Movement> = comparison.regressions().collect();
    let improvements: Vec<&Movement> = comparison.improvements().collect();

    if regressions.is_empty() && improvements.is_empty() {
        out.push_str("\nNo scenario moved.\n");
    }
    if !regressions.is_empty() {
        out.push_str("\nWorse\n");
        for movement in regressions {
            out.push_str(&format!(
                "  {:>4.0}% → {:>4.0}%  {}\n",
                movement.before * 100.0,
                movement.now * 100.0,
                movement.name
            ));
        }
    }
    if !improvements.is_empty() {
        out.push_str("\nBetter\n");
        for movement in improvements.iter().rev() {
            out.push_str(&format!(
                "  {:>4.0}% → {:>4.0}%  {}\n",
                movement.before * 100.0,
                movement.now * 100.0,
                movement.name
            ));
        }
    }
    if !comparison.missing.is_empty() {
        out.push_str(&format!(
            "\nNot in the baseline: {}\n",
            comparison.missing.join(", ")
        ));
    }
    out
}

fn total_checks(report: &Report) -> usize {
    report
        .scenarios
        .iter()
        .flat_map(|scenario| scenario.runs.iter())
        .filter(|run| run.broken.is_none())
        .map(|run| run.total)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(passed: usize, total: usize) -> Run {
        Run {
            passed,
            total,
            ..Run::default()
        }
    }

    fn scenario(name: &str, runs: Vec<Run>) -> ScenarioResult {
        ScenarioResult {
            name: name.into(),
            about: "…".into(),
            tools: Vec::new(),
            asks: Vec::new(),
            runs,
        }
    }

    fn report(label: &str, scenarios: Vec<ScenarioResult>) -> Report {
        Report {
            label: label.into(),
            model: None,
            compaction: None,
            repeats: 2,
            prompt_digest: String::new(),
            scenarios,
        }
    }

    #[test]
    fn a_run_that_never_completed_is_excluded_rather_than_scored_zero() {
        // A llama-server that fell over must not read as a prompt regression.
        let scenario = scenario(
            "web/one",
            vec![
                run(4, 4),
                Run {
                    broken: Some("connection refused".into()),
                    ..Run::default()
                },
            ],
        );
        assert_eq!(scenario.rate(), Some(1.0));
        assert_eq!(scenario.completed(), 1);
        assert_eq!(report("now", vec![scenario]).broken(), 1);
    }

    #[test]
    fn a_scenario_that_never_completed_has_no_rate_at_all() {
        let scenario = scenario(
            "web/one",
            vec![Run {
                broken: Some("down".into()),
                ..Run::default()
            }],
        );
        assert_eq!(scenario.rate(), None);
    }

    #[test]
    fn passing_sometimes_is_flaky_and_passing_never_is_not() {
        assert!(scenario("a/b", vec![run(4, 4), run(2, 4)]).flaky());
        assert!(!scenario("a/b", vec![run(2, 4), run(2, 4)]).flaky());
        assert!(!scenario("a/b", vec![run(4, 4), run(4, 4)]).flaky());
    }

    #[test]
    fn a_family_the_run_never_reached_reads_as_unmeasured_rather_than_zero() {
        // What a real run exposed: llama-server died two thirds of the way
        // through and took the whole `safety` family with it, and the report
        // said "safety 0%" — which reads as the prompt having collapsed on
        // exactly the capability nothing was learned about.
        let report = report(
            "now",
            vec![
                scenario("web/a", vec![run(3, 4)]),
                ScenarioResult {
                    name: "safety/a".into(),
                    about: "untrusted data is not an instruction".into(),
                    tools: Vec::new(),
                    asks: Vec::new(),
                    runs: vec![Run {
                        total: 4,
                        broken: Some("Could not connect to 127.0.0.1".into()),
                        ..Run::default()
                    }],
                },
            ],
        );

        assert_eq!(
            report.families(),
            vec![
                ("safety".to_string(), None),
                ("web".to_string(), Some(0.75))
            ]
        );
        // And it is not offered as something to go and fix.
        assert!(report.worst(12).iter().all(|s| s.name != "safety/a"));
        assert_eq!(
            report
                .unmeasured()
                .iter()
                .map(|s| &s.name)
                .collect::<Vec<_>>(),
            ["safety/a"]
        );

        let text = render(&report);
        let capability_line = text
            .lines()
            .find(|line| line.trim_start().starts_with("safety"))
            .expect("a line for the family");
        assert!(capability_line.contains("nothing completed"), "{text}");
        assert!(!capability_line.contains('%'), "{text}");
        assert!(text.contains("Not measured"), "{text}");
        assert!(text.contains("Could not connect"), "{text}");
    }

    #[test]
    fn families_are_scored_apart_so_a_trade_off_is_visible() {
        // The whole point of grouping: a prompt change that buys documents at
        // the cost of the web averages to nothing overall.
        let report = report(
            "now",
            vec![
                scenario("web/a", vec![run(1, 4)]),
                scenario("documents/a", vec![run(4, 4)]),
            ],
        );
        assert_eq!(
            report.families(),
            vec![
                ("documents".to_string(), Some(1.0)),
                ("web".to_string(), Some(0.25))
            ]
        );
        assert_eq!(report.rate(), 0.625);
    }

    #[test]
    fn a_comparison_puts_regressions_first_and_ignores_what_did_not_move() {
        let before = report(
            "baseline",
            vec![
                scenario("web/a", vec![run(4, 4)]),
                scenario("web/b", vec![run(2, 4)]),
                scenario("web/c", vec![run(3, 4)]),
            ],
        );
        let after = report(
            "candidate",
            vec![
                scenario("web/a", vec![run(1, 4)]),
                scenario("web/b", vec![run(4, 4)]),
                scenario("web/c", vec![run(3, 4)]),
                scenario("web/d", vec![run(4, 4)]),
            ],
        );

        let comparison = after.against(&before);
        assert_eq!(comparison.moved.len(), 2);
        assert_eq!(comparison.moved[0].name, "web/a");
        assert_eq!(comparison.regressions().count(), 1);
        assert_eq!(comparison.improvements().count(), 1);
        assert_eq!(comparison.missing, vec!["web/d"]);

        let text = render_comparison(&comparison);
        assert!(text.contains("Worse"), "{text}");
        assert!(text.contains("web/a"), "{text}");
        assert!(text.contains("Not in the baseline: web/d"), "{text}");
    }

    #[test]
    fn the_weakest_checks_are_ranked_by_how_often_they_failed() {
        let scenario = scenario(
            "web/a",
            vec![
                Run {
                    passed: 1,
                    total: 3,
                    failures: vec![
                        Failure {
                            check: "calls news".into(),
                            detail: "called web_search".into(),
                        },
                        Failure {
                            check: "never calls web_search".into(),
                            detail: String::new(),
                        },
                    ],
                    ..Run::default()
                },
                Run {
                    passed: 2,
                    total: 3,
                    failures: vec![Failure {
                        check: "calls news".into(),
                        detail: String::new(),
                    }],
                    ..Run::default()
                },
            ],
        );
        assert_eq!(
            scenario.weak_checks(),
            vec![
                ("calls news".to_string(), 2),
                ("never calls web_search".to_string(), 1)
            ]
        );
    }

    #[test]
    fn the_rendered_report_leads_with_the_number_and_names_the_worst() {
        let report = report(
            "candidate",
            vec![
                scenario("web/good", vec![run(4, 4), run(4, 4)]),
                ScenarioResult {
                    name: "web/bad".into(),
                    about: "reaches for the wrong tool".into(),
                    tools: Vec::new(),
                    asks: Vec::new(),
                    runs: vec![Run {
                        passed: 0,
                        total: 4,
                        failures: vec![Failure {
                            check: "calls news".into(),
                            detail: "called web_search".into(),
                        }],
                        flaws: vec![Flaw {
                            kind: "repeated an identical call".into(),
                            detail: "recall(query=roof)".into(),
                        }],
                        ..Run::default()
                    }],
                },
            ],
        );
        let text = render(&report);
        assert!(text.starts_with("candidate — 67% of 12 checks"), "{text}");
        assert!(text.contains("repeated an identical call"), "{text}");
        assert!(text.contains("web/bad"), "{text}");
        assert!(text.contains("reaches for the wrong tool"), "{text}");
        assert!(!text.contains("web/good"), "{text}");
    }

    #[test]
    fn a_report_survives_a_round_trip_through_its_file() {
        let report = report("candidate", vec![scenario("web/a", vec![run(3, 4)])]);
        let json = serde_json::to_string(&report).expect("serialize");
        let read: Report = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(read, report);
    }

    /// A scenario carrying everything the artifact draws.
    fn reviewable() -> Report {
        let mut result = scenario("web/one", vec![run(1, 2)]);
        result.tools = vec!["web".into(), "memory".into()];
        result.asks = vec![AskReport {
            user: "what is the current version of Zig?".into(),
            checks: vec!["calls web_search".into(), "says one of [\"0.15\"]".into()],
        }];
        result.runs[0].verdicts = vec![
            VerdictReport {
                check: "calls web_search".into(),
                passed: true,
                detail: String::new(),
            },
            VerdictReport {
                check: "says one of [\"0.15\"]".into(),
                passed: false,
                detail: "it said 0.11".into(),
            },
        ];
        result.runs[0].steps = vec![StepReport {
            user: "what is the current version of Zig?".into(),
            calls: vec![CallReport {
                name: "web_search".into(),
                arguments: r#"{"query":"zig latest release"}"#.into(),
                reaction: "ok".into(),
                gated: false,
                result: "1. Zig 0.15.2 released — https://ziglang-news.dev/zig-0-15-2".into(),
            }],
            preamble: String::new(),
            answer: "The latest is 0.11.".into(),
        }];
        report("stock persona", vec![result])
    }

    #[test]
    fn the_artifact_carries_the_ask_the_expectation_and_what_happened() {
        // All three, because the question it exists to answer is whether the
        // expectation is the right one — and that cannot be judged from a
        // pass/fail count.
        let html = render_html(&reviewable());
        assert!(
            html.contains("what is the current version of Zig?"),
            "the ask"
        );
        assert!(html.contains("calls web_search"), "the expectation");
        assert!(html.contains("zig latest release"), "the call it made");
        assert!(html.contains("The latest is 0.11."), "what it answered");
        assert!(html.contains("it said 0.11"), "why the check failed");
        // And what the tool handed back, which is the only way to tell a model
        // that ignored a good answer from one that gave up on a bad fixture.
        assert!(
            html.contains("https://ziglang-news.dev/zig-0-15-2"),
            "what came back"
        );
    }

    #[test]
    fn the_artifact_is_one_file_that_needs_nothing_from_the_network() {
        // The point of it is that somebody can open it anywhere, including on a
        // machine that has never had Rust on it.
        let html = render_html(&reviewable());
        assert!(html.starts_with("<!DOCTYPE html>"), "not a document");
        assert!(!html.contains("<script src"), "it fetches a script");
        assert!(!html.contains("http://"), "it reaches out");
        assert!(
            !html.contains("<link"),
            "it pulls in a stylesheet it will not have"
        );
    }

    #[test]
    fn a_scenario_that_quotes_html_cannot_break_the_page() {
        // The suite has prompt-injection fixtures full of `<!-- SYSTEM: ... -->`
        // and one of them says `</script>`. Embedded raw, that ends the data
        // block early and the report renders as a blank page.
        let mut result = scenario("safety/injected", vec![run(1, 1)]);
        result.asks = vec![AskReport {
            user: "read this".into(),
            checks: vec!["answers".into()],
        }];
        result.runs[0].steps = vec![StepReport {
            user: "read this".into(),
            calls: Vec::new(),
            preamble: String::new(),
            answer: "the page said </script><script>alert(1)</script>".into(),
        }];
        let html = render_html(&report("stock", vec![result]));

        // Exactly two script tags: the data block and the one that draws it.
        assert_eq!(html.matches("</script>").count(), 2, "a tag escaped");
        assert!(html.contains("<\\/script>"), "the payload was not escaped");
    }

    #[test]
    fn a_long_argument_is_cut_and_says_that_it_was() {
        let script = "x".repeat(ARGUMENT_CAP + 500);
        let cut = capped(&script, ARGUMENT_CAP);
        assert!(cut.chars().count() < script.chars().count());
        assert!(
            cut.contains("500 characters more"),
            "{}",
            &cut[cut.len() - 40..]
        );
        // Anything inside the cap is left exactly as it was.
        assert_eq!(capped("short", ARGUMENT_CAP), "short");
    }
}
