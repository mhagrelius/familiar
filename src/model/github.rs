//! Reaching GitHub through the `gh` CLI.
//!
//! `gh` is already installed and already authenticated — it holds a token in
//! the system keyring — so the useful thing is not a REST client but permission
//! to run the one binary. That is a new shape for this app, which until now
//! could not run a program at all, and the whole of this module is the argument
//! for why it is a *narrow* one.
//!
//! **Only `gh`, and only as an argv.** The tool takes a list of arguments and
//! they are handed to `execvp` as they are. There is no shell, so `;`, `|`,
//! `$(…)`, `&&` and redirections are not operators — they are literal strings
//! that `gh` will reject. A model that writes `gh pr list; rm -rf ~` gets an
//! error from `gh` about an unknown argument, which is the point of not having
//! a `run_command` tool instead.
//!
//! **The subcommand decides the gate.** `gh pr list` reads and is ungated, the
//! same as `read_file`. `gh pr merge` acts as the user on a shared repository
//! and stops at the approval dialog with its exact argv on screen. An unknown
//! subcommand is gated, for the same reason an unknown tool is.
//!
//! **A few things are refused outright rather than gated.** Approval is only
//! meaningful when a person can judge what they are approving, and
//! `gh extension install` or `gh alias set` are arbitrary code arriving under a
//! plausible name. `gh auth token` is refused because its whole output is a
//! credential, and everything a tool returns goes into the model's context and
//! the transcript on disk.

use super::tools::Gate;

/// What may be done with a `gh` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Run it, behind this gate.
    Run(Gate),
    /// Do not run it at all, and say why.
    Refuse(String),
}

/// Subcommands that only read. Ungated, like `read_file` and `list_dir`.
///
/// A pair is `noun verb`; a single entry matches on the noun alone. The list is
/// deliberately short — anything not on it is gated, so forgetting to add
/// something costs an approval click rather than an unreviewed write.
const READS: &[(&str, &str)] = &[
    ("pr", "list"),
    ("pr", "view"),
    ("pr", "status"),
    ("pr", "checks"),
    ("pr", "diff"),
    ("issue", "list"),
    ("issue", "view"),
    ("issue", "status"),
    ("repo", "view"),
    ("repo", "list"),
    ("run", "list"),
    ("run", "view"),
    ("release", "list"),
    ("release", "view"),
    ("workflow", "list"),
    ("workflow", "view"),
    ("gist", "list"),
    ("gist", "view"),
    ("label", "list"),
    ("cache", "list"),
    ("project", "list"),
    ("project", "view"),
    ("ruleset", "list"),
    ("ruleset", "view"),
    ("org", "list"),
    ("search", ""),
    ("status", ""),
    ("browse", ""),
    ("version", ""),
];

/// Every top-level `gh` command, so the subcommand can be found by name.
///
/// Being out of date here is harmless in the safe direction: a command not on
/// the list falls through to "unrecognised", and unrecognised is gated.
const NOUNS: &[&str] = &[
    "alias",
    "api",
    "attestation",
    "auth",
    "browse",
    "cache",
    "codespace",
    "completion",
    "config",
    "extension",
    "gist",
    "gpg-key",
    "issue",
    "label",
    "org",
    "pr",
    "project",
    "release",
    "repo",
    "ruleset",
    "run",
    "search",
    "secret",
    "ssh-key",
    "status",
    "variable",
    "version",
    "workflow",
];

/// Refused outright, with the reason the model is told.
///
/// Two kinds: things whose output *is* a credential, and things that install or
/// redefine what a later command means. Neither is something a person can
/// meaningfully approve from a dialog showing an argv.
fn refusal(noun: &str, verb: &str) -> Option<String> {
    match (noun, verb) {
        ("auth", "token") => Some(
            "`gh auth token` prints the user's credential, and everything a tool returns is \
             kept in this conversation and written to disk. Ask for what you actually need \
             from GitHub instead."
                .into(),
        ),
        ("auth", "login" | "logout" | "refresh" | "setup-git" | "switch") => Some(
            "changing how `gh` is signed in is the user's to do, not yours. Tell them what \
             you need and let them run it."
                .into(),
        ),
        ("extension", _) => Some(
            "`gh extension` installs and runs code from a repository, which is not something \
             the user can judge from an approval dialog. It is not available."
                .into(),
        ),
        ("alias", "set" | "delete" | "import") => Some(
            "`gh alias` changes what a later command means, so approving one call would be \
             approving every call after it. It is not available."
                .into(),
        ),
        ("config", "set") => Some(
            "`gh config set` changes how every later command behaves. Tell the user what \
             setting you need instead."
                .into(),
        ),
        ("codespace", _) => Some(
            "`gh codespace` opens a shell on a remote machine, which is a way around every \
             limit here. It is not available."
                .into(),
        ),
        _ => None,
    }
}

/// Decide what to do with the arguments the model wrote.
pub fn classify(args: &[String]) -> Decision {
    let trimmed: Vec<&str> = args
        .iter()
        .map(|argument| argument.trim())
        .filter(|argument| !argument.is_empty())
        .collect();
    if trimmed.is_empty() {
        return Decision::Refuse("no arguments were given — try [\"pr\", \"list\"]".into());
    }

    // A token that reveals a credential is refused wherever it appears, not
    // only after `auth`: `gh auth status --show-token` prints it too.
    if trimmed.contains(&"--show-token") {
        return Decision::Refuse(
            "`--show-token` prints the user's credential into this conversation. Ask for what \
             you need from GitHub instead."
                .into(),
        );
    }

    // Find the subcommand by name rather than by position. "The first thing
    // that is not a flag" is wrong the moment a flag takes a value: in
    // `gh --repo owner/name pr list` that rule picks `owner/name`, and the
    // command reads as an unknown noun.
    let at = trimmed.iter().position(|word| NOUNS.contains(word));
    let (noun, verb) = match at {
        Some(at) => (
            trimmed[at],
            // The word immediately after, and only that. Which flags take a
            // value is not knowable from here, so anything else is left
            // unrecognised — and unrecognised is gated, which is the safe
            // direction to be wrong in.
            trimmed.get(at + 1).copied().unwrap_or_default(),
        ),
        None => (trimmed[0], trimmed.get(1).copied().unwrap_or_default()),
    };

    if let Some(why) = refusal(noun, verb) {
        return Decision::Refuse(why);
    }

    // `gh api` is every endpoint at once, so it is classified by HTTP method
    // rather than by name. Without one it is a GET.
    if noun == "api" {
        let method = trimmed
            .iter()
            .position(|argument| *argument == "-X" || *argument == "--method")
            .and_then(|at| trimmed.get(at + 1))
            .map(|method| method.to_ascii_uppercase())
            .unwrap_or_else(|| "GET".to_string());
        // `--input`, `-f` and `--raw-field` send a body, which makes it a write
        // whatever the method says.
        let sends_a_body = trimmed.iter().any(|argument| {
            matches!(
                *argument,
                "-f" | "--field" | "-F" | "--raw-field" | "--input"
            )
        });
        return if method == "GET" && !sends_a_body {
            Decision::Run(Gate::Never)
        } else {
            Decision::Run(Gate::Always)
        };
    }

    let reads = READS.iter().any(|(read_noun, read_verb)| {
        *read_noun == noun && (*read_verb == verb || read_verb.is_empty())
    });
    Decision::Run(if reads { Gate::Never } else { Gate::Always })
}

/// The command to spawn. Always `gh` first: the model supplies arguments, never
/// the program.
pub fn command(args: &[String]) -> Vec<String> {
    let mut command = vec!["gh".to_string()];
    command.extend(args.iter().map(|argument| argument.trim().to_string()));
    command
}

/// How much of `gh`'s output comes back.
///
/// `gh pr list --limit 200` or an `api` call over a busy repository can run to
/// hundreds of kilobytes of JSON, and this stays in front of the model for the
/// rest of the turn.
pub const MAX_OUTPUT: usize = 20_000;

/// The guidance that rides in the prompt when this is switched on.
pub fn guidance() -> String {
    "You can use the GitHub CLI with the `gh` tool: pass the arguments as a list, so \
     `[\"pr\", \"list\", \"--state\", \"open\"]`. It is already signed in as the user, so \
     never ask them for a token and never try to read one.\n\n\
     Prefer `gh` over guessing, and prefer it over `fetch_url` for anything on GitHub — it \
     is authenticated, so it can see private repositories and gives structured output. Use \
     `--json` with the fields you want (`gh pr list --json number,title,author`) rather than \
     parsing the human-readable form, and `--limit` to keep results small.\n\n\
     Reading — `pr list`, `pr view`, `issue view`, `run list`, `api` with no method — runs \
     without asking. Anything that writes, merges, closes, deletes or dispatches stops for \
     the user's approval with the exact command shown, so say what you intend to run and \
     why before you run it. Run it in the context's workspace, which is the repository you \
     are working in; `--repo owner/name` reaches a different one."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    fn decide(line: &str) -> Decision {
        classify(&args(line))
    }

    fn gate(line: &str) -> Gate {
        match decide(line) {
            Decision::Run(gate) => gate,
            Decision::Refuse(why) => panic!("{line:?} was refused: {why}"),
        }
    }

    fn refused(line: &str) -> String {
        match decide(line) {
            Decision::Refuse(why) => why,
            Decision::Run(gate) => panic!("{line:?} was allowed at {gate:?}"),
        }
    }

    #[test]
    fn reading_does_not_stop_to_ask() {
        for line in [
            "pr list",
            "pr view 42",
            "pr diff 42",
            "issue list --state open",
            "repo view",
            "run list --limit 5",
            "release view v1.0.0",
            "search issues --owner mhagrelius",
            "status",
        ] {
            assert_eq!(gate(line), Gate::Never, "{line}");
        }
    }

    #[test]
    fn anything_that_changes_a_repository_stops_at_the_gate() {
        for line in [
            "pr create --title x --body y",
            "pr merge 42",
            "pr close 42",
            "issue create --title x",
            "release create v1.0.0",
            "repo delete owner/name",
            "workflow run deploy.yml",
            "run cancel 12345",
            "secret set NAME",
            "label create bug",
        ] {
            assert_eq!(gate(line), Gate::Always, "{line}");
        }
    }

    #[test]
    fn an_unknown_subcommand_is_gated() {
        // The same rule as an unknown tool: something nobody classified must
        // not run unreviewed.
        assert_eq!(gate("nonesuch do-a-thing"), Gate::Always);
        assert_eq!(gate("pr somethingnew 42"), Gate::Always);
    }

    #[test]
    fn a_flag_before_the_subcommand_does_not_hide_it() {
        // `gh --repo x pr merge 42` is still a merge.
        assert_eq!(gate("--repo owner/name pr merge 42"), Gate::Always);
        assert_eq!(gate("--repo owner/name pr list"), Gate::Never);
    }

    #[test]
    fn a_flag_between_the_noun_and_the_verb_does_not_hide_it() {
        assert_eq!(gate("pr --repo owner/name merge 42"), Gate::Always);
    }

    #[test]
    fn the_api_subcommand_is_classified_by_method_not_by_name() {
        // `gh api` is every endpoint at once, so the name says nothing.
        assert_eq!(gate("api repos/owner/name/pulls"), Gate::Never);
        assert_eq!(gate("api -X GET repos/owner/name"), Gate::Never);
        assert_eq!(gate("api -X POST repos/owner/name/issues"), Gate::Always);
        assert_eq!(gate("api --method DELETE repos/owner/name"), Gate::Always);
        assert_eq!(gate("api --method patch repos/owner/name"), Gate::Always);
    }

    #[test]
    fn an_api_call_that_sends_a_body_is_a_write_whatever_the_method_says() {
        // `gh api -f name=value` silently turns a GET into a POST.
        assert_eq!(gate("api repos/o/n/issues -f title=x"), Gate::Always);
        assert_eq!(gate("api repos/o/n --input body.json"), Gate::Always);
        assert_eq!(gate("api repos/o/n --raw-field q=x"), Gate::Always);
    }

    #[test]
    fn reading_the_credential_is_refused_not_gated() {
        // Everything a tool returns goes into the model's context and onto
        // disk, so this one must never be approvable by a tired click.
        assert!(refused("auth token").contains("credential"));
        assert!(refused("auth status --show-token").contains("credential"));
    }

    #[test]
    fn installing_or_redefining_commands_is_refused() {
        // Approval means nothing when what is approved is "run some code from
        // the internet" or "change what every later command means".
        assert!(refused("extension install owner/thing").contains("not available"));
        assert!(refused("alias set co pr checkout").contains("not available"));
        assert!(refused("codespace ssh").contains("not available"));
        assert!(!refusal("config", "set").unwrap_or_default().is_empty());
    }

    #[test]
    fn signing_in_and_out_is_the_users_to_do() {
        assert!(refused("auth login").contains("user's to do"));
        assert!(refused("auth logout").contains("user's to do"));
    }

    #[test]
    fn checking_who_is_signed_in_is_still_allowed() {
        // Only the token itself is the problem; knowing the account is useful.
        assert_eq!(gate("auth status"), Gate::Always);
    }

    #[test]
    fn no_arguments_says_what_to_try() {
        assert!(refused("").contains("pr"));
        assert!(matches!(classify(&[]), Decision::Refuse(_)));
        assert!(matches!(
            classify(&["   ".to_string()]),
            Decision::Refuse(_)
        ));
    }

    #[test]
    fn the_program_is_always_gh_and_never_the_models_choice() {
        // The model supplies arguments; it does not get to name the binary.
        assert_eq!(command(&args("pr list")), ["gh", "pr", "list"]);
        assert_eq!(command(&[]), ["gh"]);
    }

    #[test]
    fn shell_metacharacters_are_arguments_and_not_operators() {
        // There is no shell, so these reach `gh` as literal strings. This test
        // exists to record that the safety comes from argv, not from filtering.
        let built = command(&args("pr list ; rm -rf ~"));
        assert_eq!(built, ["gh", "pr", "list", ";", "rm", "-rf", "~"]);
        assert_eq!(gate("pr list ; rm -rf ~"), Gate::Never);
    }

    #[test]
    fn a_flag_taking_a_value_does_not_become_the_subcommand() {
        // The bug this replaced: skipping flags but not their values made
        // `gh --repo owner/name pr list` read as the command `owner/name`,
        // which fell through to gated — safe, but it asked for approval on
        // every read.
        assert_eq!(gate("--repo owner/name pr list"), Gate::Never);
        assert_eq!(gate("-R owner/name issue list"), Gate::Never);
    }

    #[test]
    fn a_repository_named_like_a_command_does_not_confuse_it() {
        assert_eq!(gate("--repo owner/pr issue list"), Gate::Never);
        assert_eq!(gate("--repo owner/pr issue create --title x"), Gate::Always);
    }

    #[test]
    fn the_guidance_says_it_is_already_signed_in() {
        // A model that asks the user for a token has misread the situation, and
        // this is the sentence that prevents it.
        let guidance = guidance();
        assert!(guidance.contains("already signed in"), "{guidance}");
        assert!(guidance.contains("--json"), "{guidance}");
    }
}
