//! Turning a video link into a transcript, through Magpie's `magpie agent` CLI.
//!
//! The same shape as [`super::planner`] and [`super::github`], and the argument
//! for a subprocess is stronger here than in either. Magpie's queue lives in the
//! memory of the running application, so a second writer loses — but the real
//! reason is that a transcript is not a function call. It is four programs in
//! sequence over ten minutes: `yt-dlp` fetches the audio, `ffmpeg` converts it
//! to 16 kHz mono, `whisper.cpp` transcribes it, `sherpa-onnx` marks who is
//! speaking. Linking Magpie's model would hand this app the argument vectors and
//! leave it supervising all four and holding partial state across their
//! failures, which is the entire application. Spawning `magpie` gets the
//! pipeline, the model management, the error classification and the record on
//! disk, for one process.
//!
//! **One verb writes, and it is slow.** `transcribe` downloads a file into the
//! user's folder and spends minutes of CPU, so it is gated and it is announced
//! before it is waited on — a spinner with nothing said in front of it is how a
//! four-minute wait becomes a bug report. Everything else here reads.
//!
//! **Standard error must not be merged into standard output.** `run_in` merges,
//! which is right for `gh` and wrong here: stdout carries one JSON object and
//! stderr carries progress lines for the whole download. Merged, the progress
//! lands in the middle of the JSON and nothing parses. The progress is worth
//! having on its own — see `ui::runner::run_slow`, which reads it as it arrives.

use super::tools::Gate;

/// What may be done with a `magpie agent` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Run(Gate),
    Refuse(String),
}

/// Verbs that only read, with their aliases, from `magpie agent describe` —
/// whose `mutates` field is the authority. Anything absent is gated.
const READS: &[&str] = &[
    "help",
    "describe",
    "tools",
    "check",
    "list",
    "downloads",
    "show",
    "job",
];

/// The one verb that takes minutes. `describe` reports it as `slow`, and it is
/// what decides whether the user is told to expect a wait.
const SLOW: &[&str] = &["transcribe"];

/// How much of a response is kept. Every response here is a small JSON object —
/// the transcript itself is a file on disk, deliberately not in the reply.
pub const MAX_OUTPUT: usize = 8_000;

/// The verb, with a leading `agent` tolerated.
pub fn verb(args: &[String]) -> Option<&str> {
    let mut words = args
        .iter()
        .map(String::as_str)
        .filter(|word| !word.trim().is_empty());
    match words.next() {
        Some("agent") => words.next(),
        other => other,
    }
}

/// Whether this invocation is one the user should be warned about before it
/// starts, rather than after it has been running for three minutes.
pub fn is_slow(args: &[String]) -> bool {
    verb(args).is_some_and(|verb| SLOW.contains(&verb))
}

pub fn classify(args: &[String]) -> Decision {
    let Some(verb) = verb(args) else {
        return Decision::Refuse(
            "`magpie` needs a verb. `tools` says whether a transcript can be made at all, \
             `list` searches what has already been transcribed, `transcribe` takes a link."
                .into(),
        );
    };
    if args.iter().any(|word| word.starts_with("--")) {
        return Decision::Refuse(format!(
            "`{verb}` takes no `--flags` — Magpie's launcher rejects an option it was not \
             told about before the verb ever runs. Use key=value pairs, like \
             `transcribe <url> format=srt speakers=no`."
        ));
    }
    if READS.contains(&verb) {
        return Decision::Run(Gate::Never);
    }
    Decision::Run(Gate::Always)
}

pub fn command(args: &[String]) -> Vec<String> {
    let mut command = vec!["magpie".to_string(), "agent".to_string()];
    let rest = match args.first().map(String::as_str) {
        Some("agent") => &args[1..],
        _ => args,
    };
    command.extend(rest.iter().cloned());
    command
}

/// What the system prompt says about Magpie.
///
/// The two facts a response cannot teach are here — that it is slow, and that
/// looking first is cheap. Everything that depends on what came back is in
/// [`note_for`].
pub fn guidance() -> String {
    "`magpie` turns a video link into a transcript of what was said. Call it with the \
     arguments after `magpie agent`: `tools`, `list [text]` and `show <id>` read and are \
     instant; `transcribe <url>` downloads and transcribes, which takes minutes and will \
     ask the user first.\n\n\
     Look before transcribing. `list <text>` finds a transcript made weeks ago for the \
     price of one fast call, and `tools` says whether transcribing is possible on this \
     machine at all — it needs yt-dlp, FFmpeg and whisper.cpp, and the first transcript \
     downloads a 466 MB speech model. Do not promise a transcript before checking.\n\n\
     `transcribe` takes one video. Options are `format=text|srt|vtt`, `language=`, \
     `model=tiny|base|small|medium` and `speakers=yes|no|N`; anything not passed comes \
     from the user's own preferences, so pass only what they asked for. A playlist link \
     is refused rather than expanded.\n\n\
     The words are never in the response — it returns a path to a file. Read it with \
     `read_file` only if the user asked about what was *said*; if they asked for a \
     transcript to be made, saying where it is is the finished job."
        .to_string()
}

/// What to say before starting a call that will not answer for minutes.
///
/// Returned to the *user*, not the model: the point is that something appears
/// on screen before the wait, not that the model is told a fact it will read
/// after the wait is over.
pub fn waiting_for(args: &[String]) -> Option<String> {
    if !is_slow(args) {
        return None;
    }
    Some(
        "Downloading and transcribing — this takes a few minutes, longer for a long video."
            .to_string(),
    )
}

/// The rules that ride with a response, attached to the shape that triggers
/// them rather than carried in the system prompt on every unrelated turn.
pub fn note_for(response: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(response).ok()?;

    if parsed.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
        let error = parsed.get("error").and_then(serde_json::Value::as_str)?;
        let hint = parsed
            .get("hint")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        return match error {
            // The outcome most easily reported as a success by accident, and
            // the one that leaves 200 MB the user does not know about.
            "transcript-failed" => Some(format!(
                "This FAILED: there is no transcript, which is what was asked for. Say that \
                 first and plainly — a download that produced no words is not a success.\n\n\
                 Then say the second half: the audio did download and is still on disk{}. \
                 Leaving that out leaves the user with a large file they do not know about.",
                if hint.is_empty() {
                    String::new()
                } else {
                    format!(" ({hint})")
                }
            )),
            "tool-missing" => Some(
                "A transcript cannot be made on this machine until what the message names is \
                 installed. Relay it — the message says which command fixes it — and do not \
                 try again until it is."
                    .into(),
            ),
            "refused" => Some(
                "Playlists are refused rather than expanded, because transcribing forty \
                 videos is hours of CPU started by one argument. Ask the user which single \
                 video they meant."
                    .into(),
            ),
            "download-failed" => Some(
                "The site refused the download. The message is Magpie's own reading of why \
                 and the hint is the remedy — relay both rather than retrying."
                    .into(),
            ),
            _ => None,
        };
    }

    // `tools` before promising anything: the first transcript on a machine
    // fetches 466 MB, and a model that has not looked cannot say so.
    if let Some(ready) = parsed.get("ready") {
        if ready.get("transcribe").and_then(serde_json::Value::as_bool) == Some(false) {
            return Some(
                "Transcribing is not possible on this machine yet — `ready.missing` says what \
                 is needed and names the command that installs it. Tell the user that before \
                 offering to transcribe anything."
                    .into(),
            );
        }
    }

    if let Some(transcript) = parsed
        .get("job")
        .and_then(|job| job.get("transcript"))
        .filter(|transcript| {
            transcript.get("state").and_then(serde_json::Value::as_str) == Some("ready")
        })
    {
        let path = transcript
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("the path above");
        let bytes = transcript
            .get("bytes")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default();
        let speakers = transcript
            .get("speakers")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let mut note = format!(
            "The transcript is finished and it is at {path}. That is the answer — say where \
             it is and stop.\n\n\
             Do NOT call `read_file` on it. The user asked for a transcript to be made, not \
             for what it says, and it is {bytes} bytes of context spent answering a question \
             nobody asked. Read it only if they go on to ask about the contents."
        );
        if !speakers.is_empty() {
            note.push_str(&format!(
                " Who was speaking was worked out too: {speakers}. Say so."
            ));
        }
        return Some(note);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn only_the_verb_that_downloads_is_gated_or_slow() {
        for line in ["tools", "list", "show 3", "describe", "help"] {
            assert_eq!(classify(&args(line)), Decision::Run(Gate::Never), "{line}");
            assert!(!is_slow(&args(line)), "{line}");
        }
        assert_eq!(
            classify(&args("transcribe https://youtu.be/x")),
            Decision::Run(Gate::Always)
        );
        assert!(is_slow(&args("transcribe https://youtu.be/x")));
    }

    #[test]
    fn an_unknown_verb_is_gated() {
        assert_eq!(
            classify(&args("delete-everything")),
            Decision::Run(Gate::Always)
        );
    }

    #[test]
    fn a_slow_call_says_what_it_is_waiting_for_before_it_waits() {
        // A spinner with nothing in front of it is how a four-minute wait
        // becomes a bug report.
        let said = waiting_for(&args("transcribe https://youtu.be/x")).expect("something to say");
        assert!(said.contains("minutes"), "{said}");
        assert_eq!(waiting_for(&args("list")), None);
    }

    #[test]
    fn the_fixed_prefix_is_added_and_not_doubled() {
        assert_eq!(command(&args("tools")), ["magpie", "agent", "tools"]);
        assert_eq!(command(&args("agent tools")), ["magpie", "agent", "tools"]);
    }

    #[test]
    fn a_flag_is_refused_with_what_to_write_instead() {
        let Decision::Refuse(why) = classify(&args("transcribe url --format srt")) else {
            panic!("a flag should be refused");
        };
        assert!(why.contains("format=srt"), "{why}");
    }

    #[test]
    fn a_download_without_a_transcript_is_reported_as_both_things() {
        // The audio is on disk, so looking at the download would say it went
        // fine. What was asked for was a transcript.
        let note = note_for(
            r#"{"ok":false,"error":"transcript-failed",
                "message":"The audio downloaded, but there is no transcript.",
                "hint":"The audio is at /home/user/Downloads/A talk.m4a."}"#,
        )
        .expect("a note");
        assert!(note.contains("FAILED"), "{note}");
        assert!(note.contains("no transcript"), "{note}");
        assert!(note.contains("A talk.m4a"), "{note}");
        assert!(note.contains("still on disk"), "{note}");
    }

    #[test]
    fn a_finished_transcript_says_the_words_are_in_a_file() {
        let note = note_for(
            r#"{"ok":true,"action":"transcribed","job":{"id":7,"title":"Me at the zoo",
                "transcript":{"state":"ready","format":"text","path":"/home/user/x.txt",
                              "bytes":193,"speakers":"2 speakers · Alice, Speaker 2"}}}"#,
        )
        .expect("a note");
        assert!(note.contains("Do NOT call `read_file`"), "{note}");
        assert!(note.contains("/home/user/x.txt"), "{note}");
        assert!(note.contains("Alice"), "{note}");
    }

    #[test]
    fn a_machine_that_cannot_transcribe_says_so_before_anything_is_promised() {
        let note =
            note_for(r#"{"ok":true,"ready":{"transcribe":false,"missing":["Install yt-dlp"]}}"#)
                .expect("a note");
        assert!(note.contains("not possible"), "{note}");

        assert_eq!(
            note_for(r#"{"ok":true,"ready":{"transcribe":true,"missing":[]}}"#),
            None
        );
    }

    #[test]
    fn a_playlist_refusal_says_to_ask_which_video() {
        let note = note_for(r#"{"ok":false,"error":"refused","message":"That is a playlist."}"#)
            .expect("a note");
        assert!(note.contains("single"), "{note}");
    }

    #[test]
    fn an_ordinary_response_needs_no_note() {
        assert_eq!(note_for(r#"{"ok":true,"downloads":[]}"#), None);
        assert_eq!(note_for("MAGPIE AGENT\n\nVerbs:"), None);
    }
}
