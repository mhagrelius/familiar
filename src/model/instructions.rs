//! Composing the system prompt, in the order that keeps the cache warm.
//!
//! llama-server reuses the KV cache for the longest stable prefix of a prompt.
//! The date is the only daily-volatile part, so it goes **last** — and it goes
//! last by construction rather than by convention, which is the whole reason
//! this is a type with a `volatile` field instead of a `format!` at the call
//! site. The test is a property: change the volatile part and everything before
//! it is byte-identical.
//!
//! The ambient memory block is *semi*-volatile. It is recomputed at thread
//! boundaries only, never mid-turn, so a fact the model writes during a turn
//! shows up in Background at the next thread switch and is findable with
//! `recall` until then.
//!
//! The user's own instructions **add to** [`DEFAULT_PERSONA`] rather than
//! replace it. Replacing it was the old behaviour and it made the smallest ask
//! — "call me Matt" — cost the paragraph that tells the model its answers are
//! rendered as Markdown, which nobody would knowingly trade away. They go
//! directly under the persona so that everything downstream, capabilities
//! included, is read in their light.

use chrono::{DateTime, TimeZone};

/// Turns the model's thinking off for one request.
///
/// A control token the chat template reads; a model whose template does not
/// define it sees an odd word and is otherwise unaffected. Every one-shot call
/// in this application uses it — extraction, consolidation, summarising and the
/// lookout are all judgements against an explicit rubric, and deliberating
/// about one costs more than the answer. It also makes a small `max_tokens`
/// safe: without it the reasoning eats the budget and the reply comes back
/// empty, which is a failure that looks exactly like a dead server.
///
/// Defined here because it was defined in three modules independently, and a
/// magic token copied four times is one that will be changed in three places.
pub const THINK_OFF: &str = "<|think_off|>";

pub const DEFAULT_PERSONA: &str = "\
You are Familiar, a personal assistant running on the user's own machine.

Be direct and concrete, and match the depth of the question. A question with \
one answer gets one answer; a question about a design, a trade-off or \
something you are unsure of deserves the reasoning that goes with it. Think it \
through properly before answering — the user can see how long you thought and \
would rather you took the time. Say when you do not know something rather than \
guessing, and say when a tool would settle it.

Your answer is rendered as Markdown in a desktop window: headings, lists, \
tables and code blocks display properly. Use them when structure helps, and \
prose when it does not.";

/// The parts of the prompt, in the only order they are allowed to appear.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prompt<'a> {
    /// Who the assistant is. [`DEFAULT_PERSONA`] unless something is being
    /// measured against a different one.
    pub persona: &'a str,
    /// What the user asked for on top, from the project they are in. Added to
    /// the persona, never in place of it.
    ///
    /// The project's *name* is deliberately not here and must not be: the model
    /// already has two meanings for that word — Planner's projects and the
    /// memory tool's `project` kind — and teaching it a third would show up as
    /// a worse score in two eval families rather than as a bug anyone could
    /// see.
    pub instructions: Option<&'a str>,
    /// What it can do, one note per enabled capability. Owned by the module
    /// that owns the tool, so adding a tool does not touch this file.
    pub capabilities: &'a [String],
    /// The memory block, already framed as untrusted data by `model::memory`.
    pub ambient: Option<&'a str>,
    /// The one part that changes from day to day.
    pub volatile: &'a str,
}

impl Prompt<'_> {
    pub fn compose(&self) -> String {
        let mut sections: Vec<String> = Vec::new();

        let persona = self.persona.trim();
        if !persona.is_empty() {
            sections.push(persona.to_string());
        }

        if let Some(asked) = self.instructions.map(str::trim).filter(|a| !a.is_empty()) {
            // Headed, because it is the user talking and the model should be
            // able to tell that from the paragraph above it — which is the
            // application talking.
            sections.push(format!("## What the user asked you to do\n\n{asked}"));
        }

        if !self.capabilities.is_empty() {
            let mut block = String::from("## What you can do\n");
            for capability in self.capabilities {
                block.push('\n');
                block.push_str(capability.trim());
                block.push('\n');
            }
            sections.push(block.trim_end().to_string());
        }

        if let Some(ambient) = self.ambient.map(str::trim).filter(|a| !a.is_empty()) {
            sections.push(ambient.to_string());
        }

        let volatile = self.volatile.trim();
        if !volatile.is_empty() {
            sections.push(volatile.to_string());
        }

        sections.join("\n\n")
    }

    /// Everything above the volatile line: the part llama-server may keep in
    /// its cache between turns.
    pub fn stable_prefix(&self) -> String {
        Prompt {
            volatile: "",
            ..self.clone()
        }
        .compose()
    }
}

/// The volatile line. Takes the caller's zone — the app passes `Local::now()`,
/// because "today" means the user's today — and the test passes a fixed one.
pub fn date_line<Tz: TimeZone>(now: DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    format!("Today is {}.", now.format("%A %-d %B %Y"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities() -> Vec<String> {
        vec![
            "You can search the user's notes with `recall`.".to_string(),
            "You can search the web with `web_search`.".to_string(),
        ]
    }

    #[test]
    fn the_volatile_line_is_last_whatever_else_is_present() {
        let capabilities = capabilities();
        let variants = [
            Prompt {
                persona: DEFAULT_PERSONA,
                instructions: Some("Call me Matt."),
                capabilities: &capabilities,
                ambient: Some("<saved_memory>…</saved_memory>"),
                volatile: "Today is Friday 31 July 2026.",
            },
            Prompt {
                persona: DEFAULT_PERSONA,
                instructions: None,
                capabilities: &[],
                ambient: None,
                volatile: "Today is Friday 31 July 2026.",
            },
            Prompt {
                persona: "",
                instructions: None,
                capabilities: &[],
                ambient: None,
                volatile: "Today is Friday 31 July 2026.",
            },
        ];
        for prompt in variants {
            let composed = prompt.compose();
            assert!(
                composed.ends_with("Today is Friday 31 July 2026."),
                "{composed}"
            );
        }
    }

    #[test]
    fn changing_the_date_leaves_the_cached_prefix_untouched() {
        // The property the KV cache depends on. If this fails, every turn on a
        // new day pays for a full prefill.
        let capabilities = capabilities();
        let prompt = Prompt {
            persona: DEFAULT_PERSONA,
            instructions: Some("Call me Matt."),
            capabilities: &capabilities,
            ambient: Some("<saved_memory>Matthew writes Rust.</saved_memory>"),
            volatile: "Today is Friday 31 July 2026.",
        };
        let tomorrow = Prompt {
            volatile: "Today is Saturday 1 August 2026.",
            ..prompt.clone()
        };

        let today = prompt.compose();
        let later = tomorrow.compose();
        let shared = prompt.stable_prefix();

        assert_eq!(shared, tomorrow.stable_prefix());
        assert!(today.starts_with(&shared), "{today}");
        assert!(later.starts_with(&shared), "{later}");
        assert_ne!(today, later);
    }

    #[test]
    fn ambient_memory_sits_above_the_date_and_below_the_capabilities() {
        let capabilities = capabilities();
        let composed = Prompt {
            persona: "You are Familiar.",
            instructions: None,
            capabilities: &capabilities,
            ambient: Some("<saved_memory>Matthew writes Rust.</saved_memory>"),
            volatile: "Today is Friday 31 July 2026.",
        }
        .compose();

        let capability = composed.find("`recall`").expect("capabilities");
        let ambient = composed.find("saved_memory").expect("ambient");
        let date = composed.find("Today is").expect("date");
        assert!(capability < ambient, "{composed}");
        assert!(ambient < date, "{composed}");
    }

    /// The whole point of the change: what the user asked for is *added* to the
    /// built-in instructions, so asking to be called Matt does not cost the
    /// paragraph about Markdown.
    #[test]
    fn what_the_user_asked_for_is_added_to_the_persona_not_swapped_for_it() {
        let composed = Prompt {
            persona: DEFAULT_PERSONA,
            instructions: Some("Call me Matt, and keep answers short."),
            capabilities: &[],
            ambient: None,
            volatile: "Today is Friday 31 July 2026.",
        }
        .compose();

        assert!(composed.contains("rendered as Markdown"), "{composed}");
        let persona = composed.find("You are Familiar").expect("the persona");
        let asked = composed.find("Call me Matt").expect("the instructions");
        let date = composed.find("Today is").expect("the date");
        assert!(persona < asked, "{composed}");
        assert!(asked < date, "{composed}");
    }

    /// The project is where the instructions came from and the model is never
    /// told that. See the header of `model::project`.
    #[test]
    fn nothing_names_the_project_the_instructions_came_from() {
        let composed = Prompt {
            persona: DEFAULT_PERSONA,
            instructions: Some("Prefer Rust."),
            capabilities: &[],
            ambient: None,
            volatile: "Today is Friday 31 July 2026.",
        }
        .compose();
        assert!(!composed.to_lowercase().contains("project"), "{composed}");
    }

    #[test]
    fn an_absent_part_leaves_no_blank_hole() {
        let composed = Prompt {
            persona: "You are Familiar.",
            instructions: None,
            capabilities: &[],
            ambient: None,
            volatile: "Today is Friday 31 July 2026.",
        }
        .compose();
        assert_eq!(
            composed,
            "You are Familiar.\n\nToday is Friday 31 July 2026."
        );
        assert!(!composed.contains("\n\n\n"), "{composed}");
    }

    #[test]
    fn each_capability_is_its_own_line() {
        let capabilities = capabilities();
        let composed = Prompt {
            persona: "",
            instructions: None,
            capabilities: &capabilities,
            ambient: None,
            volatile: "",
        }
        .compose();
        assert_eq!(
            composed,
            "## What you can do\n\nYou can search the user's notes with `recall`.\n\nYou can search the web with `web_search`."
        );
    }

    #[test]
    fn the_date_line_reads_as_a_person_would_say_it() {
        let when: DateTime<chrono::FixedOffset> =
            "2026-07-31T14:02:11+01:00".parse().expect("date");
        assert_eq!(date_line(when), "Today is Friday 31 July 2026.");
    }
}
