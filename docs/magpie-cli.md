# Magpie's agent CLI

[Magpie](https://github.com/mhagrelius/magpie) is the sibling video downloader.
It exposes `magpie agent <verb>`, a JSON interface whose point is one verb:
`transcribe`, which turns a link into a text file of what was said. This is what
it does and how Familiar should drive it.

Nothing here is wired up yet. This documents the interface so the tool can be
added deliberately rather than guessed at.

## Why a subprocess and not a crate

Two reasons, and the second is the one that settles it.

Magpie's queue lives in the memory of the running application, which rewrites
`~/.local/share/magpie/library.json` on every change. A second process writing
that file would be overwritten. `magpie agent` avoids this by riding Magpie's
own command line: Magpie sets `HANDLES_COMMAND_LINE`, so when the app is running
the invocation is forwarded to it over D-Bus and the *running* instance does the
work — the download appears in its window, where the user can watch it or cancel
it. When Magpie is not running, the invoked process becomes the primary instance
and does the work itself. Either way there is exactly one writer. This is the
same argument as [planner-cli.md](planner-cli.md), for the same reason.

The second reason is bigger. A transcript is not a function call, it is three
programs in sequence over ten minutes: yt-dlp downloads the audio, ffmpeg
converts it to 16 kHz mono, whisper.cpp transcribes it, and sherpa-onnx marks
who is speaking. Linking `magpie::model` would give Familiar the argument
vectors and leave it running and supervising all four, holding partial state
across the failures — which is the entire application. Spawning `magpie` gets
the pipeline, the model management, the error classification and the record on
disk, for one process.

So: spawn `magpie`, do not link it.

## Running it

The same shape as `gh` in `src/model/github.rs` — an argument list handed to
`execvp`, no shell:

```
magpie agent transcribe https://youtu.be/dQw4w9WgXcQ format=srt speakers=2
```

is the argv `["magpie", "agent", "transcribe", "https://…", "format=srt",
"speakers=2"]`. There is no shell, so `;`, `$(…)` and `&&` in an argument are
inert strings. Keep it that way.

`magpie` must be on `PATH`; its `install.sh` puts it in `~/.local/bin`. It is a
GTK application, so it needs the user's desktop session — a `magpie agent`
launched from somewhere with no display will not start.

**Do not merge stderr into stdout.** `run_in` in `src/ui/runner.rs` spawns with
`STDERR_MERGE`, which is right for `gh` and wrong here: stdout carries one JSON
object and stderr carries progress lines while the download runs. Merged, the
progress lines land in the middle of the JSON and nothing parses. Use the plain
`run` (`STDERR_SILENCE`), or capture the two separately if the progress is
wanted.

**Output.** One JSON object on stdout, exit 0. On failure, a JSON object with
`"ok": false` and exit 1. `help` is the one exception: it prints text, because
it is meant to be read.

**Arguments are positional words and `key=value` pairs. There are no
`--flags`** — GOption parses the command line before Magpie's own code runs and
rejects any option it was not told about in advance. Do not invent flags; they
will be refused by the launcher, not by the verb.

## The verbs

Read `magpie agent help` for the authoritative list, and `magpie agent describe`
for the same thing as JSON — name, usage, arguments, what it returns, a
`mutates` boolean and a `slow` boolean. **`describe` is the right source for a
tool definition**: generating from it means Magpie gaining a verb does not
require an edit here.

| Verb | | |
|---|---|---|
| `help [verb]` | reads | text, not JSON |
| `describe` | reads | every verb as JSON |
| `tools` | reads | what is installed, which models are downloaded, whether a transcript can be made at all |
| `list [text] [limit=N]` | reads | downloads Magpie has a record of, newest first |
| `show <download>` | reads | one of them in full |
| `transcribe <url> [options]` | **writes, slowly** | the whole point |

That is the entire surface. There is deliberately no way to download a video, to
take a playlist, to change preferences or to cancel — the window is where
someone chooses 1080p or picks eleven items out of forty, and none of that is a
thing to do blind.

## `transcribe`

```
magpie agent transcribe <url> [format=text|srt|vtt] [language=en] \
                              [model=tiny|base|small|medium] \
                              [speakers=yes|no|N] [dir=path]
```

Audio only, one video. It downloads, transcribes, and answers when the words
exist:

```json
{"ok": true, "action": "transcribed",
 "job": {"id": 7, "title": "Me at the zoo", "url": "…", "state": "done",
         "status": "Saved to Downloads · 2 speakers · Alice, Speaker 2",
         "media":      {"path": "/home/user/Downloads/Me at the zoo.webm", "bytes": 252182},
         "transcript": {"state": "ready", "format": "text", "model": "small",
                        "path": "/home/user/Downloads/Me at the zoo.txt",
                        "bytes": 193, "speakers": "2 speakers · Alice, Speaker 2"}}}
```

**The words are not in the response.** `transcript.path` is a file to read, and
`bytes` says how much of a context window that costs before reading it. An hour
of speech is roughly 40 KB.

**An option not given comes from the user's preferences**, not from a default
invented for the command line — `format`, `language`, `model` and `speakers` all
start from Preferences → Transcripts. Pass the ones that matter to the request
and leave the rest alone. `speakers=no` is worth passing explicitly if speakers
are not wanted, because the preference may say yes.

**`dir=` is relative to the directory the command was run in**, which Magpie
reads from the invocation rather than from its own process — so it means what
Familiar's workspace means when the workspace is the working directory. Nothing
is expanded: `~` is refused with a message saying so, because there is no shell.

## Gating

`mutates` in `describe` maps onto `Gate`:

- `Gate::Never` — `help`, `describe`, `tools`, `list`, `show`. These only read.
- `Gate::Always` — `transcribe`. It downloads a file into the user's folder and
  spends minutes of CPU.

Do not gate on the verb name in a list maintained here; read `mutates`. If it
cannot be read for some reason, gate.

## Things that will bite

**It takes minutes, and the turn is held for all of them.** `slow` is `true` on
exactly one verb, and it means what it says: download plus transcription, with a
small model, is a few minutes for a short video and considerably longer for an
hour of conference audio. `run` in `src/ui/runner.rs` has no timeout, so this
works — but the user is looking at a spinner throughout, and a wrapper should
say what it is waiting for before it starts waiting.

**The first transcript on a machine downloads 466 MB.** The speech model is
fetched on demand, because the caller asked for a transcript and the model is
the only way to make one. `magpie agent tools` reports `speech_models[].on_disk`
and is the way to know in advance; `model=medium` is 1.5 GB, so do not pass it
without the user having asked for it. Speakers cost another 34 MB, once.

**Check `tools` before promising anything.** `ready.transcribe` is false on a
machine without yt-dlp, FFmpeg or whisper.cpp, and `ready.missing` says which in
sentences that name the command to fix it. `transcribe` refuses in the same
words and refuses in the first second — but only after the user has been told to
wait.

**A download that worked without a transcript is a failure.** This is the
outcome most easily reported as a success by accident: the audio is on disk, so
looking at the download would say it went fine. What was asked for was a
transcript.

```json
{"ok": false, "error": "transcript-failed",
 "message": "The audio downloaded, but there is no transcript: whisper wrote no transcript.",
 "hint": "The audio is at /home/user/Downloads/A talk.m4a."}
```

Say both parts: there are no words, and there is a file. Reporting it as a plain
failure leaves a 200 MB download the user does not know about.

**A playlist link is refused, not expanded.** `error: refused`, because
transcribing forty videos is hours of CPU started by one argument. Pass the link
to a single video. A `watch?v=…&list=…` link is fine — it is treated as the
video that was clicked.

**Errors are structured.** `error` is a stable kebab-case kind to branch on —
`bad-url`, `bad-value`, `unknown-verb`, `unknown-field`, `missing-argument`,
`not-found`, `ambiguous`, `tool-missing`, `download-failed`,
`transcript-failed`, `cancelled`, `refused` — and `message` is a sentence to
relay. Most carry `hint`, which usually says exactly what to do next. For
`download-failed` the message is Magpie's own classification of what yt-dlp
said — "The site asked for a signed-in account" — and the hint is the remedy,
which for that one is a setting in Preferences.

**Look before transcribing again.** `magpie agent list <text>` finds a
transcript made weeks ago, in a session nobody remembers, for the price of one
fast call. `transcript.path` on a finished job is a file that is still there
unless someone deleted it — and if they did, `bytes` is absent while `path` is
not, which is how to tell.

**Killing the command does not always stop the work.** When Magpie is running,
the job belongs to its window and carries on; the user can cancel it there. When
Magpie is not running, the work dies with the command, leaving a part-finished
download that the next attempt resumes from.

## A worked sequence

```
magpie agent tools
  → ready.transcribe, and whether the model is already downloaded

magpie agent list 'the lecture'
  → do we already have this one? transcript.path if so

magpie agent transcribe https://youtu.be/dQw4w9WgXcQ speakers=yes
  → minutes. Then read transcript.path, and say what speakers says.
```
