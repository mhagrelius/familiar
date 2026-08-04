//! Asking a stronger model, when the local one has genuinely run out of road.
//!
//! Familiar runs a 27B model on one machine, which is enough for nearly
//! everything it is asked and is not enough for some things. `claude -p` and
//! `codex exec` are already installed and already signed in, so the escape
//! hatch costs one subprocess.
//!
//! **The whole design problem here is restraint, not plumbing.** A tool
//! described as "ask a better model" is one a small model will reach for the
//! moment a question looks hard, and every one of those calls sends the user's
//! words to a company's servers, spends their subscription, and takes half a
//! minute. So three things hold it back, and none of them is politeness:
//!
//! * It is [`Gate::Always`], and the approval dialog shows the exact text that
//!   would leave the machine. That is the leakage control — not a promise in a
//!   paragraph, but the user reading the question before it is sent.
//! * The guidance says what it is *for* in the narrowest terms that are still
//!   true, and the eval scores the model **not** reaching for it far more often
//!   than reaching for it.
//! * The question has to be self-contained, which is a real cost to pay and
//!   makes an idle escalation feel like the work it is.
//!
//! **It consults; it does not act.** Both CLIs are agents that can edit files
//! and run commands, and neither is invoked in a way that lets them: `claude`
//! runs in plan mode with its editing tools denied, `codex` in its read-only
//! sandbox, and both in an empty scratch directory rather than the user's
//! workspace. What comes back is text. A stronger model quietly rewriting
//! files while the local one was only asking a question is not a capability
//! anybody asked for, and the gate the user approved said "send this question",
//! not "let something else loose in my files".

use std::path::{Path, PathBuf};

/// Which CLI answers. Both are agents; both are invoked here as oracles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    #[default]
    Claude,
    Codex,
}

impl Backend {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// What the user is told to install when it is not there.
    pub fn install_hint(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code (`claude`) is not installed, or not on the PATH",
            Self::Codex => "Codex (`codex`) is not installed, or not on the PATH",
        }
    }
}

/// How much of a question may be sent.
///
/// A ceiling on what can leave the machine in one call, and on what the user
/// has to read at the gate before approving it. A question longer than this is
/// nearly always a transcript being forwarded wholesale, which is the thing the
/// gate exists to prevent someone approving by accident.
pub const MAX_QUESTION: usize = 6_000;

/// How much of an answer comes back.
pub const MAX_OUTPUT: usize = 12_000;

/// Why a call was refused before anything was spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Empty,
    TooLong(usize),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str(
                "there is no question to ask — put the whole question in `question`, written so \
                 it can be answered by someone who cannot see this conversation",
            ),
            Self::TooLong(length) => write!(
                f,
                "that question is {length} characters and the limit is {MAX_QUESTION}. Ask the \
                 question, not the whole conversation: everything in it leaves this machine, \
                 and the user has to read it before it goes."
            ),
        }
    }
}

/// The invocation, as an argv. **The question is not in it** — see [`prompt`].
///
/// Two reasons, and the second was found by running the thing. Both CLIs take
/// variadic options: `--disallowed-tools <tools...>` and `--add-dir
/// <directories...>` swallow every following word, so a question passed as a
/// trailing argument silently became another tool name and the call died with
/// "Input must be provided either through stdin or as a prompt argument". And
/// an argument is world-readable — anyone on the machine can watch `ps` and
/// read the user's private question. Standard input fixes both.
///
/// The working directory is set by the caller to an empty scratch directory
/// rather than the workspace. Both CLIs orient themselves by where they are
/// standing — reading `CLAUDE.md`, looking for a git repository, offering the
/// files around them to the model — and a consultation should carry what the
/// question says and nothing it happened to be next to.
pub fn command(backend: Backend, model: Option<&str>) -> Vec<String> {
    let mut argv: Vec<String> = match backend {
        Backend::Claude => [
            "claude",
            "--print",
            // Plan mode cannot edit. The denials are belt and braces, and they
            // are the tools that would otherwise let a consultation turn into
            // an agent loose on the machine. Comma-separated as one argument:
            // this option is variadic and eats whitespace-separated words.
            "--permission-mode",
            "plan",
            "--disallowed-tools",
            "Bash,Edit,Write,NotebookEdit,Task",
        ]
        .iter()
        .map(|word| word.to_string())
        .collect(),
        Backend::Codex => [
            "codex",
            "exec",
            "--sandbox",
            "read-only",
            "--skip-git-repo-check",
        ]
        .iter()
        .map(|word| word.to_string())
        .collect(),
    };

    if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
        argv.push("--model".into());
        argv.push(model.to_string());
    }
    argv
}

/// What goes down standard input: the question, with the one instruction the
/// answer needs.
pub fn prompt(question: &str) -> String {
    framed(question)
}

/// Where a consultation runs: a directory with nothing in it.
pub fn scratch(root: &Path) -> PathBuf {
    root.join("escalations")
}

/// The question, with the one instruction the answer needs.
///
/// Without it both CLIs answer as coding agents talking to a developer —
/// offering to make the change, asking which file to start in. What is wanted
/// is prose that another model can read and use.
fn framed(question: &str) -> String {
    format!(
        "Answer the following question directly and completely, as text. Do not offer to make \
         changes, do not ask a follow-up question, and do not describe what you would do — the \
         person reading your answer cannot reply to you. If the question cannot be answered as \
         asked, say what is missing.\n\n{}",
        question.trim()
    )
}

/// What the user is told before a wait that runs to half a minute or more.
pub fn waiting_for(backend: Backend) -> String {
    format!(
        "Asking {} — this takes a little while, and what you approved is on its way.",
        backend.label()
    )
}

/// What the system prompt says about it.
///
/// Written to be *discouraging*, and the eval holds it to that: most of the
/// escalation family scores the model answering the question itself. The
/// failure this is guarding against is not a missed escalation, which costs a
/// slightly worse answer; it is an assistant that hands the user's private
/// questions to a cloud service every time something looks hard.
pub fn guidance(backend: Backend) -> String {
    format!(
        "`escalate` sends one question to {}, a much larger model, and returns its answer.\n\n\
         **This is a last resort and almost never the right move.** You are a capable model \
         with tools; answer the question yourself. Do not decide on your own that a question \
         is too hard — the ones you are worst at are the ones you are most sure about. Not \
         because a question is long, not because a subject is unfamiliar, and never before \
         you have attempted it.\n\n\
         **When the user asks for it, do it — even if you think you know the answer.** That is \
         not a judgement call and it is not a suggestion you can talk them out of. \"Ask a \
         stronger model\", \"get a second opinion\", \"run this past {}\", \"check this with \
         something bigger\" are all the same request. Answering it yourself with \"I can settle \
         this here, no escalation needed\" ignores what they asked for; they know what you can \
         do and they asked anyway. Send the question, and then say what you think as well if \
         you disagree with what comes back. The other time to reach for this is when you have \
         genuinely tried and failed at something with a right answer — a proof, a subtle bug, \
         reasoning you can tell you have got wrong twice.\n\n\
         **Everything you put in the question leaves this machine.** It goes to a company's \
         servers and it costs the user money. Write the smallest self-contained question that \
         can be answered — the answerer cannot see this conversation, their notes, or their \
         files — and do not paste file contents, keys, or anything personal into it unless \
         the user has asked you to. The user approves the exact text before it is sent, so \
         write it as something you would be content for them to read.\n\n\
         What comes back is one model's opinion, not a fact. Say where the answer came from, \
         and check it against anything you can actually verify.",
        backend.label(),
        backend.label()
    )
}

/// The rules that ride with a reply rather than sitting in the prompt.
pub fn note_for(answer: &str) -> Option<String> {
    if answer.trim().is_empty() {
        return None;
    }
    Some(
        "That is the stronger model's answer. Two things before you pass it on: say plainly \
         that you asked it rather than presenting it as your own, and check anything in it \
         you have a tool for — it could not see the user's files, notes or conversation, so \
         a claim about any of those is a guess. Do not escalate again this turn."
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neither_backend_is_invoked_in_a_way_that_can_change_anything() {
        // The claim the gate rests on: the user approved sending a question,
        // not turning a coding agent loose. If these flags go missing the tool
        // is a different tool.
        let claude = command(Backend::Claude, None).join(" ");
        assert!(claude.contains("--print"), "{claude}");
        assert!(claude.contains("--permission-mode plan"), "{claude}");
        for denied in ["Bash", "Edit", "Write", "Task"] {
            assert!(claude.contains(denied), "{denied} is not denied: {claude}");
        }

        let codex = command(Backend::Codex, None).join(" ");
        assert!(codex.contains("exec"), "{codex}");
        assert!(codex.contains("--sandbox read-only"), "{codex}");
    }

    #[test]
    fn the_denied_tools_are_one_comma_separated_argument() {
        // Measured against the real CLI. `--disallowed-tools` is variadic, so
        // listing the tools as separate words swallowed everything after them
        // — including the question — and the call died with "Input must be
        // provided either through stdin or as a prompt argument", which says
        // nothing whatever about the actual cause.
        let argv = command(Backend::Claude, None);
        let at = argv
            .iter()
            .position(|word| word == "--disallowed-tools")
            .expect("the denials");
        assert_eq!(argv[at + 1], "Bash,Edit,Write,NotebookEdit,Task");
    }

    #[test]
    fn the_question_never_appears_on_the_command_line() {
        // Two reasons. The variadic options above eat it, and an argument is
        // world-readable — anyone on this machine can watch `ps` and read a
        // question the user approved in confidence.
        let argv = command(Backend::Claude, None);
        assert!(!argv.iter().any(|word| word.contains("rm -rf")));
        assert!(!argv.iter().any(|word| word == "-c" || word == "sh"));

        let asked = prompt("what does `rm -rf /` do; and why?");
        assert!(asked.contains("rm -rf /"), "{asked}");
        assert!(asked.contains("cannot reply"), "{asked}");
    }

    #[test]
    fn a_consultation_has_somewhere_empty_to_run() {
        // Both CLIs read the directory they are standing in — CLAUDE.md, the
        // git repository, the files around them.
        assert!(scratch(Path::new("/data")).ends_with("escalations"));
    }

    #[test]
    fn the_answer_is_asked_for_as_text_rather_than_as_an_offer_to_help() {
        // Both CLIs are coding agents by default and answer like one —
        // "I can make that change for you, which file?" — to a caller that
        // cannot reply.
        let prompt = framed("Is this proof sound?");
        assert!(prompt.contains("cannot reply"), "{prompt}");
        assert!(prompt.contains("Is this proof sound?"), "{prompt}");
    }

    #[test]
    fn a_named_model_is_passed_through_and_an_empty_one_is_not() {
        let named = command(Backend::Claude, Some("opus"));
        assert!(named.iter().any(|word| word == "opus"));
        let unnamed = command(Backend::Claude, Some("  "));
        assert!(!unnamed.iter().any(|word| word == "--model"));
    }

    #[test]
    fn the_guidance_discourages_far_more_than_it_encourages() {
        // The failure being guarded against is not a missed escalation. It is
        // an assistant that sends someone's private questions to a cloud
        // service whenever something looks hard.
        let note = guidance(Backend::Claude);
        assert!(note.contains("last resort"), "{note}");
        assert!(note.contains("answer the question yourself"), "{note}");
        assert!(note.contains("leaves this machine"), "{note}");
        assert!(note.contains("costs the user money"), "{note}");
        // And it names the one case the user always gets: asking for it. In
        // more than one wording, because a model that only recognises "ask a
        // stronger model" answers "get a second opinion on this" by explaining
        // that it does not need one — which is what two runs in three did.
        assert!(note.contains("When the user asks for it, do it"), "{note}");
        for phrasing in ["second opinion", "run this past", "something bigger"] {
            assert!(note.contains(phrasing), "{phrasing} is not named: {note}");
        }
    }

    #[test]
    fn an_answer_arrives_with_the_rule_that_it_is_not_a_fact() {
        let note = note_for("The proof holds because …").expect("a note");
        assert!(note.contains("say plainly that you asked it"), "{note}");
        assert!(note.contains("Do not escalate again this turn"), "{note}");
        assert_eq!(note_for("   "), None);
    }

    #[test]
    fn an_empty_or_enormous_question_is_refused_before_anything_is_spawned() {
        assert!(Refusal::Empty.to_string().contains("no question to ask"));
        let long = Refusal::TooLong(20_000).to_string();
        assert!(long.contains("20000"), "{long}");
        assert!(long.contains("leaves this machine"), "{long}");
    }

    #[test]
    fn a_backend_is_named_the_way_a_person_writes_it() {
        assert_eq!(Backend::parse("claude"), Some(Backend::Claude));
        assert_eq!(Backend::parse(" Codex "), Some(Backend::Codex));
        assert_eq!(Backend::parse("gpt"), None);
        assert_eq!(Backend::default(), Backend::Claude);
    }
}
