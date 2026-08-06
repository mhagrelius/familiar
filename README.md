# Familiar

An assistant for the GNOME desktop, in Rust with GTK 4 and libadwaita, pointed
at a local [`llama-server`](https://github.com/ggml-org/llama.cpp). The model
runs on your own GPU; the only bytes that leave the machine are a web search you
asked for.

Its memory is [Brain](https://github.com/mhagrelius/brain)'s vault — notes are
entities, `[[wikilinks]]` are relations, `#tags` are types — so what the
assistant knows is Markdown you can read, edit, and delete without either app.

See [`DESIGN.md`](DESIGN.md) for what it does and why it is built this way.

## What it does

**Projects and chats.** A project is a folder on disk, a set of tools, and
instructions added to the assistant's own — what you would call a project
anyway. The sidebar is a tree of them, and clicking one opens its page: what
you have told it to do, the files in its folder (open, rename, make or trash
them), every chat in it with a search box, and anything scheduled to run on its
own. Chats that belong to no project live under **Chats**, which is also where
you say how the assistant should behave when nothing else says otherwise.

**Thinking, separated.** llama.cpp streams a model's reasoning in a non-standard
`reasoning_content` field. Familiar shows it behind a disclosure that says how
long the model thought, and keeps it in the transcript. It is also carried back
into history, which measurably halves how much the model re-derives per turn —
all of it or none of it, because a sliding window rewrites the cached prefix
and costs a full re-prefill every turn.

**Honest numbers.** llama.cpp's own `timings` block, not wall-clock guesses:
tokens in and out, generation rate, time to first token, speculative-decode
acceptance, and how full the context window is.

**Memory that is just your notes.** `remember` appends a line to a note under a
heading it marks as its own; `recall` is Brain's hybrid search — the words and
the meaning, fused, when an embedding server is running; `forget` removes only
what it wrote and never deletes a note. Relations are `[[wikilinks]]`, because
that is how Brain already models them. It searches the whole vault, and when it
has to *create* a note it files it under `Familiar/` — so your root stays yours,
while a subject you already have a note for gets appended to where it lives.

**It notices without being asked.** The facts worth keeping arrive in the middle
of asking for something else, and a model in the middle of answering has one job
it is already doing. So a finished turn goes to a second, separate reader with
no tools, whose only job is to say what will still matter next week. Most turns
hold nothing and it says so. What it does save is said out loud, in the chat,
where you can see it.

**And it tidies up overnight.** Everything above only adds, which is what makes
it safe and what eventually makes it useless — a memory written to every day for
a year is one nothing can be found in. So once a night it looks over what it has
saved against what has actually been wanted since, collapses what is said twice,
refiles what is misfiled, and drops what has gone cold. It never touches a line
it did not write, never removes who you are, never empties a subject, and keeps
every sentence it removed where you can read it back.

**It works the answer out rather than guessing it.** A language model doing
arithmetic in its head is guessing, and it guesses with exactly the confidence
it would have if it knew. So anything with an exact answer — compounding,
conversions, dates, totals, sorting a list of figures — is written as a Python
script and run in a podman container, and what comes back is what you are told.
The container has no network at all, a directory of its own that survives
between calls, and your workspace mounted read-only, so a script can compute
over your files and cannot change them. Getting a file it made *out* of there
asks you first, like every other write. It needs podman and one build of the
image; without them it says so rather than falling back on guessing.

**Your mail, read and sorted.** Search it, read it, label it, file it. Sending
and deleting stop and ask you first; reading and filing do not, because nobody
wants forty dialogs to sort a morning's post. What it finds can become a task in
Planner. And the rule underneath all of it: a message is *data*, never an
instruction — mail is the one thing anyone can put in front of your assistant
for free, so a message telling it to forward an invoice gets reported to you,
never obeyed.

Gmail is what it is built for, and Gmail is not quite IMAP: labels rather than
folders, `[Gmail]/Trash` rather than `Trash`, archiving that removes a label
instead of moving anything, and a search box whose syntax — `has:attachment`,
`older_than:7d` — happens to be the syntax the model already knows, so on Gmail
your words go over almost unchanged. Set it up with your address and a
[Google app password](https://myaccount.google.com/apppasswords); the servers
and ports fill themselves in, and are there behind an expander if you are not on
Gmail. An app password rather than a sign-in with Google because the second one
means shipping a client secret inside an application you can read the source of,
where it is not a secret at all.

**It can reach for what it hasn't got.** Most capabilities are off in a new
conversation, which keeps the assistant sharp — a small model with a long tool
list picks the wrong tool, and that is measured rather than assumed. So instead
of switching everything on, it carries a one-line menu of what exists, and turns
one on when the conversation actually needs it: ask for a spreadsheet and it
switches on documents and makes you one, in the same turn. It only offers what
would really work on your machine, everything that asked permission before still
asks, and the switch stays on afterwards where you can see it and turn it back
off. If it needs somewhere to put files and you have not chosen a folder, it
makes `~/Documents/Familiar` and tells you — every write into it still stops and
asks first.

**Something worth saying, only when there is something worth saying.** Now and
then it looks over what is due, what has arrived, and what the weather is
warning about, and decides whether any of it is worth a notification. Almost
always it decides no. That is the point: a notification that fires every day is
one you mute, and then the useful one is invisible too.

**A stronger model, when it is genuinely stuck.** It can hand one question to
Claude or Codex and use the answer — but only when you ask, or when it has
really tried and failed. You approve the exact words before they leave the
machine, because that is the only leakage control worth having.

**Tools with a visible gate.** Calls appear as chips carrying the tool, its
argument and its result — and clicking one opens what it was asked and what came
back, so "done" can never hide a no-op. Anything that changes something outside
your notes stops and asks first, showing exactly what it would run.

**Images and PDFs.** Paste or drop a picture and ask about it. Drop a PDF and it
is read page by page — text where there is a text layer, rendered and looked at
where there is not — so a typed report with a scanned appendix works, and an
answer can cite the page it came from.

**Documents it can make.** Switch a project on and it writes Word documents,
Excel workbooks, PowerPoint decks and PDFs into its folder, behind the same
approval dialog as any other write — and merges or splits PDFs you already
have. The files are written in-process: no LibreOffice, no Python, no
conversion step. Numbers in a spreadsheet are numbers, `=SUM(B2:B9)` is a
formula, and headings in a document are real Word styles, so the navigation
pane and a table of contents work.

Each format has a skill in Anthropic's `SKILL.md` shape — a description that
sits in the prompt, and a body the model loads with `read_skill` when it
actually needs it. All four descriptions cost about 250 tokens; the bodies they
defer are around 3,500.

These tools *write*; they cannot open a document that already exists. The Python
sandbox can, so with both switched on the assistant is told which to use for
which: the tools for making anything they can make, a script for reading an
`.xlsx` back or for something the tools cannot express. That split was measured
rather than assumed — asked to produce one specified document, model-written
`python-docx` got it right five times in six, and the tools get the format right
every time because the model never touches it.

**Weather.** Current conditions, a seven-day forecast and any active watches or
warnings, from the US National Weather Service — no account, no key. Set a
latitude and longitude in Preferences. US only, and it says so rather than
guessing about anywhere else.

**GitHub, through `gh`.** It is already signed in, so a project can ask about
pull requests, issues, workflow runs and the API directly. Reading runs
immediately; anything that writes stops for your approval with the exact command
shown. Only `gh`, only as an argument list — there is no shell, so there is
nothing for a pipe or a semicolon to do.

**Your tasks, and your transcripts.** Two more projects' worth of tools, each
driving a sibling app through its own command line rather than reaching into its
files. **Planner** reads and changes your task list — `overview`, a filter query,
a quick-add line — and reports what it actually did rather than what it was
asked to do, so a task that landed in the Inbox because the project did not
exist says so, and a repeating task that comes back next week is reported as
both things. **Magpie** turns a video link into a transcript: it looks for one
you already have before spending minutes making another, says what it is waiting
for before it starts, and shows the download's progress on the chip while it
runs. The words come back as a file, not as forty kilobytes of context.

**Chats that wake on their own.** Give a chat a schedule and a standing prompt
— "weekdays at 07:00, check my pull requests and the forecast" — and it runs
itself and notifies you. It is the *same* chat, so it can refer back to what it
found last time. A run the machine slept through is skipped rather than
delivered stale. **Scheduled Chats** in the menu lists them all with their
status; click the schedule or the prompt to change either, and pause or remove
one without touching the conversation.

You can also just ask for it — "set me up a morning briefing at seven" switches
scheduling on and sets one, and you approve the exact time and the exact
standing prompt before it agrees to anything. Until this existed the assistant
would make you a *task* instead, reminding you to come and ask for the briefing,
and then tell you it had no scheduler at all.

**A job with steps, that you get to change.** On out of the box, with notes, the
web and the weather. Ask it to work out how something gets done and it writes a
plan — three to a dozen steps — and then *stops*. The
plan appears in a strip above where you type: open it, reorder the steps, reword
them, add a note to any one of them, then press Start. It works through one step
at a time, and each step's outcome sits under it, so "done" cannot hide a step
that did nothing.

A note you add to a step is yours. The assistant never writes one and never
overwrites one, and it is handed the note at the moment it reaches that step
rather than carrying all of them all the way through. If you change the plan
while it is running, the next thing it reads says so — it is not allowed to carry
on from what it remembers.

**Keep** files the shape under the project as Markdown you can open in any
editor, and "run the quarterly comparison workflow" starts it again. It will not
save one uninvited, and it will tell you plainly when there is no saved workflow
by the name you used rather than inventing what it thinks was in it.

**Talk to it.** Press a shortcut anywhere — with the window closed, on another
workspace, in the middle of something else — say what you want, and stop
talking. Silence ends the utterance; there is nothing to hold down. What you
said appears as you say it, the answer is read back a sentence at a time as the
model writes it, and the whole thing lands in an ordinary chat that memory,
schedules and the sidebar treat like any other. Ask again within eight minutes
and it carries on the same chat; the window names the one it is continuing and
starts a new one on request.

Speech is recognised on this machine and no audio leaves it. The models are not
shipped — `packaging/fetch-speech-models.sh` gets them, or Familiar reads
[Scribe](https://github.com/mhagrelius/scribe)'s copy if that is installed.
Answers are read back through speech-dispatcher, which is already on the
desktop and sounds like it, or through Kokoro for a voice that sounds like a
person — `packaging/speech-server.sh` runs one. **You can talk over it.** The
microphone stays open while it thinks and while it speaks, so starting to talk
stops the answer and takes the new question; say nothing for a few seconds and
the conversation ends on its own. Everything else works without any of it.

**Long chats keep working.** When a chat grows into the top of the context
window — measured against what the server says the window is, not counted in
turns — the older exchanges fold into a rolling summary written by the model
itself, between turns, so nothing waits on it. The transcript on disk keeps
everything, and a note in the chat says what left the model's view.

## Status

All twelve milestones are built. `web_search` and `fetch_url` are declared to
the model but not connected to a provider — they say so rather than pretending
— and it installs with `./install.sh` into `~/.local`, which is the only way
it is distributed.

The document *tools* only write. To read a `.docx`, `.xlsx` or `.pptx` back,
switch on the Python sandbox — it has the libraries, and with both on the
assistant is told which to use for which. `read_pdf` reads PDFs on its own.

Mail has never run against a real server. The protocol and the transport are
tested against a fake one, and the Gmail dialect is written from its
documentation rather than from a session with it.

Voice is **toggle, not push-to-talk**: the `GlobalShortcuts` portal is the only
thing that hands out a key release, and on this desktop it refuses every caller
without an application identity that a non-Flatpak app has no way to declare —
measured, not assumed. The speech endpoint has been written and tested against
its own request shape but never against a running Kokoro; the desktop voice
has. The first build on a machine
downloads an ONNX Runtime into `~/.cache`, so it needs the network once.

## Development

```sh
./test.sh              # fmt, clippy -D warnings, tests
./test.sh --headless   # the same under Xvfb

packaging/build-sandbox.sh   # the image `run_python` runs in, once per machine
packaging/fetch-speech-models.sh   # the speech models, once per machine

cargo run --release --example hear -- some.wav     # the speech model, no window

cargo run --example preview -- /tmp/preview        # the UI, painted offscreen
cargo run --example ask -- "why is the sky blue?"  # the transport, no window
cargo run --example tools -- ~/Notes "what do you know about me?"
cargo run --release --example memory -- read ~/Notes "I use Zed now." "Noted."
cargo run --release --example memory -- index ~/Notes    # embed what the vectors lack
cargo run --release --example memory -- recall ~/Notes "what the contractor did"
cargo run --release --example memory -- dream ~/Notes    # add --apply to carry it out
cargo run --example office -- /tmp/out              # one of each document
cargo run --example weather -- 40.0529 -83.0925     # the real forecast
cargo run --example news -- "Gemma 4" 30            # every news lane, for real

cargo run --release --example eval -- --repeats 3 --out baseline.json
cargo run --release --example eval -- --repeats 3 --no-catalogue --baseline baseline.json
cargo run --release --example eval -- --repeats 3 --html eval-report.html
cargo run --release --example eval -- --suite memory --repeats 3
cargo run --release --example eval -- --suite lookout --repeats 4
```

The examples exist because a transport problem and a UI problem are otherwise
indistinguishable. `ask` proves the wire; `tools` proves the agentic loop
against a real vault; `memory` drives the passive reader, a night's
consolidation and a semantic search against your own notes, and prints what it
would do before it does anything — it is also the quickest way to find out
whether the embedding server is reachable, since everything else degrades to
matching words in silence; `preview` answers "does this look right?" without needing a
compositor's permission to take a screenshot; `office` writes the four file
formats so they can be opened, because "will Word take this?" is not a question
a unit test answers.

The Python sandbox is the one capability whose tests need something installed.
`tests/sandbox.rs` runs real containers to check that the isolation the design
rests on is actually there — no network, a read-only workspace, none of the
host — and skips itself entirely when podman or the image is absent, because a
machine that has not built it is not a machine with a bug.

`eval` is the one that grades the system prompt. It runs 112 conversations past
the model and scores *how it worked* — which tool it reached for, in what order,
with what arguments — never whether a tool's answer was right, because no tool
is run: every result is invented. `--persona FILE` swaps the prompt, `--baseline`
diffs two reports scenario by scenario, and antipatterns nobody wrote a scenario
for (repeating a call, arguing with a decline, thrashing, answering with nothing
at all) are counted separately. `--list` prints what it asks; `--filter` and
`--verbose` are for iterating on one family at a time.

`--suite recall` is a second set of scenarios asking a different question: what a
thread still holds ten turns in. Facts are planted at turn two and asked for at
turn ten, past the point where compaction starts folding. `--compaction off`
measures the model reading the whole thread; `--compaction model` measures what
the app actually sends, and the gap between them is what folding costs.
`headings` is the fallback used when the summarizer cannot be reached, kept as a
floor. Half the scenarios offer no tools, so a right answer has to have come from
the conversation rather than a search; the other half keep the tools on, because
reaching for the web to answer something you were already told is its own
failure.

`--suite lookout` is a fourth, grading the proactive check: given a day's
signals, is there one thing worth a notification? Five of the nine cases expect
silence, because the failure mode of a proactive assistant is eagerness — and a
notification that fires on an ordinary Tuesday is one you mute.

`--no-catalogue` runs the main suite without the menu of switched-off
capabilities, which is the only way to find out what carrying it in every prompt
costs. The argument for a menu over switching everything on is that it is
cheaper; that is a claim with a number behind it or it is nothing.

`--overlap current|reword|disambiguate` settles an argument about one English
word. `gh workflow list` is a real subcommand and the `workflow` capability is a
real tool, so a project with both on has two plausible places to send "run the
deploy workflow". Rather than guess, the three candidate fixes are three arms and
the `overlap` family is run under each. Its scenarios come in pairs — the same
ask with the capability on and off — because "cannot find `gh` at all" and
"picked the wrong one of two" look identical in a trace and want opposite fixes.
The decision rule was written down before the first run, including the outcome
where nothing changes.

`--html FILE` writes the run as one self-contained page: every scenario, what it
asks, **what it expects and why that expectation might be wrong**, and what the
model actually did on each run, with the failing run open first. It exists
because the terminal report answers "did it get worse" and cannot answer the
harder question — *are these the right expectations?* — which needs the ask, the
assertion and the trace side by side, for the passing scenarios as much as the
failing ones. Search, filter to what is not 100%, and type into the box under
any scenario; "Copy my notes" hands back a list naming each one. Notes live in
your own browser and go nowhere until you copy them.

`--suite memory` is a third set, grading the two calls that are not turns at
all: the passive reader deciding what to save, and the nightly pass deciding
what to let go. Half of every family scores the assistant *not* acting, because
the failure mode of both is confidence rather than timidity — a reader that
saves something every turn fills a vault with sediment in a fortnight, and a
model shown a list and asked what to remove will remove things. What is scored
is what the application would do: a reply goes through the same gate, parser,
vetting and policy that run before anything touches a vault.

Reports record which compaction arm they were run under, so two of them cannot
be compared as a prompt A/B by accident, and `--repeats 6` is the smallest
sample worth believing — a suite-level number hides a regression confined to one
scenario, so read the per-scenario lines.

## Installing

```sh
./install.sh           # into ~/.local, no root
```

It needs a `llama-server` to talk to and looks for one at
`http://127.0.0.1:8080` unless Preferences says otherwise.

### Voice

Talking to it needs two more things, and neither is installed by `install.sh`:

```sh
packaging/fetch-speech-models.sh    # ~700 MB into ~/.local/share/familiar/models
packaging/speech-server.sh          # optional: Kokoro, for a voice worth hearing
```

and a shortcut, which is switched on in **Preferences → Voice**. It is off
until asked for: registering a system-wide key and opening a microphone on
somebody's behalf at first launch is not a thing to do. The default is
**Super+Alt+Space**, changed by pressing the keys you want.

`pw-record` is needed to listen and comes with PipeWire — `pipewire-bin` on
Debian and Ubuntu, `pipewire-utils` on Fedora. `spd-say` is needed to speak
back and comes with speech-dispatcher, which is usually already there. Neither
is a build dependency; both are checked for at the moment they are used and the
Voice page says which is missing.

`uninstall.sh` takes the shortcut back out. The models are left where they are;
delete `~/.local/share/familiar/models` if you want the space back.
