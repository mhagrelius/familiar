//! Long-form conversational recall: what a thread still holds ten turns in.
//!
//! The prompt suite in [`super::suite`] asks whether the model works the way the
//! prompt told it to. This asks something the prompt cannot fix — whether a fact
//! stated at turn two is still there at turn ten. Two things move that number
//! and they are worth telling apart:
//!
//! * the model, which is why these scenarios offer **no tools at all**. A thread
//!   that can reach for `recall` or the web has a second way to look right, and
//!   the question here is what the context alone still holds. Run one arm per
//!   model and compare with `--baseline`.
//! * **compaction**, which is why the driver can fold between turns. The gap
//!   between `--compaction off` and `--compaction headings` is what folding
//!   costs, measured rather than assumed.
//!
//! Every scenario is exactly [`TURNS`] asks, which is chosen against
//! `Settings::keep_recent_turns` (6): by the last ask, turns two through four
//! have been folded and turn one has not. So the facts are planted in that
//! window on purpose, and where in a message they are planted decides whether
//! [`crate::model::compaction::Headings`] can carry them — it keeps the first
//! line of each *user* message and nothing else, so a short fact survives a fold
//! and the same fact three lines down does not. Scenarios exist on both sides of
//! that line, because a suite where every case fails tells you nothing about
//! which part failed.
//!
//! Probes are lexical, like the rest of the harness — no model judges another
//! model here. That works only because every planted fact is a distinctive
//! token (a proper noun, a figure, a weekday), and none of them appear in
//! [`super::scenario::AMBIENT`], so a hit cannot have come from the memory block
//! instead of the conversation.

use super::check::Check;
use super::scenario::{nothing, out_of_the_box, Scenario};

/// Asks per scenario. Past `keep_recent_turns` by enough to fold the planted
/// window, and no further — every extra turn is another round trip against a
/// local server that falls over under a long unbroken run, and seven scenarios
/// at ten turns is already a pass measured in hours.
pub const TURNS: usize = 10;

/// Filler that carries no facts and needs no tools.
///
/// Deliberately off-topic: a thread that keeps circling its own subject
/// rehearses the fact under test on every turn, and then measures nothing.
const FILLER: [&str; 5] = [
    "Unrelated aside — what's the difference between a stack and a queue?",
    "What does the word 'quixotic' mean?",
    "Roughly how long does bread dough need to prove at room temperature?",
    "Why do onions make you cry?",
    "What's a reasonable way to keep a shopping list you'll actually use?",
];

/// Turn one is never folded, so it is where the thread's subject goes rather
/// than a fact under test.
fn opener(scenario: Scenario, said: &'static str) -> Scenario {
    scenario.ask(said, [Check::Answers])
}

/// The five neutral turns between the plant and the probe.
fn filler(mut scenario: Scenario) -> Scenario {
    for said in FILLER {
        scenario = scenario.ask(said, [Check::Answers]);
    }
    scenario
}

pub fn all() -> Vec<Scenario> {
    vec![
        short_fact(),
        buried_fact(),
        superseded_value(),
        no_such_fact(),
        first_message_anchor(),
        tooled_fact_at_distance(),
        tooled_search_at_distance(),
    ]
}

/// Whether a scenario is one of the isolated ones, which is also the line
/// between "what did the model retain" and "did the prompt survive the
/// distance". The report reads better split on it than averaged over it.
pub fn is_isolated(scenario: &Scenario) -> bool {
    !scenario.name.contains("/tooled-")
}

/// The control. Both facts are short first lines of user messages, which is
/// exactly what `Headings` keeps, so a fold should cost nothing here. When this
/// one drops it is the model's context handling, not the summarizer's.
fn short_fact() -> Scenario {
    filler(
        opener(
            Scenario::new(
                "recall/short-fact",
                "a one-line fact from turn two, asked for at turn ten",
                nothing(),
            ),
            "I'm putting a trip together and I'd like you to help me keep the details straight.",
        )
        .ask(
            "My flight lands in Ljubljana at 6:40am on the 12th.",
            [Check::Answers],
        )
        .ask(
            "The hotel I've booked is called the Bregenz.",
            [Check::Answers],
        )
        .ask(
            "I'll have about forty minutes between landing and the first meeting.",
            [Check::Answers],
        ),
    )
    .ask(
        "Back to the trip: what time does my flight land, and where am I staying?",
        [
            Check::Says(&["6:40"]),
            Check::Says(&["Bregenz"]),
            Check::Answers,
        ],
    )
}

/// The same distance, with the facts three lines into the message instead of on
/// the first one. `Headings` keeps `first_line` of a user message and drops the
/// rest, so this is the case a fold destroys and a full history does not — the
/// single clearest reading of what the summarizer costs.
fn buried_fact() -> Scenario {
    filler(
        opener(
            Scenario::new(
                "recall/buried-fact",
                "facts below the first line of a long turn-two message",
                nothing(),
            ),
            "I'm going to paste some notes as we go — just keep track of them for me.",
        )
        .ask(
            "Here are my notes from the kickoff, roughly as I typed them:\n\n\
         Budget signed off at 48,000 euros.\n\
         The vendor we picked is Okonkwo Systems.\n\
         Deadline is the 3rd of November.",
            [Check::Answers],
        )
        .ask("That's everything from the kickoff.", [Check::Answers])
        .ask(
            "I'll send the follow-up notes later this week.",
            [Check::Answers],
        ),
    )
    .ask(
        "What did we agree the budget was, and who is the vendor?",
        [
            Check::Says(&["48,000", "48000", "48 000"]),
            Check::Says(&["Okonkwo"]),
            Check::Answers,
        ],
    )
}

/// A value stated, then corrected. Both are short user first lines, so both
/// survive a fold — as two bullets with nothing to say one replaced the other.
/// Whether the model still answers with the correction is the thing being read.
fn superseded_value() -> Scenario {
    filler(
        opener(
            Scenario::new(
                "recall/superseded-value",
                "a date given at turn two and changed at turn four",
                nothing(),
            ),
            "Help me keep a couple of scheduling details straight over the next few messages.",
        )
        .ask(
            "Let's put the design review on Tuesday the 9th.",
            [Check::Answers],
        )
        .ask(
            "Noted. Anything else I should be thinking about for it?",
            [Check::Answers],
        )
        .ask(
            "Change of plan — move the design review to Thursday the 11th.",
            [Check::Answers],
        ),
    )
    .ask(
        "Which day is the design review on? Just the day is fine.",
        [Check::Says(&["Thursday"]), Check::Answers],
    )
}

/// The other half of recall: not inventing what was never said. A thread this
/// long gives a model plenty of nearby detail to build a plausible answer out
/// of, and the only right answer is that it does not have one.
fn no_such_fact() -> Scenario {
    filler(
        opener(
            Scenario::new(
                "recall/no-such-fact",
                "asked at turn ten for a detail never given",
                nothing(),
            ),
            "I'm organising a small event and I'll give you the details as they firm up.",
        )
        .ask("The venue is the Aldersgate Institute.", [Check::Answers])
        .ask("It's on the afternoon of the 22nd.", [Check::Answers])
        .ask(
            "Catering is sorted — sandwiches and urns of tea.",
            [Check::Answers],
        ),
    )
    .ask(
        "What was the room number I gave you for the venue?",
        [
            Check::Says(&[
                "didn't",
                "did not",
                "haven't",
                "have not",
                "don't have",
                "do not have",
                "wasn't",
                "was not",
                "no room number",
                "not mentioned",
                "not given",
            ]),
            Check::Answers,
        ],
    )
}

/// A standing instruction in turn one, which `compact` is written never to fold
/// away. It should therefore hold in both arms — and if it does not, the fault
/// is the model losing the top of a long context rather than anything the
/// summarizer did.
fn first_message_anchor() -> Scenario {
    filler(
        opener(
            Scenario::new(
                "recall/first-message-anchor",
                "a standing instruction from the never-folded first turn",
                nothing(),
            ),
            "For the whole of this conversation, give me any measurement in metric only, \
         with no imperial conversion in brackets.",
        )
        .ask(
            "Let's start easy. What's a good pace for a first-time runner?",
            [Check::Answers],
        )
        .ask(
            "How much water should someone drink on a long walk?",
            [Check::Answers],
        )
        .ask(
            "Is it worth stretching before or after a run?",
            [Check::Answers],
        ),
    )
    .ask(
        "How long is a marathon?",
        [
            Check::Says(&["km", "kilomet"]),
            Check::NeverSays(&["mile"]),
            Check::Answers,
        ],
    )
}

/// The realistic arm of the fact probe: the same question at the same distance,
/// with the tools a fresh install actually has switched on.
///
/// Two ways to be wrong now, and only the first is about memory. A model that
/// has kept the fact can still reach for `recall` or the web to answer it,
/// which is the over-searching the prompt spends a paragraph on — and every
/// scenario that catches that today asks at turn one or two. This asks at turn
/// ten, which is the only place guidance decay is visible.
fn tooled_fact_at_distance() -> Scenario {
    filler(
        opener(
            Scenario::new(
                "recall/tooled-fact-at-distance",
                "a conversational fact at turn ten, with tools available to over-reach for",
                out_of_the_box(),
            ),
            "I'm putting a trip together and I'd like you to help me keep the details straight.",
        )
        .ask(
            "My flight lands in Ljubljana at 6:40am on the 12th.",
            [Check::Answers],
        )
        .ask(
            "The hotel I've booked is called the Bregenz.",
            [Check::Answers],
        )
        .ask(
            "I'll have about forty minutes between landing and the first meeting.",
            [Check::Answers],
        ),
    )
    .ask(
        "Back to the trip: what time does my flight land, and where am I staying?",
        [
            Check::Says(&["6:40"]),
            Check::Says(&["Bregenz"]),
            // It was told this. Looking it up is the failure even when the
            // answer that comes back is right.
            Check::NoTools,
            Check::Answers,
        ],
    )
}

/// The same decay, measured in the other direction.
///
/// Nine turns of chat is a long time for "go and look when the answer is
/// current" to hold, and a model that has settled into answering from itself
/// will quietly stop searching. Without this, `tooled-fact-at-distance` could
/// be passed by a model that has simply given up on tools altogether.
fn tooled_search_at_distance() -> Scenario {
    filler(
        opener(
            Scenario::new(
                "recall/tooled-search-at-distance",
                "a question at turn ten that still needs a tool, after nine that did not",
                out_of_the_box(),
            ),
            "I'd like to talk through a few odds and ends — nothing urgent.",
        )
        .ask("No particular agenda today.", [Check::Answers])
        .ask(
            "I've been meaning to tidy up my reading list.",
            [Check::Answers],
        )
        .ask("Nothing to add to it yet.", [Check::Answers]),
    )
    .ask(
        "Changing the subject — what's the latest stable release of Zig, and when did it land?",
        [Check::CallsAny(&["web_search", "news"]), Check::Answers],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::compaction::{self, Fold, Headings};
    use crate::model::wire::{Message, Role};

    /// The suite is built around this number matching the application's
    /// default, so a change to one has to be a change to both.
    #[test]
    fn every_scenario_is_long_enough_to_be_folded() {
        let keep_recent = crate::model::settings::Settings::default().keep_recent_turns;
        assert!(
            TURNS > keep_recent,
            "{TURNS} asks would never fold at keep_recent_turns {keep_recent}"
        );
        for scenario in all() {
            assert_eq!(scenario.asks.len(), TURNS, "{}", scenario.name);
        }
    }

    #[test]
    fn an_isolated_scenario_offers_nothing_to_look_it_up_with() {
        // The whole point of that half: a hit has to have come from the
        // conversation rather than from a search.
        for scenario in all().iter().filter(|s| is_isolated(s)) {
            assert!(
                super::super::offered(scenario).is_empty(),
                "{} offers tools, so a pass could be a lookup",
                scenario.name
            );
        }
    }

    #[test]
    fn a_tooled_scenario_offers_the_tools_it_is_meant_to_over_reach_for() {
        // A `NoTools` check passes trivially when there was nothing to call.
        for scenario in all().iter().filter(|s| !is_isolated(s)) {
            let offered = super::super::offered(scenario);
            assert!(
                offered.iter().any(|tool| tool == "web_search"),
                "{} cannot demonstrate over-searching: {offered:?}",
                scenario.name
            );
            assert!(
                offered.iter().any(|tool| tool == "recall"),
                "{} cannot demonstrate over-recalling: {offered:?}",
                scenario.name
            );
        }
    }

    #[test]
    fn both_arms_of_the_tooled_pair_are_present() {
        // One alone is passable by a model that stopped calling tools at all,
        // or by one that calls them for everything.
        let tooled: Vec<&str> = all()
            .iter()
            .filter(|s| !is_isolated(s))
            .map(|s| s.name)
            .collect();
        assert!(tooled.len() >= 2, "{tooled:?}");
    }

    #[test]
    fn no_planted_fact_is_already_in_the_ambient_block() {
        // A token the memory block also carries would score a pass without the
        // conversation having been read at all.
        let ambient = super::super::scenario::AMBIENT.to_lowercase();
        for scenario in all() {
            for ask in &scenario.asks {
                for check in &ask.checks {
                    let (Check::Says(words) | Check::NeverSays(words)) = check else {
                        continue;
                    };
                    for word in *words {
                        assert!(
                            !ambient.contains(&word.to_lowercase()),
                            "{} probes for {word:?}, which AMBIENT already says",
                            scenario.name
                        );
                    }
                }
            }
        }
    }

    /// Replay a scenario's asks as a thread, folding at each boundary the way
    /// the driver does, and return what the model would be sent at the end.
    ///
    /// `Headings` rather than the real summarizer, because a unit test cannot
    /// have a server — which is exactly why the `headings` arm is the floor the
    /// `model` arm gets compared against rather than a thing anyone ships.
    fn folded_view(scenario: &Scenario, keep_recent: usize) -> String {
        let mut history: Vec<Message> = Vec::new();
        let mut fold: Option<Fold> = None;
        for ask in &scenario.asks {
            history.push(Message::user(ask.user));
            while let Some((chunk, more)) =
                compaction::to_summarize(&history, fold.as_ref(), keep_recent)
            {
                fold = Some(compaction::extend(fold.as_ref(), &chunk, more, &Headings));
            }
            history.push(Message::assistant("(an answer)"));
        }
        compaction::view(&history, fold.as_ref())
            .iter()
            .map(|message| message.text_of().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_short_fact_survives_a_fold_and_the_buried_one_does_not() {
        // This is the contrast the two scenarios exist to create. If it ever
        // stops holding, the arms are no longer measuring different things and
        // the summarizer number means something else.
        let short = folded_view(&short_fact(), 6);
        assert!(short.contains("6:40"), "{short}");
        assert!(short.contains("Bregenz"), "{short}");

        let buried = folded_view(&buried_fact(), 6);
        assert!(
            !buried.contains("Okonkwo"),
            "the buried fact survived, so the scenario tests nothing:\n{buried}"
        );
        assert!(!buried.contains("48,000"), "{buried}");
    }

    #[test]
    fn the_first_message_is_still_there_at_the_last_turn() {
        let anchored = folded_view(&first_message_anchor(), 6);
        assert!(anchored.contains("metric only"), "{anchored}");
    }

    #[test]
    fn a_ten_turn_thread_folds_three_turns_away_one_at_a_time() {
        // Folding is incremental, because the driver and the application both
        // bring the fold up to date at every turn boundary rather than once at
        // the end. Guards the arithmetic the module docs rest on — that turns
        // two through four are the ones gone by the last ask, which is where
        // the facts are planted.
        let mut history: Vec<Message> = Vec::new();
        let mut fold: Option<Fold> = None;
        let mut folds = Vec::new();
        for turn in 1..=TURNS {
            history.push(Message::user(format!("q{turn}")));
            while let Some((chunk, more)) = compaction::to_summarize(&history, fold.as_ref(), 6) {
                fold = Some(compaction::extend(fold.as_ref(), &chunk, more, &Headings));
                folds.push((turn, more));
            }
            history.push(Message::assistant("a"));
        }

        assert_eq!(fold.as_ref().map(|fold| fold.covers), Some(3));
        // The degenerate zero-turn fold is gone: every pass moved something.
        assert!(folds.iter().all(|(_, more)| *more > 0), "{folds:?}");
        // And it starts at turn eight, not seven — seven had nothing to fold.
        assert_eq!(folds.first().map(|(turn, _)| *turn), Some(8));

        let users: Vec<String> = compaction::view(&history, fold.as_ref())
            .iter()
            .filter(|message| message.role == Role::User)
            .map(|message| message.text_of().to_string())
            .collect();
        assert_eq!(users, ["q1", "q5", "q6", "q7", "q8", "q9", "q10"]);
    }

    #[test]
    fn the_turn_that_used_to_fold_nothing_now_folds_nothing_at_all() {
        // At exactly `keep_recent + 1` the old fold ran anyway: it summarised
        // no turn, dropped the first answer without trace, and announced
        // "Summarized 0 earlier turns". Now there is simply nothing to do.
        let mut history: Vec<Message> = Vec::new();
        for turn in 1..=7 {
            history.push(Message::user(format!("q{turn}")));
            history.push(Message::assistant(format!("a{turn}")));
        }
        history.pop();

        assert_eq!(compaction::to_summarize(&history, None, 6), None);
        assert_eq!(compaction::view(&history, None), history);
    }
}
