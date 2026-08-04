//! A Python interpreter the model may write scripts for, in a container that
//! can reach almost nothing.
//!
//! This exists because a language model doing arithmetic is guessing. It is
//! very good at guessing, which is worse than being bad at it: "the compound
//! total is £18,472.16" arrives with the same confidence whether it was
//! computed or remembered, and the user cannot tell which. A script is the
//! opposite kind of answer — it is *derivable*, the model can read its own
//! output, and getting it wrong produces a traceback rather than a plausible
//! number. So the rule this capability is built around is that anything
//! deterministic should be *run*, not recalled.
//!
//! **The container is disposable and the directory is not.** Each call is a
//! fresh `podman run --rm`; what persists between them is the bind-mounted
//! sandbox directory, which is where a script's files, its data and its
//! intermediate results live. That split was measured rather than assumed: a
//! cold start here is 170 ms, so a long-lived container would buy nothing but
//! the lifecycle bugs that come with one — a stale process to reap, state left
//! behind by a script that crashed, and a warm interpreter whose globals are
//! whatever the last three calls happened to leave in it. Files are the part
//! that actually needs to survive, and files survive.
//!
//! # What it can reach
//!
//! | | |
//! |---|---|
//! | `/work` | the sandbox's own directory, read-write, persists between calls |
//! | `/workspace` | the context's workspace, **read-only**, absent if it has none |
//! | the network | nothing at all — `--network=none` |
//! | the rest of the host | nothing |
//!
//! Those four lines are the whole security argument, and they are why
//! [`crate::model::tools::gate_of`] does not gate `run_python`. The gate exists
//! to stop the model changing something outside the vault, and this cannot: it
//! writes only to a directory this application owns, it reads only what
//! `read_file` would already hand over ungated, and with no network it cannot
//! send what it read anywhere. Running is not the thing that needs approval —
//! *escaping* is, and there is nowhere to escape to. Getting a result out of
//! the sandbox and into the user's files goes through `copy_to_workspace`,
//! which is gated like every other write.
//!
//! Rootless podman is what makes the bind mount work: the container's root maps
//! to the invoking user, so a file the script writes to `/work` is owned by the
//! person running the app rather than by a subuid nothing can read afterwards.
//! `--cap-drop=ALL` and `no-new-privileges` mean that root is toothless anyway.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The image the sandbox runs. Built by `packaging/build-sandbox.sh`, which is
/// named in the refusal so nobody has to guess.
pub const IMAGE: &str = "localhost/familiar-sandbox:1";

/// What to run to get the image, quoted back to the user verbatim.
pub const BUILD_COMMAND: &str = "packaging/build-sandbox.sh";

/// How long a script may run before the container is killed.
///
/// Generous for arithmetic and mean for an accident. The thing this is really
/// protecting against is `while True:`, which a model writes about as often as
/// anyone else does, and which without this pins a core until the app is
/// closed.
pub const TIMEOUT_SECONDS: u32 = 30;

/// How much memory the container gets.
pub const MEMORY: &str = "1g";

/// How many processes. A fork bomb is the other accident that costs a machine.
pub const PIDS: &str = "256";

/// How much of a script will be accepted.
///
/// A model that is writing 20,000 characters of Python in one call has
/// misunderstood the tool; the answer is to say so rather than to hand a wall
/// of generated code to an interpreter.
pub const MAX_CODE: usize = 20_000;

/// How much output comes back. Beyond this the model is told to print less,
/// which is nearly always the right fix — the usual cause is dumping a whole
/// dataframe when the question was one number.
pub const MAX_OUTPUT: usize = 8_000;

/// Where the script is written, relative to the sandbox root. Under a dot
/// directory so that a listing of what the model produced is not two thirds
/// our own plumbing.
pub const SCRATCH: &str = ".familiar";

/// Files the sandbox keeps for itself, never reported as the model's output.
const OURS: &[&str] = &[SCRATCH];

/// Why a script was not run at all. Neither of these reaches podman.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No code, or nothing but whitespace.
    Empty,
    /// Longer than [`MAX_CODE`].
    TooLong(usize),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str(
                "there is no code to run — pass the script as `code`, as you would write it \
                 in a file",
            ),
            Self::TooLong(length) => write!(
                f,
                "that script is {length} characters and the limit is {MAX_CODE}. Write the \
                 part that answers the question, not a program that covers every case."
            ),
        }
    }
}

/// The sandbox for one context: its own directory, and what it may read.
#[derive(Debug, Clone)]
pub struct Sandbox {
    root: PathBuf,
    /// The context's workspace, mounted read-only. `None` when the context has
    /// none, in which case `/workspace` simply is not there and the guidance
    /// says so.
    workspace: Option<PathBuf>,
}

impl Sandbox {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            workspace: None,
        }
    }

    /// Mount a workspace read-only. Read-only is the whole point: the sandbox
    /// can compute over the user's files and cannot touch them, so every change
    /// to the workspace still goes through a tool the user approves.
    pub fn reading(mut self, workspace: Option<PathBuf>) -> Self {
        self.workspace = workspace;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the script for this call goes, on the host.
    pub fn script_path(&self) -> PathBuf {
        self.root.join(SCRATCH).join("script.py")
    }

    /// Make the directory and its scratch corner. Called before every run
    /// rather than once at start-up, because a user who deletes the folder
    /// mid-session should get it back rather than an error.
    pub fn prepare(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.root.join(SCRATCH))
    }

    /// The podman invocation, as an argv.
    ///
    /// Everything hostile is switched off here rather than trusted to the
    /// image: no network, no capabilities, no way to gain any, a memory ceiling,
    /// a process ceiling and a wall-clock ceiling. The script is passed as a
    /// path inside the bind mount rather than on the command line or down
    /// stdin — a heredoc through a shell is how quoting bugs become code
    /// execution, and there is no shell in this argv at all.
    pub fn command(&self) -> Vec<String> {
        let mut argv: Vec<String> = [
            "podman",
            "run",
            "--rm",
            // Never reach a registry. Without this, an image that is not built
            // sends podman off to `https://localhost/v2/` for three seconds of
            // retries and then reports a connection error, which says nothing
            // about the actual problem. With it the failure is instant and the
            // message is "image not known", which is the truth.
            "--pull=never",
            "--network=none",
            "--cap-drop=ALL",
            "--security-opt",
            "no-new-privileges",
            "--memory",
            MEMORY,
            "--pids-limit",
            PIDS,
            "--timeout",
            &TIMEOUT_SECONDS.to_string(),
        ]
        .iter()
        .map(|word| word.to_string())
        .collect();

        argv.push("--volume".into());
        argv.push(format!("{}:/work", self.root.display()));
        if let Some(workspace) = &self.workspace {
            argv.push("--volume".into());
            argv.push(format!("{}:/workspace:ro", workspace.display()));
        }
        argv.push("--workdir".into());
        argv.push("/work".into());
        argv.push(IMAGE.into());
        argv.push("python3".into());
        argv.push(format!("/work/{SCRATCH}/script.py"));
        argv
    }

    /// Everything the model has put in the sandbox, as relative paths.
    ///
    /// Used either side of a run so the result can name what the script
    /// actually produced. Our own scratch directory is not the model's output
    /// and is left out.
    pub fn listing(&self) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        collect(&self.root, &self.root, &mut found);
        found
    }
}

fn collect(root: &Path, at: &Path, found: &mut BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(at) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let name = relative.to_string_lossy().to_string();
        if OURS
            .iter()
            .any(|ours| name == *ours || name.starts_with(&format!("{ours}/")))
        {
            continue;
        }
        match entry.metadata() {
            Ok(data) if data.is_dir() => collect(root, &path, found),
            Ok(_) => {
                found.insert(name);
            }
            Err(_) => {}
        }
    }
}

/// What a finished run produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ran {
    pub stdout: String,
    pub stderr: String,
    /// The container's exit code. Zero is a script that finished.
    pub code: i32,
    /// Killed by the wall-clock ceiling rather than by anything in the script.
    /// Worth telling apart from an ordinary failure: the fix is a different
    /// algorithm, not a corrected line.
    pub timed_out: bool,
    /// Files that were not in the sandbox before this call.
    pub created: Vec<String>,
}

impl Ran {
    pub fn finished(&self) -> bool {
        self.code == 0 && !self.timed_out
    }
}

/// Whether podman killed the container for running too long.
///
/// Measured rather than read off the exit code, because podman reports a bare
/// 255 when `--timeout` expires — not the 137 a SIGKILL would suggest — and 255
/// is also what a script gets by calling `sys.exit(255)`. The clock is the
/// signal that cannot be forged from inside the container, so the elapsed time
/// decides and the exit code is corroboration.
pub fn killed_by_clock(code: i32, elapsed_seconds: u64) -> bool {
    code != 0 && elapsed_seconds >= u64::from(TIMEOUT_SECONDS)
}

/// Trouble with the sandbox itself rather than with the script.
///
/// Kept apart from a failing script because the remedies have nothing in
/// common: one is a line of Python and the other is a command the *user* has to
/// run. A model handed "Error: ..." for both will try to fix the second one by
/// rewriting its code, forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trouble {
    /// podman is not installed, or could not be started.
    NoPodman,
    /// podman ran and the image is not built.
    NoImage,
}

impl std::fmt::Display for Trouble {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPodman => write!(
                f,
                "the Python sandbox needs podman and it could not be run. Tell the user to \
                 install podman — this is not something you can work around, and you must not \
                 answer a question that needed a calculation by doing the arithmetic yourself."
            ),
            Self::NoImage => write!(
                f,
                "the sandbox image has not been built on this machine yet. Tell the user to \
                 run `{BUILD_COMMAND}` once, which takes a few minutes. Until they do, say you \
                 cannot run the calculation rather than estimating it."
            ),
        }
    }
}

/// Whether podman failed before the script ever started.
///
/// Read out of standard error rather than the exit code, because podman uses
/// 125 for "podman itself failed" *and* a script is free to `sys.exit(125)`.
/// The wording below is podman's own for an image it cannot find, under either
/// of the two spellings it uses.
pub fn trouble(stderr: &str) -> Option<Trouble> {
    let said = stderr.to_lowercase();
    let missing = said.contains("image not known")
        || said.contains("unable to find image")
        || said.contains("short-name resolution")
        || (said.contains("error") && said.contains(IMAGE) && said.contains("not"));
    missing.then_some(Trouble::NoImage)
}

/// The result the model reads.
///
/// The rules ride here rather than in the system prompt, which is the pattern
/// the rest of this application settled on: a rule attached to the shape that
/// triggers it is read at the moment it applies, and costs nothing on the turns
/// it does not. So the note after a traceback is about fixing the script once,
/// and the note after a clean run is about answering with the number — which is
/// the failure this suite has measured most often, a model that runs the
/// calculation and then reports having run it.
pub fn frame(ran: &Ran) -> String {
    if ran.timed_out {
        return format!(
            "The script was still running after {TIMEOUT_SECONDS} seconds and was stopped.\n\n\
             {}\n\nThis is not a script to retry unchanged — it did not finish because of what \
             it was doing, not because of a typo. Either compute the answer a cheaper way or \
             tell the user it is too big a job for this.",
            section("Output before it was stopped", &ran.stdout)
        );
    }

    let mut framed = String::new();
    if !ran.stdout.trim().is_empty() {
        framed.push_str(&section("Output", &ran.stdout));
    }
    if !ran.stderr.trim().is_empty() {
        if !framed.is_empty() {
            framed.push_str("\n\n");
        }
        framed.push_str(&section("Standard error", &ran.stderr));
    }

    if !ran.finished() {
        if framed.is_empty() {
            framed.push_str(&format!("The script exited with status {}.", ran.code));
        }
        framed.push_str(
            "\n\nThe script failed. Read the traceback — it names the line and the reason — and \
             send one corrected version. If it fails a second time, say what went wrong rather \
             than trying a third.",
        );
        return framed;
    }

    if framed.is_empty() {
        framed.push_str(
            "The script ran and printed nothing.\n\n\
             Nothing was printed, so there is no result to report. Python does not echo the \
             last expression the way a REPL does — `print()` what you want to see.",
        );
    }

    if !ran.created.is_empty() {
        framed.push_str(&format!(
            "\n\nFiles created in the sandbox: {}. They are still there on the next call. \
             The user cannot see them from here — use `copy_to_workspace` if they asked for a \
             file rather than an answer.",
            ran.created.join(", ")
        ));
    }

    framed.push_str(
        "\n\nThat output is the answer. Give it to the user now, in a sentence, with the \
         figure in it — do not describe the script or say that you ran one. If it is not what \
         you needed, fix the code and run it again rather than reasoning about what it would \
         have said.",
    );
    framed
}

/// One labelled block of output, capped.
fn section(label: &str, text: &str) -> String {
    let trimmed = text.trim_end();
    let kept: String = trimmed.chars().take(MAX_OUTPUT).collect();
    let cut = kept.chars().count() < trimmed.chars().count();
    format!(
        "{label}:\n```\n{kept}\n```{}",
        if cut {
            format!(
                "\n\n[cut off after {MAX_OUTPUT} characters — print the answer rather than the \
                 whole dataset]"
            )
        } else {
            String::new()
        }
    )
}

/// What the system prompt says about the sandbox.
///
/// Short on purpose, and about *when* rather than *how*. What the environment
/// contains is in the tool's own description, which sits beside the schema the
/// model is reading as it writes the call; what to do with the output is in
/// [`frame`], which arrives at the moment it applies. This paragraph only has
/// to answer one question, which is the one the model gets wrong: is this worth
/// a script, or do I already know it?
pub fn guidance(has_workspace: bool) -> String {
    let mut note = String::from(
        "You can write and run Python with `run_python`. **Anything that has an exact answer \
         should be computed rather than recalled.** Arithmetic beyond a couple of small \
         numbers, percentages, compounding, unit conversion, dates and durations, sorting or \
         totalling a list, parsing structured text — write the three lines, print the result, \
         and report what came back. You are fluent enough at arithmetic to produce a wrong \
         answer confidently, and this is how you stop doing that.\n\n\
         Not everything, though. `2 + 2`, a fact you know, or a question about what a word \
         means is not a calculation, and reaching for the interpreter to answer one wastes the \
         user's time. The test is whether being one digit out would matter.\n\n\
         The script gets no network, so it cannot look anything up — `web_search` is still how \
         you find things out. Its directory persists between calls, so a second script can \
         build on the first one's files.",
    );
    if has_workspace {
        note.push_str(
            "\n\nThe user's workspace is mounted at `/workspace`, read-only, so a script can \
             read a CSV or a spreadsheet and compute over it. It cannot write there: use \
             `copy_to_workspace` to put a file the script produced where the user can see it.",
        );
    }
    note
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> (tempfile::TempDir, Sandbox) {
        let directory = tempfile::tempdir().expect("temp dir");
        let sandbox = Sandbox::new(directory.path());
        sandbox.prepare().expect("prepare");
        (directory, sandbox)
    }

    fn ok(stdout: &str) -> Ran {
        Ran {
            stdout: stdout.into(),
            ..Ran::default()
        }
    }

    #[test]
    fn the_container_is_cut_off_from_everything_that_matters() {
        // The whole security argument for this tool being ungated is in this
        // argv. If a flag goes missing the tool is a different tool, and the
        // reasoning in `tools::gate_of` no longer holds.
        let (_directory, sandbox) = sandbox();
        let argv = sandbox.command().join(" ");
        assert!(argv.contains("--network=none"), "{argv}");
        assert!(argv.contains("--pull=never"), "{argv}");
        assert!(argv.contains("--cap-drop=ALL"), "{argv}");
        assert!(argv.contains("no-new-privileges"), "{argv}");
        assert!(argv.contains("--rm"), "{argv}");
        assert!(
            argv.contains(&format!("--timeout {TIMEOUT_SECONDS}")),
            "{argv}"
        );
        assert!(argv.contains("--memory 1g"), "{argv}");
        assert!(argv.contains("--pids-limit 256"), "{argv}");
    }

    #[test]
    fn the_workspace_is_mounted_read_only_or_not_at_all() {
        // Read-write here would mean the model could change the user's files
        // without the approval every other write goes through.
        let (_directory, sandbox) = sandbox();
        assert!(
            !sandbox
                .command()
                .iter()
                .any(|word| word.contains("/workspace")),
            "a context with no workspace mounted one anyway"
        );

        let reading = sandbox
            .clone()
            .reading(Some(PathBuf::from("/home/someone/Projects")));
        let mount = reading
            .command()
            .into_iter()
            .find(|word| word.contains(":/workspace"))
            .expect("a workspace mount");
        assert_eq!(mount, "/home/someone/Projects:/workspace:ro");
    }

    #[test]
    fn there_is_no_shell_anywhere_in_the_invocation() {
        // The script travels as a file in the bind mount. A heredoc through a
        // shell is how a quoting bug becomes arbitrary execution on the host.
        let (_directory, sandbox) = sandbox();
        let argv = sandbox.command();
        assert!(!argv
            .iter()
            .any(|word| word == "sh" || word == "bash" || word == "-c"));
        assert_eq!(
            argv.last().expect("a script"),
            &format!("/work/{SCRATCH}/script.py")
        );
    }

    #[test]
    fn a_run_that_produced_nothing_says_so_rather_than_looking_successful() {
        // The commonest first mistake: a script that computes and never prints.
        // Handing back an empty string reads as success and the model invents a
        // number to go with it.
        let framed = frame(&ok(""));
        assert!(framed.contains("printed nothing"), "{framed}");
        assert!(framed.contains("print()"), "{framed}");
    }

    #[test]
    fn a_clean_run_is_told_to_answer_with_the_number() {
        // Measured across the whole suite as the commonest failure: the model
        // does the work and then reports having done the work.
        let framed = frame(&ok("18472.16"));
        assert!(framed.contains("18472.16"), "{framed}");
        assert!(framed.contains("That output is the answer"), "{framed}");
        assert!(framed.contains("do not describe the script"), "{framed}");
    }

    #[test]
    fn a_traceback_asks_for_one_correction_and_not_a_spiral() {
        let failed = Ran {
            stderr: "Traceback (most recent call last):\n  File \"/work/x.py\", line 2\n\
                     ZeroDivisionError: division by zero"
                .into(),
            code: 1,
            ..Ran::default()
        };
        let framed = frame(&failed);
        assert!(framed.contains("ZeroDivisionError"), "{framed}");
        assert!(framed.contains("one corrected version"), "{framed}");
        assert!(framed.contains("rather than trying a third"), "{framed}");
    }

    #[test]
    fn output_printed_before_a_crash_is_not_thrown_away() {
        // The image sets PYTHONUNBUFFERED for this reason: half a result is
        // often enough to see what went wrong.
        let framed = frame(&Ran {
            stdout: "step 1 done".into(),
            stderr: "KeyError: 'total'".into(),
            code: 1,
            ..Ran::default()
        });
        assert!(framed.contains("step 1 done"), "{framed}");
        assert!(framed.contains("KeyError"), "{framed}");
    }

    #[test]
    fn a_timeout_is_told_apart_from_a_bug() {
        // The remedies are different: a timeout wants a different algorithm and
        // a traceback wants a corrected line. A model told only "it failed"
        // sends the same script again.
        let framed = frame(&Ran {
            stdout: "counting…".into(),
            code: 255,
            timed_out: true,
            ..Ran::default()
        });
        assert!(framed.contains(&TIMEOUT_SECONDS.to_string()), "{framed}");
        assert!(
            framed.contains("not a script to retry unchanged"),
            "{framed}"
        );
        assert!(framed.contains("counting…"), "{framed}");
    }

    #[test]
    fn the_clock_decides_a_timeout_and_not_the_exit_code() {
        // Measured against podman 5.7: `--timeout` expiring reports a bare 255,
        // which is also what `sys.exit(255)` gives. A script that fails quickly
        // has not timed out however it exited.
        assert!(killed_by_clock(255, u64::from(TIMEOUT_SECONDS)));
        assert!(killed_by_clock(137, u64::from(TIMEOUT_SECONDS) + 4));
        assert!(
            !killed_by_clock(255, 1),
            "a fast sys.exit(255) is not a timeout"
        );
        assert!(
            !killed_by_clock(0, 90),
            "a script that finished did not time out"
        );
    }

    #[test]
    fn enormous_output_is_cut_with_the_fix_named() {
        let framed = frame(&ok(&"x".repeat(MAX_OUTPUT * 2)));
        assert!(framed.contains("cut off"), "{framed}");
        assert!(framed.contains("print the answer rather than the whole dataset"));
    }

    #[test]
    fn files_the_script_made_are_named_with_the_way_to_deliver_them() {
        // Without this the model produces a chart in a directory the user
        // cannot see and reports it as done.
        let framed = frame(&Ran {
            stdout: "saved".into(),
            created: vec!["chart.png".into()],
            ..Ran::default()
        });
        assert!(framed.contains("chart.png"), "{framed}");
        assert!(framed.contains("copy_to_workspace"), "{framed}");
        assert!(framed.contains("still there on the next call"), "{framed}");
    }

    #[test]
    fn a_listing_is_what_the_model_made_and_not_our_plumbing() {
        let (directory, sandbox) = sandbox();
        std::fs::write(sandbox.script_path(), "print(1)").expect("script");
        std::fs::write(directory.path().join("chart.png"), "png").expect("chart");
        std::fs::create_dir_all(directory.path().join("data")).expect("dir");
        std::fs::write(directory.path().join("data/rows.csv"), "a,b").expect("csv");

        let listed = sandbox.listing();
        assert!(listed.contains("chart.png"), "{listed:?}");
        assert!(listed.contains("data/rows.csv"), "{listed:?}");
        assert!(
            listed.iter().all(|name| !name.contains(SCRATCH)),
            "the script we wrote is reported as the model's output: {listed:?}"
        );
    }

    #[test]
    fn a_missing_image_is_told_apart_from_a_failing_script() {
        // The two have nothing in common as far as the fix goes, and a model
        // that cannot tell will rewrite its Python to fix an unbuilt image.
        assert_eq!(
            trouble("Error: localhost/familiar-sandbox:1: image not known"),
            Some(Trouble::NoImage)
        );
        assert_eq!(
            trouble("Trying to pull ...\nError: unable to find image locally"),
            Some(Trouble::NoImage)
        );
        assert_eq!(
            trouble("Traceback (most recent call last): ValueError"),
            None
        );
        assert_eq!(trouble(""), None);
    }

    #[test]
    fn both_kinds_of_trouble_forbid_answering_from_the_models_own_arithmetic() {
        // The dangerous failure. A sandbox that cannot run is a reason to say
        // so, not a reason to fall back on guessing — which is the exact thing
        // this whole capability exists to stop.
        for trouble in [Trouble::NoPodman, Trouble::NoImage] {
            let said = trouble.to_string();
            assert!(
                said.contains("yourself") || said.contains("rather than estimating"),
                "{said}"
            );
        }
        assert!(Trouble::NoImage.to_string().contains(BUILD_COMMAND));
    }

    #[test]
    fn an_empty_or_enormous_script_is_refused_before_podman_is_started() {
        assert!(Refusal::Empty.to_string().contains("no code to run"));
        let long = Refusal::TooLong(50_000).to_string();
        assert!(long.contains("50000"), "{long}");
        assert!(long.contains(&MAX_CODE.to_string()), "{long}");
    }

    #[test]
    fn the_guidance_says_when_to_reach_for_it_and_when_not_to() {
        // Both halves or neither. Told only to compute, the model runs Python
        // to add two and two; told only to be sparing, it does compound
        // interest in its head, which is the failure that matters.
        let note = guidance(false);
        assert!(
            note.contains("should be computed rather than recalled"),
            "{note}"
        );
        assert!(note.contains("`2 + 2`"), "{note}");
        assert!(note.contains("no network"), "{note}");
        assert!(
            !note.contains("/workspace"),
            "a context with no workspace was told about one"
        );

        let with = guidance(true);
        assert!(with.contains("/workspace"), "{with}");
        assert!(with.contains("read-only"), "{with}");
    }
}
