//! What a project could switch on, and the one tool that switches it on.
//!
//! # Why this exists rather than everything being on
//!
//! A project is its tool bundle, and most of them are off. That is right and it
//! is also a discovery problem: somebody who opens a new conversation and asks
//! for a spreadsheet is told the assistant cannot make one, when what is true is
//! that it *could* if a switch three menus away were flipped.
//!
//! The obvious answer — switch everything on — was measured and is worse. With
//! the escalation note added to the suite's everything-on tool set, the planner
//! family scored 92% at six repeats; without it, 94%. Every capability costs a
//! paragraph of system prompt that every turn then carries, and a small local
//! model with a long tool list reaches for the wrong one. Eleven capabilities on
//! by default would buy discovery by making every answer slightly worse.
//!
//! So: the *names* are always in the prompt and the *tools* are not. One line
//! each, no guidance, no declarations — a menu rather than eleven paragraphs —
//! and one tool that turns a capability on for this project when the
//! conversation turns out to need it. The tools appear on the next round, which
//! is why the request is rebuilt per round rather than per turn.
//!
//! Every string in this file is read by the model, so none of them says
//! "project". The window calls a workspace a project; the prompt calls it a
//! workspace folder, because the model already has Planner's projects and the
//! memory tool's `project` kind and a third meaning would cost both of them.
//! [`tests::the_prompt_never_learns_the_word_project`] is the line.
//!
//! # What this does not do
//!
//! It does not weaken a gate. Everything switched on this way keeps the gate it
//! always had: `send` and `delete` still ask, `escalate` still asks, writing to
//! the workspace still asks. Switching a capability on is not permission to use
//! it, it is permission to *offer* it — the difference matters most for
//! `escalate`, where the leakage control was never the switch but the per-call
//! approval of the exact words.
//!
//! Nor does it offer what cannot work. A capability whose requirements are
//! missing — no podman for the sandbox, no account for mail, no folder for the
//! workspace — is left out of the catalogue entirely, because the rule the rest
//! of this application keeps is that a tool which always fails is worse than one
//! that was never offered.

use super::project::ToolSet;

/// One thing a project can switch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    /// What the model asks for, and what the switch is called.
    pub name: &'static str,
    /// One line, written for somebody deciding whether this is the thing they
    /// need. Deliberately not the guidance: the guidance arrives once the
    /// capability is on, and costs nothing until then.
    pub summary: &'static str,
}

/// Everything switchable, in the order the catalogue lists it.
///
/// `memory`, `web`, `weather` and `workflow` are not here. They are on out of
/// the box, they have no requirements to check, and a menu entry for something
/// already switched on is noise.
pub const ALL: &[Capability] = &[
    Capability {
        name: "workspace",
        summary: "read and write files in the user's workspace folder",
    },
    Capability {
        name: "documents",
        summary: "make Word documents, Excel workbooks, PowerPoint decks and PDFs, and read \
                  PDFs the user already has",
    },
    Capability {
        name: "python",
        summary: "write and run Python in a container with no network — for arithmetic, dates, \
                  sorting, and reading spreadsheets",
    },
    Capability {
        name: "github",
        summary: "pull requests, issues and workflow runs, through the `gh` command line",
    },
    Capability {
        name: "planner",
        summary: "the user's task list — read it, add to it, change it",
    },
    Capability {
        name: "dynamo",
        summary: "the house's own electricity use, per circuit, live and historical",
    },
    Capability {
        name: "magpie",
        summary: "turn a video or audio link into a transcript",
    },
    Capability {
        name: "mail",
        summary: "search, read, label and file the user's email; sending and deleting ask first",
    },
    // Deliberately worded around what somebody *asks for* rather than what it
    // is. Asked to "set up a morning briefing" with only `planner` in the menu,
    // the assistant made a task reminding the user to ask for a briefing — the
    // nearest available thing — and then stated that it had no scheduler. The
    // entry has to be recognisable from the request, or the same substitution
    // happens with a longer menu.
    Capability {
        name: "scheduling",
        summary: "run this chat on a schedule and notify them — a morning briefing, a weekly \
                  check; work you do, not a reminder for them to do it",
    },
    Capability {
        name: "escalate",
        summary: "put one question to a stronger model in the cloud, which the user approves \
                  word for word before it leaves the machine",
    },
];

pub fn named(name: &str) -> Option<&'static Capability> {
    let wanted = name.trim().to_lowercase();
    ALL.iter().find(|capability| capability.name == wanted)
}

/// Whether a capability is on in this set.
pub fn is_on(tools: &ToolSet, name: &str) -> bool {
    match name {
        "workspace" => tools.workspace,
        "documents" => tools.documents,
        "python" => tools.python,
        "github" => tools.github,
        "planner" => tools.planner,
        "dynamo" => tools.dynamo,
        "magpie" => tools.magpie,
        "mail" => tools.mail,
        "escalate" => tools.escalate,
        "scheduling" => tools.scheduling,
        "workflow" => tools.workflow,
        _ => false,
    }
}

/// Switch one on. Returns whether it was off before, so a caller can tell the
/// difference between doing something and being asked for what is already true.
pub fn switch_on(tools: &mut ToolSet, name: &str) -> bool {
    let was = is_on(tools, name);
    match name {
        "workspace" => tools.workspace = true,
        "documents" => {
            // Documents are written *into* the workspace, so one without the
            // other is a capability that cannot do its one job. Asking for it
            // means asking for both.
            tools.documents = true;
            tools.workspace = true;
        }
        "python" => tools.python = true,
        "github" => {
            tools.github = true;
            tools.workspace = true;
        }
        "planner" => tools.planner = true,
        "dynamo" => tools.dynamo = true,
        "magpie" => tools.magpie = true,
        "mail" => tools.mail = true,
        "escalate" => tools.escalate = true,
        "scheduling" => tools.scheduling = true,
        "workflow" => tools.workflow = true,
        _ => return false,
    }
    !was
}

/// What the model may ask for: switchable, not already on, and able to work.
///
/// `usable` answers "could this actually run on this machine, for this user" —
/// podman and an image for the sandbox, an account for mail, a folder for the
/// workspace. It is the application's question, not this module's, which is why
/// it arrives as a closure.
pub fn offerable<F>(tools: &ToolSet, usable: F) -> Vec<&'static Capability>
where
    F: Fn(&str) -> bool,
{
    // A project with every switch off is a deliberate plain conversation, and
    // handing it a menu of eight capabilities is the opposite of what was
    // asked for. It is also what the recall suite's isolated arm runs under,
    // where the whole measurement is that there is nothing to look anything up
    // with — but the rule is right on its own terms and would be right if that
    // suite did not exist.
    if !is_anything_on(tools) {
        return Vec::new();
    }
    ALL.iter()
        .filter(|capability| !is_on(tools, capability.name))
        .filter(|capability| usable(capability.name))
        .collect()
}

/// The catalogue note for the system prompt, or `None` when there is nothing
/// left to offer.
///
/// Short on purpose. This is the thing that has to stay cheap: it is carried by
/// every turn of every conversation, and the moment it grows into eleven
/// paragraphs it has become the design it was written to avoid.
pub fn catalogue(offerable: &[&'static Capability]) -> Option<String> {
    if offerable.is_empty() {
        return None;
    }
    // The github entry says "workflow runs", which is the phrase the `workflow`
    // capability collides with. The arm decides whether it stays that way, and
    // it is applied here rather than `ALL` carrying two copies of the line.
    let overlap = crate::model::workflow::Overlap::current();
    let entries: Vec<String> = offerable
        .iter()
        .map(|capability| {
            format!(
                "- `{}` — {}",
                capability.name,
                overlap.applied(capability.summary)
            )
        })
        .collect();
    Some(format!(
        "Some capabilities are switched off in this conversation. You cannot use them yet, but \
         you can turn one on with `use_tools` when the user asks for something that needs it:\n\
         {}\n\n\
         Turn one on when the conversation actually calls for it — the user asks for a \
         spreadsheet, or about their email — and then do the thing they asked for in the same \
         turn. Do not turn one on to look prepared, and do not list these to the user unless \
         they ask what you can do. Everything that asked for approval before still asks for \
         it.",
        entries.join("\n")
    ))
}

/// Whether this project has any capability at all, including the three that are
/// on out of the box and are not in [`ALL`].
fn is_anything_on(tools: &ToolSet) -> bool {
    tools.memory
        || tools.web
        || tools.weather
        || tools.workflow
        || ALL.iter().any(|capability| is_on(tools, capability.name))
}

/// Whether a program is on the `PATH`.
///
/// Half of "could this work" is "is the thing installed", and every sibling
/// capability here is a command line. Cheap, but not free — a caller that asks
/// per round should remember the answer, because the answer does not change
/// while the application is running.
pub fn installed(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
}

/// What comes back after switching some on: what changed, and what to do next.
pub fn switched(turned_on: &[String], already: &[String]) -> String {
    let mut said = String::new();
    if !turned_on.is_empty() {
        said.push_str(&format!(
            "Switched on: {}. The tools are available from your next call — make it now and \
             carry on with what the user asked for.",
            turned_on.join(", ")
        ));
    }
    if !already.is_empty() {
        if !said.is_empty() {
            said.push(' ');
        }
        said.push_str(&format!(
            "Already on: {} — those tools were already available to you.",
            already.join(", ")
        ));
    }
    said
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nothing() -> ToolSet {
        ToolSet {
            memory: true,
            web: true,
            weather: true,
            workspace: false,
            github: false,
            documents: false,
            planner: false,
            magpie: false,
            dynamo: false,
            python: false,
            escalate: false,
            mail: false,
            scheduling: false,
            workflow: false,
        }
    }

    /// The window calls a workspace a **project**. The model must never hear
    /// it: Planner's `#Project` and the memory tool's `project` kind are both
    /// real, both scored by their own eval families, and a third meaning would
    /// degrade them silently — as a couple of points on a suite nobody was
    /// running that day, not as anything that looks like a bug.
    #[test]
    fn the_prompt_never_learns_the_word_project() {
        let everything: Vec<&'static Capability> = ALL.iter().collect();
        let mut prompt = catalogue(&everything).expect("a catalogue");
        prompt.push_str(&switched(
            &["workspace".to_string()],
            &["documents".to_string()],
        ));
        assert!(
            !prompt.to_lowercase().contains("project"),
            "the catalogue names a project: {prompt}"
        );
    }

    #[test]
    fn every_capability_can_be_switched_on_by_its_own_name() {
        // A name in the catalogue that nothing acts on would be a menu entry
        // that does nothing when ordered.
        for capability in ALL {
            let mut tools = nothing();
            assert!(
                switch_on(&mut tools, capability.name),
                "{} did not switch on",
                capability.name
            );
            assert!(
                is_on(&tools, capability.name),
                "{} switched on and reads as off",
                capability.name
            );
        }
    }

    #[test]
    fn switching_on_something_already_on_changes_nothing_and_says_so() {
        let mut tools = nothing();
        assert!(switch_on(&mut tools, "planner"));
        assert!(!switch_on(&mut tools, "planner"));
        assert!(tools.planner);
    }

    #[test]
    fn an_unknown_name_switches_nothing_on() {
        let mut tools = nothing();
        assert!(!switch_on(&mut tools, "root"));
        assert_eq!(tools, nothing());
    }

    #[test]
    fn documents_bring_the_workspace_they_are_written_into() {
        // `create_document` writes a file. Switched on alone it is eight tools
        // that all fail on the one thing they do.
        let mut tools = nothing();
        switch_on(&mut tools, "documents");
        assert!(tools.workspace, "documents without somewhere to put them");
    }

    #[test]
    fn the_catalogue_offers_only_what_is_off_and_could_work() {
        let mut tools = nothing();
        tools.planner = true;

        let offered = offerable(&tools, |name| name != "mail");
        let names: Vec<&str> = offered.iter().map(|c| c.name).collect();
        assert!(!names.contains(&"planner"), "already on: {names:?}");
        assert!(!names.contains(&"mail"), "cannot work: {names:?}");
        assert!(names.contains(&"documents"), "{names:?}");
    }

    #[test]
    fn a_conversation_with_every_switch_off_is_left_as_one() {
        // Somebody who turned everything off meant it, and a menu offering to
        // turn eight things back on is an argument with them.
        let bare = ToolSet {
            memory: false,
            web: false,
            weather: false,
            ..nothing()
        };
        assert!(offerable(&bare, |_| true).is_empty());
        // One switch on is an ordinary context, and gets the menu.
        assert!(!offerable(&nothing(), |_| true).is_empty());
    }

    #[test]
    fn a_context_with_everything_on_carries_no_catalogue_at_all() {
        // The note is pure cost to a context that has nothing left to offer,
        // and the whole argument for this design is that the cost is small.
        let mut tools = nothing();
        for capability in ALL {
            switch_on(&mut tools, capability.name);
        }
        assert_eq!(catalogue(&offerable(&tools, |_| true)), None);
    }

    #[test]
    fn the_catalogue_stays_short_enough_to_be_worth_carrying() {
        // The measured trade this whole module rests on: two capabilities'
        // worth of *guidance* cost the planner family two points. The catalogue
        // has to stay a menu rather than become the thing it replaced.
        //
        // Budgeted per part rather than in total, because a flat ceiling is a
        // number that has to be raised every time a capability is added — and a
        // guard you raise to make it pass is not a guard. What must not grow is
        // the *framing*, which is paid for once, and any single entry, which is
        // where a paragraph would creep in.
        let note = catalogue(&offerable(&nothing(), |_| true)).expect("a catalogue");
        assert!(note.contains("use_tools"), "{note}");

        let framing: usize = note
            .lines()
            .filter(|line| !line.starts_with("- `"))
            .map(str::len)
            .sum();
        assert!(
            framing < 700,
            "the catalogue's framing has grown to {framing} characters"
        );
        for capability in ALL {
            let entry = capability.summary.len() + capability.name.len();
            assert!(
                entry < 170,
                "the {} entry is {entry} characters — that is a paragraph, not a menu line",
                capability.name
            );
        }
    }

    #[test]
    fn the_catalogue_says_the_capabilities_are_not_available_yet() {
        // Otherwise the model reads the list as a tool list and calls `mail`
        // directly, which is an undeclared tool and a wasted round.
        let note = catalogue(&offerable(&nothing(), |_| true)).expect("a catalogue");
        assert!(note.contains("cannot use them yet"), "{note}");
    }

    #[test]
    fn switching_on_reports_what_changed_and_what_did_not() {
        let said = switched(&["mail".into()], &["planner".into()]);
        assert!(said.contains("Switched on: mail"), "{said}");
        assert!(said.contains("Already on: planner"), "{said}");
        // The half that matters: a model told only "done" stops.
        assert!(
            said.contains("carry on with what the user asked for"),
            "{said}"
        );
    }
}
