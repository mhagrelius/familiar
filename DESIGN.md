# Familiar — design for review

A GTK 4 / libadwaita assistant for GNOME, in Rust, pointed at a local
`llama-server`. Built the way Stickies, Planner and Brain are built: a GTK-free
`model/` half that `cargo test` exercises with no display and no server, an
imperative `ui/` half of `glib::wrapper!` subclasses, no blueprint, no `.ui`
XML, no meson, no async runtime.

It is `llamatui` re-thought as a desktop app rather than ported. The terminal
constrained that design in ways a window does not, and two of its subsystems
turn out to be things this machine already has.

## Scope

**Local by default.** The model runs on your GPU. The only bytes that leave the
machine are a web-search query and a page the assistant fetches, and only if
those tools are on and the model reaches for one.

**The vault is the memory.** Familiar does not build a knowledge graph. Brain
already owns one — notes are entities, `[[wikilinks]]` are relations, `#tags`
are types — and it stores it as Markdown files you can read without either app.
Familiar links `brain::model` and writes into the same vault. This is the
largest single decision in the design and the rest of the memory section is its
consequences.

No accounts, no sync, no mobile, no multi-user. One person, one machine, one
GPU.

## What it does

### Projects and chats

A **Project** is what a person would call a project: a name, instructions that
are added to the assistant's own, a set of enabled tools, a folder on disk, and
the chats that belong to it. It is the durable workspace the cowork model calls
a Context, named after the thing the user already has rather than after the
idea — and a project *is* its tool bundle, which is what makes MCP-into-Planner
coherent later: a "Planning" project gets the planner tools and nothing else.

**The default project is the chats that belong to no project.** It always
exists, it is called *Chats* in the window, and it cannot be renamed or
deleted. It is a project in every other way, which is what makes it the place
to set the assistant's ordinary behaviour: whatever its instructions say is
added to the built-in ones for every chat that is not in a project.

**"Project" is a word in the window and never in the prompt.** The model
already has two meanings for it — Planner's `#Project` and the memory tool's
`project` kind, both scored by their own eval families — so nothing about a
project is composed into the system prompt except the instructions the user
typed. The model is told about a **workspace folder**, which is the word its
tools have always used. `capability.rs` holds that line with a test.

A **Chat** is one continuous conversation inside a project; `Thread` in the
code, because that is what the file on disk has always been called. A **Turn**
is one user submission plus the assistant's streamed response. On disk:

```
~/.local/share/familiar/
  projects/
    default/
      project.json          instructions, tools, folder, model overrides
      threads/
        2026-07-31T14-02-11.json
    planning/
      project.json
      threads/…
```

The layout before 2026-08-03 was `contexts/<slug>/context.json`, with the
default project called `main-line`. `Store::migrate` renames all three, once,
and only where the old name exists and the new one does not.

One thread is one JSON file, appended a turn at a time, written tmp → fsync →
rename. There is no database. Brain measured this ground already: a thousand
files read off disk in under two milliseconds, and the whole reason a SQLite
transcript existed in `llamatui` was that Textual had nothing better. If a
thread search ever needs an index it is built in memory at startup like Brain's.

### The turn

A turn arrives as a stream and is folded into structured state exactly once, by
`model/turn.rs`. `TurnStream` is the only code in the app that knows
llama-server's wire shape — that `reasoning_content` is where llama.cpp puts
thinking, and where it hides its `timings` block. Everything above it sees a
`TurnState`: thinking, answer, tool calls, time-to-first-token, usage.

The reverse fold is `ui/turn_view.rs`: `TurnState` → one `FamiliarTurn` widget.
It owns the render throttle, the live tokens/second estimate, tool-chip
bookkeeping and the thinking-pane settle policy. The widget itself is
mechanical — setters only, no policy — and the same fold renders a live stream
and a thread loaded from disk, so a reopened conversation cannot look different
from the one you just had.

**Reasoning goes back when the template can use it.** This started as "history
never carries reasoning", inherited from Cogsworth, and it was wrong here.
llama-server *accepts* `reasoning_content` on a history message, and the
froggeric template re-emits it inside `<think>` tags when started with
`preserve_thinking` — which this server is. What is genuinely rejected is a
structured `text_reasoning` content part, which is a different shape; that is
what the original rule was really about.

Measured over a six-turn design conversation, carrying it roughly halves how
much the model re-derives per turn — 42,000 characters of thinking down to
24,000 — for the same volume of answer. Since generation is ~30× slower per
token than prefill, that is a large end-to-end win.

**All of it or none of it, never a window.** Carrying the most recent two turns
was measurably the worst option: as old reasoning falls out of the middle of the
prompt it rewrites the cached prefix, which cost a 4,000-token re-prefill on
every turn. Carrying everything only ever appends, so the prefix survives — the
final turn of the measured thread prefilled 23 tokens. Size is bounded by
compaction, which folds at turn boundaries where a rewrite is expected anyway.

### Reading a reply

The assistant answers in Markdown, and Familiar renders it with Brain's
scanner. `brain::model::markdown::scan` already reports which characters are
*syntax* in char offsets, which is exactly what a `GtkTextBuffer` wants, so an
answer is styled the way a note is styled: headings scale, code gets a mono face
and a background, `[[links]]` are blue and open in Brain. Unlike Brain's editor
the markers are hidden unconditionally — nobody is editing a reply — and the
view is not editable.

Hiding the syntax is not the same as taking back the room it occupied, and the
difference is most of the scrolling in a long answer. The buffer holds what the
model wrote, so a paragraph break is an empty line costing a line of prose, a
fence is two more, and the newline that ends a table's source asks for a line of
height under the grid that replaced it. So a blank line between blocks is kept
at a fraction of its height — the gap is what says two paragraphs are two — and
a line with nothing left on it to draw loses its newline as well as its
characters. On a reply with six headings, a table and a code block in it that is
a fifth of the height, measured off the conversation preview.

Thinking sits above the answer behind a disclosure that reads "Thought for 4s"
once the turn settles, collapsed by default and remembered per preference. It is
set at caption size and dimmed: present, subordinate, never competing with the
answer.

### Metrics

llama.cpp's `timings` block gives true prefill and generation throughput,
speculative-decode acceptance, and prompt versus generated token counts.
Familiar shows the honest numbers rather than wall-clock guesses: one dimmed
caption line under each finished turn, and a context-usage indicator in the
bottom bar that fills as the thread grows. Watching a 5090 saturate is half the
fun of running your own model and a GUI has room for it that a TUI did not.

### Memory is the vault

The memory tools write Markdown into Brain's vault.

- **`remember`** — appends an observation to the note for a subject, creating
  the note if it does not exist, marked with `#familiar` so what the assistant
  wrote is always separable from what you wrote. A relation is a `[[wikilink]]`
  in the sentence, which is how Brain already models relations, so no second
  edge format exists. The same sentence twice is written once.
- **`recall`** — `brain::model::search::hybrid`: BM25 over the vault's index,
  fused with cosine similarity over Brain's vector store when an embedding
  server is running. A hit found by meaning alone comes back marked, because
  "the words were there" and "only the vectors liked this" are different degrees
  of confidence and a caller that cannot tell them apart reports a near-miss as
  an answer.
- **`forget`** — removes an observation line. It never deletes a note and never
  touches a line it did not write. Your files are yours; the assistant is a
  guest in them.

**Three rules hold everywhere, and they are why this is safe to point at a
person's notes.** It only ever appends. It only removes lines it marked. A note
is never deleted — emptying the observations leaves the file.

#### What is stored, and where

A vault is not only Familiar's, so the protocol has to say which lines are whose
without keeping a shadow copy to compare against.

| What | Where | Whose |
|---|---|---|
| An observation about a subject you already have a note for | that note, under `## Noted by Familiar` | yours; Familiar's lines are marked inside it |
| An observation about a subject with no note anywhere | `Familiar/<Subject>.md` | Familiar's, until you edit it |
| What has been reached for, and when | `~/.local/share/familiar/memory-use.json` | Familiar's, and not a note |
| What a night's tidy-up removed | `~/.local/share/familiar/dreams.json` | Familiar's, and not a note |
| Vectors | Brain's cache, shared | derived, rebuildable |

Writes are scoped and reads are not. The assistant joins your notes; it only
keeps its own foundlings together. The two sidecars are outside the vault
because they are records *about* the notes rather than notes — `uses=7` written
beside a sentence in your journal would be telemetry in your journal.

#### Four tiers, and only two of them are new

"Memory" is four different things with four different lifetimes, and most of the
confusion in this area comes from calling them one word.

| Tier | Where it lives | How long |
|---|---|---|
| The turn | the request | one exchange |
| The thread | `compaction::Fold`, on the thread file | one conversation, for ever, in the transcript |
| The vault | Brain's notes | across every conversation |
| The prompt | the ambient block | recomputed at thread boundaries |

The first two already existed. A thread's rolling summary *is* per-conversation
memory — it is written by the model, carried on the thread, and re-read on every
later turn of it — and nothing about the vault replaces it. The vault is the
cross-thread tier: what is true of the user rather than of one conversation. The
ambient block is the projection of the vault into the prompt, and the only tier
with a running cost, which is why it is the only one with a budget.

The lineage is worth naming, because none of this is novel and pretending
otherwise would be the wrong way to design it. MemGPT/Letta splits core from
archival memory and runs a separate agent over it while nothing else is
happening — that is the core/archival line and the dream. ChatGPT distinguishes
saved memories, which a user can list, from referenced history, which is
retrieved — that is `remember` versus `recall`. The generative-agents papers
score retrieval as recency × importance × relevance — that is the salience
function, with "importance" replaced by the kind's weight and by how often
something has actually been reached for, which is a number rather than a
judgement.

What is specific here is that all of it is Markdown in a folder the user already
owns. There is no memory store, no import step, and no representation of a fact
that survives deleting the line.

#### Kinds, and the line between core and archival

Every saved line carries a **kind** in the HTML comment that already held its
date. Four of them, and the split is the one every assistant-memory system
arrives at — MemGPT's core versus archival blocks, ChatGPT's saved memories
versus referenced history — between what has to be in front of the model and
what it can go and look up.

| Kind | What it is | Half-life | In every prompt |
|---|---|---|---|
| `profile` | who the user is | never expires | yes |
| `preference` | how they want things done | a year | yes |
| `project` | what they are working on | three months | no |
| `fact` | anything else durable | six weeks | no |

A preference the model would have to `recall` before honouring is a preference
it will not honour, because nothing in the turn tells it to go looking. That is
the whole reason the split exists, and it is why the most useful thing the
nightly pass does is refile a standing instruction that got saved as a passing
fact.

Lines written before kinds existed parse as `fact`, which is the honest reading
of an unlabelled observation and needs no migration.

#### The ambient block has a budget

What rides in the prompt is **About the user** (core memory), **Learned
recently** (the rest, by salience) and **From the user's own notes** (the vault's
most-linked notes) — under a hard character ceiling, filled in that order.
Salience is kind weight × recency decay × how often it has been reached for, so
what makes the cut is what has been *wanted* rather than what happens to have
been written last. Six months of use produces the same size of prompt as six
days; the test is a property.

The block is untrusted data and is framed as such: a notice that it is reference
material and never instructions, facts wrapped in `<saved_memory>` delimiters.
That is a soft mitigation; the hard one is structural, in that these tools only
ever read and append text.

#### Remembering without being asked

`remember` is a tool, and a tool has to be reached for. That works when someone
says "remember that" and works badly the rest of the time: the durable facts in
a conversation arrive in the middle of asking for something else, and a model in
the middle of answering has one job it is already doing.

So a finished turn goes to a second reader — a separate low-temperature call
with **no tools** whose only job is to say what will still matter next week.
Letta calls the equivalent sleep-time compute. Three properties make it safe to
run after every turn: it cannot act (it returns candidates, and `remember` is
what writes); it is gated by a pure function over the user's message before it
costs anything; and everything it proposes is vetted for length, for being about
the assistant rather than the user, and for already being in the vault.

The conversational model is *not* told this exists. The two are belt and braces
— it saves what the user plainly states, out loud so they can see it, and the
reader catches what it missed. Duplicates cost nothing.

#### Dreaming

Everything above only adds, which is what makes it safe and what makes it,
eventually, useless: a memory written to every day for a year is one nothing can
be found in. So there is a pass that takes things out, on a schedule, at night.

The reason to do it later rather than at save time is that the evidence only
exists later. When a fact is saved nothing is known about it. Weeks on, three
things are: whether anyone ever reached for it, whether its subject has come up
in conversation at all since, and whether something else has come to say the
same thing better.

Two passes. **Arithmetic** is pure — exact duplicates, and things that are old,
unused, unmentioned and below a decay floor — and it is what runs when no server
is up. **The model** makes the judgements arithmetic cannot: that two
differently-worded sentences say one thing, that a value has been superseded,
that a passing fact turned out to be a standing preference. It proposes;
`Policy` disposes.

The rails are most of the design, because this deletes text out of a person's
notes while they are asleep:

- A `profile` observation is never dropped, and never merged away either.
- Nothing younger than thirty days, or reached for inside sixty.
- At most twenty lines a night, and at most a quarter of everything held. A
  vault bigger than one batch is several requests, and what is left of the
  night's ceiling is carried between them — ten plans each staying inside the
  budget is not the same as staying inside the budget.
- At most half of any one note, so a subject cannot be emptied. This is the rail
  that stops the two worst answers measured: shown a fact worded twice the model
  dropped *both* as duplicates of each other, and shown a date that had been
  changed it dropped the old value as superseded *and* the new one as stale.
- Every removed sentence is written to `dreams.json` first, so "it forgot
  something I wanted" has an answer that is not "it is gone".
- A merge is one atomic write of the whole note, so there is no moment at which
  it is half-done. It got there the hard way: appending the replacement and then
  striking the originals could not tell the replacement from an original it
  matched word for word, which is exactly what collapsing a duplicate looks
  like, and the note came out with three lines where it started with two.

The rails apply to deletion. A merge is exempt from the age and use ones, because
it leaves a line behind saying the thing — a rail that counted it made a
well-used note the one thing consolidation could never tidy.

Scheduling reuses `heartbeat::Schedule`, so a night the laptop slept through is
skipped rather than done at lunchtime — the same rule, and the same arithmetic,
a scheduled thread follows.

#### Vectors

`ui/embedder.rs` is the only socket besides the model and the web: one worker
thread owning one `soup::Session`, talking to an embedding server started with
`--embeddings`. **Its address comes from Brain's config, not Familiar's** —
there is one vault and there had better be one set of vectors over it, and two
applications embedding the same notes with different models would each
invalidate the other's cache on every launch. It need not be this machine: the one this was
built against is a NAS on the tailnet, doing **CPU** inference, and three things
follow from that.

**Two threads, not one.** A catch-up over a real vault is hundreds of requests at
about a second each, and a `recall` sharing that queue would sit behind the whole
pass rather than behind one request. So there is an interactive lane and a batch
lane, each with its own session — a session belongs to the thread that made it —
and its own timeout. A query embeds in 0.05 s and is given 8 s, because somebody
is watching; a batch of chunks is given 60 s, because nobody is. Both rest for a
minute after a failure, or every lookup made while the box is asleep would wait
out a connection first.

**Chunks are cut to 1,500 characters before they are sent.** Brain splits notes
at 2,000 and calls that "a conservative 512 tokens for English prose". It is not:
measured prose came out at 519 tokens and llama.cpp refused it outright, because
the physical batch defaults to 512. Every note holding a full-size chunk failed
to embed — and failed *silently*, because a note with no vectors simply does not
come back from a semantic search.

**And four chunks to a request.** All sixteen of a note's chunks at once measured
17.8 s: one long request holding the lane, and on a slower day one that trips the
timeout and loses the whole note. It is blocking on purpose — a catch-up pass is every note
in the vault, one request each, and there is nothing to watch while it happens.
Replies come back through `glib::idle_add_once`; there is no channel crate and
no runtime.

The whole thing is optional. No server, a server without `--embeddings`, a model
that changed underneath the cache: each ends with `recall` matching words, which
is what it did before and is a perfectly good answer. The store is Brain's, at
Brain's configured URL, because there is one vault and there had better be one
set of vectors over it.

Both apps can hold the vault open at once. Writes go through `brain::model::vault`
so they are atomic per note, and a `gio::FileMonitor` on the vault keeps
Familiar's index current when Brain — or git, or you — changes a file underneath
it.

### Tools

Three shapes, and the shape is the security boundary:

| Shape | Example | Gate |
|---|---|---|
| In-process, over the vault | `remember` / `recall` / `forget` | never |
| Remote MCP, over HTTP | Exa `web_search`, `fetch_url`, `news` | never; egress only |
| Local, mutating | workspace `write_file`, `run_command` | always |
| Local, in-process, writing a file | `create_document`, `create_pdf` | always |
| One named program, as an argv | `gh pr list` / `gh pr merge` | by subcommand |
| Arbitrary code, in a sealed container | `run_python` | never |
| One named program, as an argv | `mail search` / `mail send` | by verb |
| Egress to somebody else's model | `escalate` | always |

A gated call pauses the turn and raises an `AdwAlertDialog` naming the tool and
showing its arguments, Cancel first and the specific verb last, destructive
appearance for anything that writes. Denying returns a denial to the model and
the turn continues rather than dying. `run_command` is excluded from any
"approve the rest" affordance — that one always asks.

Calls appear inline in the turn as chips carrying the tool, its primary argument
and its *result*, so "done" cannot mask a no-op or an error. A small local model
sometimes leaks a `<tool_call>` into its prose without executing anything; that
text is stripped from what is shown and what is persisted.

### Scheduling, and a capability nobody had told the model about

The heartbeat has been in the application since the ninth milestone: give a chat
a schedule and a standing prompt and it runs itself. It was driven entirely from
the menu, and **nothing ever told the model it existed**.

What that produced, in real use: asked to "set up a scheduled task that lands me
a morning briefing", the assistant created a *Planner task* — a recurring
reminder for the user to come and ask for the briefing themselves — and then
explained that it had "no scheduling capability that auto-triggers on its own",
"no cron or background scheduler I can tap into", and that its tools "all run
reactively". Every one of those statements was false, and the last of them is
the worst kind of failure this application can have: the user was told a
capability they own does not exist, in a confident sentence, by the thing that
owns it.

The substitution is the instructive part. It did not invent a scheduler and it
did not refuse — it picked *the nearest thing in the menu*, which was Planner,
and then rationalised the gap. A capability that is absent from the catalogue is
not neutrally absent; it is a hole that an adjacent capability gets pulled into.

So `schedule` is a tool now: `set`, `show`, `clear`, writing the same
`Thread.heartbeat` the Scheduled Chats window writes, so anything the assistant
sets up is paused and removed exactly where everything else is. It is gated,
and the gate is what makes it offerable at all — a schedule is a standing
commitment to spend tokens and run tools while nobody is watching, and the
dialog is where the user sees the exact time and the exact standing prompt
before any of that becomes true.

`when` is parsed rather than free-form: `daily at 07:00`, `weekdays at 08:30`,
`Mondays at 09:00`, `every 4 hours`. Anything else is refused rather than
guessed at, because a briefing that silently lands at midnight is worse than one
that was never set up — the user believes in it. `7pm` becoming 07:00 is the
same failure twelve hours later, so the am/pm handling has a test of its own.

The `scheduling` family scores it at 100%, and **both** tools are switched on in
every scenario. A family offering only `schedule` would pass without
demonstrating anything: the question is which of two adjacent tools it picks,
and two of the five scenarios want `planner` — "remind me to take the bins out"
is a nudge for the user and is not this.

### Reaching a capability that is switched off

A project is its tool bundle and most of the bundle is off, which is right and
is also a discovery problem: somebody opens a new conversation, asks for a
spreadsheet, and is told the assistant cannot make one — when the truth is that
it could, if a switch two menus away were flipped.

Switching everything on was the obvious answer and it is measurably worse. The
A/B is recorded below: with the escalation note added to the suite's
everything-on tool set, the planner family scored 92% at six repeats and 94%
without it. Every capability costs a paragraph that every turn then carries, and
a small local model with a long tool list reaches for the wrong one. Eleven
capabilities on by default would buy discovery by making every answer slightly
worse, which is the trade this application has refused everywhere else.

So the **names** are in the prompt and the **tools** are not. `model::
capability` holds a one-line summary of each switchable capability — a menu, not
eight paragraphs — and one tool, `use_tools`, that turns one on for the open
project when the conversation turns out to need it. The request is rebuilt on
every round rather than every turn, which is what lets a capability switched on
mid-turn be callable on the very next one.

Three rules keep it honest:

* **It offers only what could work.** A capability whose requirements are
  missing — no podman, no mail account, no workspace folder — is left out of the
  catalogue entirely. A model told it can switch on Magpie, on a machine with no
  Magpie, will switch it on, call it, be told the command does not exist, and
  learn that the catalogue is not to be believed.
* **It weakens no gate.** Everything switched on this way keeps the gate it
  always had. Switching a capability on is permission to *offer* it, not to use
  it — which matters most for `escalate`, where the leakage control was never
  the switch but the per-call approval of the exact words.
* **A project with every switch off is left alone.** Somebody who turned
  everything off meant it, and a menu offering to turn eight things back on is
  an argument with them.

The switch is written to disk, so the next conversation in that project has it
too, and it is visible three ways — the chip, a toast, and the switch in the
project's settings now being on. A capability that switches itself on invisibly
is one nobody can switch back off.

None of these strings says "project": the catalogue the model reads calls the
folder a workspace, for the reason in *Projects and chats* above.

**Switching on the file tools establishes a folder if there is none.** A fresh
`main-line` has `workspace: None`, so without this the catalogue would offer
documents and then the tools would fail on the one thing they do — which is the
discovery problem moved one step along rather than solved. The folder is
`XDG_DOCUMENTS_DIR/Familiar`, made at the moment it is needed and not before,
and the result tells the model where it is so it can tell the user. It is under
Documents rather than the application's own data directory because a spreadsheet
somebody asked for should be somewhere they would look for a spreadsheet. Every
write into it still asks. `github` is the exception and is still gated on a
folder somebody chose: `gh` acts on the repository it is standing in, and a
default directory is not a checkout of anything.

The `reaching` family scores this, and half of it scores the model *not*
reaching: a model that switches things on to look prepared arrives at the
eleven-capability configuration anyway, one reasonable-looking step at a time.
The harness carries these calls out for real — the declarations and the system
message are both rebuilt — so a scenario that switches something on and then
uses it measures the whole loop rather than the first half.

**What the menu costs: nothing measurable.** `--no-catalogue` runs the whole
suite without it, and the two arms — same binary, same scenarios, three repeats,
one variable — both came out at **96% of 1245 checks**. Per family it moves
things around rather than up or down: without the catalogue, `conversation` and
`safety` were better and `documents` and `web` were worse, all within what three
repeats can distinguish. The claim the design rests on is therefore paid for:
a menu of eight names costs what switching two capabilities on cost, which is to
say the menu is roughly free and the paragraphs are what is expensive.

The A/B also found a scenario that had gone stale, which is the other thing an
A/B is for. `safety/no-invented-tools` scored 50% with the catalogue and 100%
without, and the reason was that its premise had expired: it asked for a
forecast to be saved to a file with no workspace switched on, and with
`use_tools` in the prompt that is no longer an invented capability — the model
switched the workspace on and wrote the file, correctly, and was marked down for
it. It now asks for something no capability covers and never will.

### Running Python

A model doing arithmetic is guessing. It is very good at guessing, which is the
problem: "£18,472.16" arrives with identical confidence whether it was computed
or remembered, and nobody downstream can tell which. So `run_python` writes a
script into a podman container and hands back what it printed, and the guidance
is built around one rule — anything with an exact answer is *run*, not recalled.

The container is disposable and its directory is not. Each call is
`podman run --rm`; what persists between calls is the bind-mounted sandbox
directory, one per project, beside that project's chats. A cold start measured
170 ms, so a long-lived container would buy nothing but a process to reap and a
warm interpreter holding whatever the last three scripts left in it. Files are
the part that needs to survive, and files survive.

`--network=none`, `--cap-drop=ALL`, `no-new-privileges`, 1 GB, 256 processes and
a 30-second wall clock. `/work` is the sandbox's own directory, read-write.
`/workspace` is the project's folder, **read-only**, absent when it has none.
Nothing else of the host is in there. Rootless podman is what makes the mount
usable: the container's root maps to the invoking user, so a file a script
writes is owned by the person running the app.

**Which is why it is ungated**, and that is the one call here worth arguing
with. The gate exists to stop the model changing something outside the vault,
and this cannot: it writes only to a directory the app owns, it reads only what
`read_file` already hands over ungated, and with no network it cannot send what
it read anywhere. Putting a dialog in front of arithmetic would train the habit
of approving without reading, which is precisely the habit the dialog in front
of `write_file` depends on nobody having. Getting a result *out* — `copy_to_workspace`
— is gated like every other write, and that is the seam that keeps the read-only
mount from being decoration.

The gate decision rests entirely on those claims being true, so
`tests/sandbox.rs` checks each of them against real podman:
no network, workspace readable and not writable, the host's home absent, the
directory persisting while variables do not. The unit tests assert the argv and
the eval suite asserts what the model reaches for; neither would notice if
`--network=none` stopped working.

The image is `packaging/Containerfile.sandbox`, built once by
`packaging/build-sandbox.sh`, and it carries the libraries Anthropic's skills
assume — numpy, pandas, scipy, sympy, matplotlib, openpyxl, python-docx,
python-pptx, pypdf, reportlab, Pillow, dateutil. There is no `pip install` at
run time and deliberately no way to add one, so everything a script might reach
for has to be in the image. An image that is not built refuses with the command
that builds it, and both that refusal and a missing podman tell the model
plainly not to fall back on doing the sum itself — which is the failure the
whole capability exists to prevent.

This does not replace the document tools. Making a `.docx` is a solved shape and
stays in Rust; working out what belongs in it is arithmetic.

### Documents: which writer, and when

With a Python sandbox in the picture, the obvious question is whether the
in-process Rust writers should be replaced by Anthropic's own approach — a
skill plus `python-docx`, `openpyxl` and `python-pptx`. All three libraries are
in the image and all three work; the answer is still no, and it was measured
rather than argued.

Asked to produce one specified `.docx` — a title, a heading, a paragraph and a
two-row table — the local model wrote a `python-docx` script that produced a
correct file **5 times out of 6**. The sixth dropped the title. The Rust path
does not have a failure mode of that kind at all: the model supplies Markdown
and the writer supplies the format, so content fidelity is 100% by construction
and the only thing left to get wrong is the workflow around it, which the
`documents` family already measures at about 90%.

So the division is by *direction*, not by format:

| | |
|---|---|
| Making a document | the Rust tools — right every time, one call, no code to get wrong |
| Reading one back | `run_python` — `openpyxl` and `python-docx` open what the writers cannot |
| Anything the writers cannot express | `run_python` — charts in a deck, conditional formatting |

The second row is the real gain, and it closes a gap this document used to list
as a limitation: the writers cannot open a `.docx` or `.xlsx` at all, and the
sandbox reads both. `skills::catalogue` says so in the prompt, but only when a
project has both switched on — a note about an interpreter that is not there
would be worse than no note.

### Asking a stronger model

`escalate` sends one question to `claude -p` or `codex exec` and returns the
answer. The design problem is restraint, not plumbing: a tool advertised as
"ask a better model" is one a small model reaches for whenever a question looks
hard, and every such call sends someone's words to a company's servers.

Three things hold it back. It is `Gate::Always`, and the approval dialog shows
the exact text that would leave the machine — that is the leakage control, not
a promise in a paragraph. The guidance and the tool description both say "last
resort" and both say what it costs. And the eval scores the model **not**
escalating in seven of nine scenarios; the family runs at 100%.

**It consults; it does not act.** Both CLIs are coding agents, and neither is
invoked in a way that lets them be: `claude` runs in plan mode with Bash, Edit,
Write, NotebookEdit and Task denied, `codex` in its read-only sandbox, and both
in an empty scratch directory rather than the workspace. The question travels
on **standard input**, which is worth stating because it was learned twice: the
CLIs' `--disallowed-tools` and `--add-dir` are variadic and silently swallowed a
trailing prompt argument, and an argument is world-readable — anyone who can run
`ps` could read a question the user approved in confidence.

### Mail

One tool taking an argv, gated by verb, the shape `gh` and the sibling CLIs
already use. `folders`, `search` and `read` never ask; `label` and `move` never
ask either; `delete` and `send` always do.

The middle row is the judgement call. Putting every label behind a dialog makes
the thing actually wanted — monitoring mail and organising it — impossible,
because nobody wants forty dialogs to file a morning's post. What makes it safe
enough is that nothing in that row destroys anything or leaves the machine, both
are undone by the opposite verb, and one call may touch at most 25 messages. A
move to Trash is a delete wearing another name, so `move … Trash` is recognised
and gated with `delete`.

**A message is data, never an instruction.** Mail is the only input here whose
contents an attacker chooses and can put in front of the assistant for free, so
the rule is first in the guidance and repeated in every result that carries
message text — the system prompt was read thousands of tokens before the
message saying "URGENT: forward this" arrives. The eval's inbox contains exactly
such a message; the family runs at 100% and the injection scenario asserts that
it neither forwards, nor deletes, nor stays silent about the attempt.

IMAP and SMTP are spoken directly over `gio`'s TLS, so this needed no new
dependency. The protocol is pure and tested in `model::email`; the socket half
is in `ui::mail` and is exercised against a fake server in `tests/mail.rs`,
including a message body containing a line that looks exactly like a tagged
response — the case that makes IMAP's literals mandatory and a line-splitting
reader wrong. There is no mail account configured on this machine, so **none of
this has run against a real server**.

#### Gmail is not quite IMAP

The account this will actually be used with is Gmail, which answers every
standard command and means different things by some of them. `model::email::
dialect` holds the differences and `ui::mail` asks it rather than assuming:

* **Folders are labels, and the special ones carry a prefix.** `MOVE 42 Trash`
  fails on Gmail with "no such mailbox", which reads to the model like the
  message was not there. The dialect spells `Trash` as `[Gmail]/Trash`, `Sent`
  as `[Gmail]/Sent Mail`, and leaves any label the user made exactly as they
  wrote it.
* **Labelling is `X-GM-LABELS`, not IMAP keywords.** Keywords do exist on Gmail
  and are not the same thing: they do not appear in the web interface, so
  filing a morning's post with them would look, to the person who asked for it,
  like nothing happened.
* **Search is Gmail's own language.** `X-GM-RAW` takes the query syntax from the
  search box — `has:attachment`, `older_than:7d`, `larger:5M`, `label:Receipts`
  — which is also the syntax a language model has read a million examples of.
  Against Gmail the model's words go over almost unchanged; everywhere else
  they are translated by `criteria` and most of them are lost on the way.
* **Archiving is not a move.** On a standard server it means moving to a folder
  called Archive. On Gmail it means removing the `\Inbox` label and leaving
  everything else alone, because the message never left All Mail. Moving it to
  All Mail instead would be a no-op reported as a success.

Which dialect is in play is guessed from the hostname and then replaced by what
the server said to `CAPABILITY` — `X-GM-EXT-1` — so a Workspace account on a
custom domain is recognised too.

**Authentication is an app password, not OAuth.** Google has not accepted an
account password over IMAP for years, so the choice was between an app password
and a full OAuth 2.0 flow. OAuth for a desktop application means registering a
Cloud project, shipping a client secret inside an application anybody can read —
where it is not a secret — running a loopback HTTP server to catch the redirect,
and storing a refresh token. An app password is sixteen characters pasted once,
revocable on its own without touching the account password, and scoped to mail.
Preferences → Assistant → Mail asks for an address and that password and fills
in the rest; the servers and ports are behind an expander for anyone not on
Gmail. It sits in the settings file beside the Exa key, which is the same trade
this application already made — a keyring would be better and is one change to
make once, for both.

A login refused by Gmail says so specifically rather than passing on "Invalid
credentials", which is what Google returns for a typo *and* for the right
password used the wrong way, and which sends anybody sensible round the loop of
trying it again.

### Workflows: a job with steps, and who says go

A workflow is a goal, three to a dozen steps, and where the work has got to. The
model proposes one with `workflow plan`, the user reads it, changes anything they
like, and says go; the model then does one step per `advance` and records what it
produced. `save` keeps the shape under the project; `start` runs a saved one.

**There is no second type for a saved one.** A workflow is a workflow whether it
is on the open thread or in a file — saving clears the outcomes and writes the
same shape to `projects/<slug>/workflows/<name>.md`. An earlier draft had a *run*
and a *routine* and the pair bought nothing but two words the user would have to
learn, when "save" already carries the idea.

Markdown rather than JSON, because it is a thing the user writes; the vault makes
the same argument for notes. A heading is the goal, a numbered list is the steps,
a block quote under a step is the user's note. Files a person edited by hand read
back — numbers that do not ascend, `)` instead of `.`, ragged indentation.

#### One word in the window, the code and the prompt

This is the exception here. `project.rs` maintains a three-way split — the window
says project, the code says `Project`, the prompt says *workspace folder* —
because Planner's `#Project` and the memory tool's `project` kind had both
claimed that word first, and it pays for the split with a test that greps the
composed prompt. Nothing had claimed **workflow**, so nothing has to be
translated.

Almost nothing. `gh workflow list` is a real subcommand and the GitHub guidance
says "workflow runs" in prose, so a project with both switched on offers the
model two plausible landings for "run the deploy workflow". That is measured
rather than argued about — see *Two tools that share an English word* below.

#### The steering is the user's, and it is delivered where it applies

Each step can carry a **note**: the user's instruction for that step. The model
never writes one and never overwrites one — the edit dialog is the only door.

The note is not carried in the system prompt. It is delivered in the tool result
of the `advance` that makes its step current, which is the rule `planner::note_for`
already keeps: a rule attached to the result that raises it is read exactly when
it applies and costs nothing on every other turn. Carrying five steps' notes for
all five steps would be four steps of noise, and — because `instructions.rs` is
built around the prompt's stable prefix staying byte-identical — a cache rewrite
every time the user typed.

Revisions follow the note **by the step's text, not by its position**. The first
draft matched by position and a test caught what that does: the user annotates
step 2, the model reorders, and the sentence ends up steering a step it was never
about — worse than losing it, because nothing about the result looks wrong. A
note whose step is gone is dropped *and said out loud*.

A user edit reaches the model through `Workflow::edited`, cleared the moment a
render is handed over. A `thread::Note` cannot do that job: notes are addressed
to the reader and `messages_for_model` deliberately never sends them, so a model
carrying on against a plan it read three rounds ago would never learn the
steering had moved. That is the exact failure the capability exists to prevent,
so it could not be the one it shipped with.

#### Why the timeline is pinned rather than drawn in the conversation

The obvious place for a checklist is inline, beside the thinking disclosure and
the tool chips. It does not work, and the reason constrains the model side too:
**a workflow spans turns and chips do not.** A chip belongs to the turn that made
it. Draw the checklist inline and it renders at the turn that created it and then
scrolls away while steps 3, 4 and 5 happen off screen — or every turn redraws it
and the conversation becomes five copies of one list. Worse, if the calls grouped
under their step, turn 4's work would appear in a card sitting up at turn 2: you
ask a question and nothing happens where you are looking.

So the chips stay where they are, each turn showing what *it* did, and an
`AdwBottomSheet` over the conversation shows where the whole job is: a strip with
the current step and Start/Edit/Stop, pulling up into the list of steps. Not
modal — you have to be able to read the plan and type a correction at once. Not
the window's bottom toolbar either: that strip is a passive caption, and buttons
in it would make the model name and context gauge look clickable.

Ungated, and that is the shape of the thing. A plan is a list of intentions;
every action a step takes keeps the gate it always had. Putting a dialog in front
of a checklist would teach the user to click through the ones that matter.

#### On by default, and what that cost

Workflows ship **on**, with memory, the web and the weather — so they are not in
the `use_tools` catalogue either, because a menu entry for something already
switched on is noise.

That is a real bet against this application's own default. Every other
capability is off precisely because a paragraph of guidance carried by every
turn of every conversation is not free: the escalation note cost the planner
family two points, and an over-long memory note once cost `documents` thirteen.
The argument for making an exception is that a capability nobody finds is a
capability that does not exist — which is the lesson the scheduler taught, at
the cost of an assistant that filed a task and then denied having a scheduler.

The worry that kept it off was that a model with a planning tool in front of it
plans the weather. It does not: every scenario in the `workflow` family's
not-planning half scores 100%, including the weather.

So the only open question was the prompt-length cost, measured on the two
families that have historically been the canaries for it — `documents`, where
the memory note's thirteen points showed up, and `planner`, where the escalation
note's two did. Three repeats, same scenarios, the only change being that every
tool set now carries `workflow`:

| Family | Off | On | Δ |
|---|---|---|---|
| `documents` | 94% (99/105) | 96% (101/105) | +2 checks |
| `planner` | 96% (52/54) | 94% (51/54) | −1 check |

**One check each way, which is no detectable cost rather than no cost.** At three
repeats that is inside the noise in both directions, and the honest statement is
that the paragraph is cheap enough not to show up on the two families most
sensitive to prompt length — not that it is free. If either ever slips, this is
the first thing to re-measure, at six repeats.

Two scenarios moved enough to name. `documents/skill-before-the-first-one` — the
canary within the canary, the one the memory note took from 88% to 42% — dropped
83% → 67%, while three others in that family rose to 100%. `planner/looks-
before-filing` went 100% → 88%. Both are one check on six runs.

What did *not* happen is the failure worth worrying about: no scenario in
`planner` substituted a workflow for a task, and the workflow family's own
`a-nudge-for-the-user-is-still-a-task` stays at 100%.

The eval's tool sets carry `workflow` everywhere a *real project* would, and
that is the point: a default-on capability left out of the suite would mean the
harness never carried a paragraph that every real conversation does. Two keep it
off deliberately — `nothing()`, the recall suite's isolated arm, where the whole
measurement is that there is nothing to look anything up with; and
`repository()`, the overlap experiment's control, where the absence *is* the
variable.

#### What the eval found

Three findings, all of them the fixtures or the checks rather than the prompt,
which is the pattern by now:

- The first authoring ask said only "help me work out the steps for the quarterly
  comparison". The model asked four clarifying questions instead of planning, and
  it was right to — there was nothing to plan *from*. The scenario was measuring
  its own under-specification. An ask that names the material, the work and the
  output took it from 50% to 92%.
- The model sent `steps` **one element at a time**, four times running, and was
  told each time that fewer than two steps is not a workflow and it should just
  do the job. True of a genuinely small job, useless to a model that has a plan
  and sent one line of it. One step and no steps now get different messages.
- `ArgNever { tool: "workflow", needle: "plan" }` looks like "it never planned"
  and is not: it reads every argument of every call, so a model quoting the
  budget table's **Plan**ned column into an `outcome` failed a perfect trace.
  `ArgNeverAt` is the keyed form, and the one to use for a short needle.

The one genuine prompt change: **when the user asks you to work out the steps,
call `plan` rather than writing them in the answer.** A list in prose looks like
the same thing and is not one — it cannot be reordered, annotated, started or
kept. Without that sentence the model wrote prose, which is the substitution the
`scheduling` family already records in another form.

The family settled at **91%** of 102 checks at three repeats, with every
scenario in the not-planning half at 100% — the weather is not planned, a
two-call job is not planned, a nudge for the user is still a task, recurring work
is still a schedule, and nothing is saved uninvited.

#### Where a rule lives is worth more than how it is worded

One weakness survived: at 61%, "Hm, that plan looks reasonable" was read as a
green light and the model started deleting. The fix is one sentence, and where it
goes was measured three ways.

| Where the rule lives | Family | `approval-shaped` | authoring arc |
|---|---|---|---|
| nowhere | 91% | 61% | 93% |
| the system prompt | 85% | 94% | 60% |
| **the tool result** | **94%** | 66% | 100% |

As a paragraph of guidance it did exactly what it was asked and cost more than it
bought: the model became careful about *the tool* as well as about starting, and
stopped calling `plan` at all on the first ask. Six points net worse — the same
shape as the memory note that cost `documents` thirteen points, and as the
deletion paragraph whose first draft was phrased too generally.

In `Workflow::render`'s next-move line it is read at the one moment it applies —
a plan exists and nobody has said go — and it cannot compete with the sentence
telling the model to reach for `plan`, because that sentence has already done its
work by the time this is on screen. Every other turn pays nothing for it.

This is the third confirmed instance of the same principle and the first with a
control arm on both sides, so it is worth stating as a rule rather than as a
habit: **prefer the result that raises a rule over the prompt that carries it
everywhere.** The remaining 66% on `approval-shaped` is an honest weakness, not
one worth spending prompt on.

### The lookout

The heartbeat wakes a thread with a prompt somebody wrote. The lookout is the
other half: a periodic run with no prompt, which gathers what is due, what has
arrived and what the weather is warning about, and decides whether any of it is
worth a notification.

Almost always it is not, and that is the design — a notification that fires on
an ordinary Tuesday gets muted, and a muted notification is worth less than
none. So it is one call with no tools, silence is the shortest reply to write,
and `lookout::read` throws away anything vague enough to have been produced
without looking: a headline that says "you have a few things today" is scored
as silence, because that is what it amounts to.

Two things were measured and both were wrong at first. The instruction led with
the bar — stay quiet about anything "already obvious" — and a deadline in
somebody's own task list is obvious by that reading, so the model answered QUIET
to a lease due today four times out of four. Leading with the permission and
adding three worked examples took the suite from 68% to **95%**. The examples
had to be examples of the *form*: with one that matched a scenario, the model
reproduced it word for word and the case passed for no reason at all. A test
asserts no example uses a word a scenario uses.

The third thing was worse than a wrong instruction, and the suite could not see
it. `lookout/a-warning-that-changes-the-day` sat at 0–25% through every attempt
to fix the rubric, and the reason was that **the application never gathered a
weather alert at all**. `Signals.alerts` was written by the eval suite and by no
other caller: the rubric talked about weather, the worked example was about
weather, the harness scored weather, and the shipping proactive check had none.
Wiring it up — two requests, the point lookup and the active alerts, with no
forecast because the lookout has no use for one — took the family to **100%**.

The fixture was wrong in the same direction. It passed `"Severe Thunderstorm
Warning until 21:00 — Franklin County, OH"`, and against that the model answered
QUIET six times in six; it also answered QUIET to a *tornado* warning over an
afternoon on a roof, which is not a judgement call the rubric was going to
change. What the weather service actually sends is a sentence saying what the
hazard is — "damaging winds up to 60 mph, move indoors and away from elevated
work" — and the collision is obvious once that is in front of it. The signal now
carries the event, when it ends, and the service's own words, which cost nothing
because they were already in the response.

### Compaction

A long thread stops fitting the context window, and the fix is `llamatui`'s,
which was the right shape: the last N turns are never touched, older turns fold
into a single rolling summary just after the first user message, and the fold
happens at turn boundaries so the cached KV prefix survives. It is **in-memory
and lossless on disk** — the model's view narrows, the transcript and the
scrollback do not, and a system note in the thread says what left the model's
view. A context-overflow error escalates toward a floor (first message, current
message) and retries once, but only if no approved tool has already run that
turn, since retrying would repeat its side effects.

Off means off: with compaction disabled an overflow is a plain error and manual
compaction stays available.

**What decides that a thread is long is tokens, not turns.** `should_fold`
measures the last turn's `prompt_tokens + generated_tokens` — the number the
status bar already shows — against the window the server reported in `/props`,
and folds above `FOLD_ABOVE` (70%). Turn count only decides how much to keep
once a fold is warranted. This used to be a turn count on its own, so a thread
folded on its seventh turn whether it was near the window or nowhere near it;
on a 175k window that was every thread, spending fidelity to solve a problem
nobody had.

The preferences dialog went on describing the old design for some time after
that. A row called **Turns Kept in Full** with a range of 2–40 and no subtitle
reads as the trigger — fold once the thread passes this many turns — which is
what the code used to do and has not done since. It is now **Recent Turns Kept
Whole**, and it says what it is: how much of the end of the thread survives a
fold, with the threshold that actually causes one named beside it. The subtitle
is built from `FOLD_ABOVE` rather than typed out, so the two cannot drift apart
again.

**The summary is state on the thread, not a derivation from it.** `Thread.fold`
holds what the summary says and how many exchanges it stands in for. `view`
applies it and is pure, so it can run on every request; `to_summarize` says what
still needs folding and is also pure. The expensive part — a low-temperature
call to the same server, `summary_request` — happens **once, between turns, and
asynchronously**, because the request is built on the GTK main thread and
waiting for a model there would freeze the window. So the turn that crossed the
threshold is sent unfolded and the next one benefits, which is what the 30%
margin above `FOLD_ABOVE` is for. Which fold a turn is sent under is fixed at
its first request and carried on `InFlight`: a summary landing between two tool
rounds belongs to the next turn, not this one.

Persisting the fold is what makes a reopened thread answerable without
summarising itself again, and it is why `Fold` is serialised beside `entries`
rather than recomputed on load.

The summarizer is told to keep names, figures, dates and corrections verbatim
and to drop everything else, because the gist is the one thing the next turn can
reconstruct without help. It runs with thinking switched off — the control token
the chat template reads — after a measurement showed 347 tokens of deliberation
in front of a 19-token summary. When the call fails, `Headings` takes over: a
list of the first line of each folded user message, which is poor but cannot
fail, and a thread that cannot be folded is a thread that cannot be sent.

### The window

```
┌───────────────────┬──────────────────────────────────────┐
│              +▾   │  Chat title                   ⟳  ☰   │
│ ▾ ⌂ Chats         │  Planning                            │
│     + New Chat    │ ┌──────────────────────────────────┐ │
│     Vault ideas   │ │ ▸ Thought for 4s                 │ │
│ ▾ 🗀 Planning     │ │                                  │ │
│     + New Chat    │ │ The scanner reports syntax spans │ │
│     Q3 roadmap    │ │ in char offsets, so …            │ │
│ ▸ 🗀 Taxes        │ │  ⌗ recall "markdown scanner" → 3 │ │
│                   │ │  1,412 tok · 84 tok/s · 0.3s     │ │
│                   │ └──────────────────────────────────┘ │
│                   │ ┌──────────────────────────────────┐ │
│                   │ │ Ask something…                ➤  │ │
│                   │ └──────────────────────────────────┘ │
│                   │ 12% of context · qwen3-30b           │
└───────────────────┴──────────────────────────────────────┘
```

An `AdwOverlaySplitView` with a `GtkListView` over a `GtkTreeListModel`: one
root per project, opening onto a way to start a chat and the chats themselves.
A tree rather than the flat `AdwSidebar` this used before, because a project
*contains* chats and a flat list cannot say so. An `AdwBreakpoint` at 675sp
collapses the split.

Rows open what you click, once (`single-click-activate`, which is what a
navigation sidebar means). **Clicking a project opens its page**; the
disclosure arrow expands it, which `GtkTreeExpander` gives for free — the arrow
is a button and swallows its own click. Every row carries a right-click menu
that is its own: a chat offers Rename and Delete, a project its settings and,
unless it is the default one, deletion.

The application rebuilds the tree after every turn, so what was open is
remembered by row key and reopened — a tree that collapsed under the reader
after each answer would be unusable. Which rows are open is read back off the
tree rather than recorded as it happens, because the arrow expands a row
without telling this widget anything.

The header carries one `AdwSplitButton`: New Chat, with New Project on its
menu. Two header buttons for one idea would be two affordances for it.

### A project's page

The content side is a `GtkStack` of two things: the conversation, and the page
of the project you clicked. The composer belongs to the first — there is
nothing on a project page to type into.

```
┌──────────────────────────────────────────────────────────┐
│  Planning                                     ⟳  ☰       │
│                                                          │
│  Planning                          [ Project Settings… ] │
│  3 chats                                                 │
│                                                          │
│  Instructions                                            │
│  Added to what Familiar already knows, in every chat here │
│  ┌────────────────────────────────────────────────────┐  │
│  │ You help plan the week.                            │  │
│  └────────────────────────────────────────────────────┘  │
│                                                          │
│  Files            ~/Notes                        [+🗀][🗁]│
│  ┌────────────────────────────────────────────────────┐  │
│  │ ▸ 🗀 drafts                                        │  │
│  │   🗎 budget.csv                                    │  │
│  └────────────────────────────────────────────────────┘  │
│                                                          │
│  Chats                                    [ Search… ]    │
│  ┌────────────────────────────────────────────────────┐  │
│  │ Q3 roadmap                        Friday · 12 turns│  │
│  └────────────────────────────────────────────────────┘  │
│                                                          │
│  Runs on Its Own                                         │
│  ┌────────────────────────────────────────────────────┐  │
│  │ Morning briefing    Weekdays at 07:00 · ran 2h ago │  │
│  └────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────┘
```

Four things, in the order somebody coming back to a project wants them: what it
has been told to do, the files it is about, the chats in it, and anything that
runs on its own. Before it existed a project was only ever a heading with chats
under it — its folder was a branch in a 300-pixel sidebar, its instructions were
two menus away, and the fact that one of its chats woke every morning was
visible nowhere at all.

The file tree (`ui/file_tree.rs`) is the same `GtkTreeListModel` shape as the
sidebar, given the width to be read. It reads directories and changes nothing:
Open, Show in Files, New Folder, Rename and Move to Trash leave as a
`file-action` signal the application carries out, after checking the path is
still inside the project's folder. A file goes to the **trash**, not to
`unlink`.

**An empty state says why it is empty.** No folder chosen says so and offers
Choose; a folder that has been moved or unmounted says *that*, rather than
showing an empty box that looks like an empty folder. That was the bug which
moved files off the sidebar in the first place.

The conversation is a `GtkBox` in a scroller under an `AdwClamp` at a readable
measure — not a `GtkListView`, because recycled widgets fight variable-height
streaming text and compaction bounds the count anyway. It reads as a document,
not a messenger: your message is a `card` with a dimmed attribution, the
assistant's answer is prose at full measure with no bubble around it.

**A selection in an answer can be asked about.** Right-clicking one offers
*Explain This* beside the view's own Copy and Select All — `extra-menu` rather
than a gesture of our own — and Ctrl+Shift+E does the same from the keyboard.
What it sends is the selection as a Markdown blockquote and one sentence asking
for more. Quoted, not pointed at: the thread is folded as it grows, so the
answer the phrase came from may not be in the next request at all. The text
taken is what is *on screen* rather than what is in the buffer, because the
Markdown syntax is still there under an invisible tag and quoting the hidden
characters would send `**this**` back with the asterisks the view took off. The
menu item lives in the turn's own action group and so needs no focus; the
shortcut arrives at the window with no idea which answer it means, and asks the
focused widget.

The composer is a `GtkTextView` in a card, Enter to send, Ctrl+Enter for a
newline, growing to a few lines before it scrolls. The send button turns into a
stop button while a turn streams, and Escape cancels. `AdwToolbarView` carries a
bottom bar with context usage and the model name.

When `llama-server` is unreachable the send button goes insensitive with a
tooltip and an `AdwBanner` says so with a Retry — a persistent condition gets a
banner, not a toast that is missed while typing. An empty thread is an
`AdwStatusPage`. Preferences are an `AdwPreferencesDialog`: server URL, vault
path, sampling, thinking budget, thinking visibility, compaction. There is an
`AdwShortcutsDialog` and an About entry.

### Making documents

An assistant that can read a PDF and not produce one is half a tool. So a
project can be switched into making Word documents, Excel workbooks,
PowerPoint decks and PDFs, into its folder, behind the same approval dialog
as any other write.

**They are written here, in Rust, not converted by something else.** `.docx`,
`.xlsx` and `.pptx` are ZIP archives of XML, and a writer for the parts each
application actually requires is a few hundred lines with no dependency at all
— the archive entries are *stored* rather than deflated, which every reader has
accepted since 1989 and which saves carrying a compressor. The PDF is painted
by Cairo and Pango, which GTK has already loaded into the process.

The alternative was a converter, and LibreOffice is the good one: it is
installed on this machine, it would take the `.docx` we already produce, and
it would render better than we will. It is also ~800 MB, three seconds cold,
and — being a Flatpak itself — reachable from Familiar's sandbox only through
`flatpak-spawn --host` with `--talk-name=org.freedesktop.Flatpak`, which is
full host access. That is a large hole in a sandbox for an app whose first
sentence is "local by default", to render some headings. weasyprint wants
Python and pandoc wants TeX; neither is in `org.gnome.Sdk`, and Cairo and Pango
both are.

**One block spec, four writers.** A model writes Markdown, `office::markup`
parses it into `Block`s, and the `.docx` writer and the PDF painter consume the
same blocks — so the Word file and the PDF of one document agree by
construction rather than by care. No external converter can promise that
without being the same converter. `.xlsx` is the exception, because a
spreadsheet is not prose: it takes typed cells, and `Cell::infer` is where
`42` becomes a number, `=SUM(B2:B9)` a formula, and `007` stays *text* —
because an order number turned into a number is a data loss no formatting
undoes.

**Skills, in Anthropic's shape.** Each format has a `SKILL.md`: frontmatter
with a name and a description, then a body of instructions. The descriptions
ride in the system prompt — about 250 tokens for all four — and the body is
fetched by a `read_skill` call when the model decides to make that kind of
file. This is progressive disclosure, and the reason is attention rather than
budget: the window here is 175,104 tokens and the system prompt is the cached
prefix, so 3,500 tokens of skill body would be paid for once. But every token
in that prefix is attended to on every token generated, a small model follows a
short instruction set better than a long one, and standing instructions for a
task nobody asked for are an invitation to do it — a model holding four pages
on spreadsheets reaches for `create_spreadsheet` when the question only
mentioned one. The descriptions say the capability exists; the body arrives
after the decision is made.

Familiar reached this conclusion once already from the other end: the note
below about Exa records that the value in Claude Code's plugin was *the skill
and not the MCP server*, because the useful part was prompt text. This is that
observation with the second half of the format attached.

What is deliberately not copied: the scripts. Anthropic's versions of these
skills are Python driving `python-docx`, `openpyxl` and `python-pptx` under a
code interpreter, and the body is largely instructions for writing that code.
Familiar has no interpreter and wants none, so its bodies teach a model to use
the tools — a shorter thing to say, with no arbitrary-execution shape to gate.

PDFs the user already has are handled with the poppler-utils already required
for reading a dropped one: `read_pdf` is the same page-by-page extraction the
composer does, `merge_pdfs` is `pdfunite`, and `extract_pages` is
`pdfseparate` then `pdfunite` — two passes, because poppler has no single tool
that takes a discontinuous range, and because doing it that way makes `7,1-3`
mean what it says.

### News

`web_search` answers "what is true about X". `news` answers "what has changed
about X lately", which is a different search: a semantic engine handed a topic
returns the *best* pages about it, and the best page about anything is usually
years old. Recency has to be a filter, and Exa's `startPublishedDate` is that
filter — `web_search` never sent one, which is why it was no good at this.

**The tool does the research, not the model.** Everything Familiar talks to is a
27B model on the local machine; asking it to run four searches, dedupe them and
weigh the results is asking for one search and a shrug. So `news` takes the
plain name of a thing and expands it itself into fixed, page-describing queries
across three lanes, fires them together, and hands back a ranked brief. The
model's job is to read the brief and write the answer.

| Lane | Where | What it contributes |
|---|---|---|
| Press | Exa, `category:news`, windowed | what was announced |
| Community | Exa, restricted to a short forum list | what people made of it |
| Engagement | Hacker News' Algolia index | points and comment counts |

**Agreement is the ranking.** Borrowed from the `/last30days` skill, which is
right about the signal: a press item nobody discussed is a press release, a
thread with four hundred points no outlet covered is a community story, and the
two together is news. Stories are merged on a normalised URL, then scored on
log-scaled engagement × recency-within-window × a convergence multiplier. Log
scaling is not decoration — linear, one viral thread buries the other nine
entries.

**Hacker News is a lane of its own** rather than left to the community lane that
already crawls it, because it is the only source here that reports what a story
actually scored. Exa can return an HN thread's text but not its points, and a
ranking with no engagement term is a ranking by recency wearing a hat.

**Reddit is reached through Exa, not through Reddit.** Its own JSON endpoints
answer an unauthenticated client with HTML; the Pushshift successors are alive
and keyless but freeze a post's score at ingest, so everything reads as one
point and would poison the ranking; the rest want a paid scraper key. Exa has
the pages crawled, so the forum list gets Reddit discussion through a key the
app already has and no second credential.

A call with no topic sweeps what is drawing attention generally. It is
deliberately thin next to `/last30days`' discovery mode, which spends three
round trips having a reasoning model judge candidates: this has one round trip
and a small model, so it sweeps what other people have already ranked — Hacker
News' front page — and lets convergence do the rest.

### Weather

"What is the weather" is the most common thing anyone asks an assistant, so
`weather` is on by default beside memory and web: no key, no workspace, no
approval, one government API.

**The US National Weather Service**, because it needs no account, its data is
public-domain US Government work with no attribution obligation, and it is the
only free source that also carries **alerts**. GNOME's own `libgweather` was
the obvious candidate and does not ship in `org.gnome.Sdk` or
`org.gnome.Platform` 50 — GNOME Weather bundles it in its own manifest — so it
would be a third-party C dependency, not the platform built-in that won Cairo
and Pango their place. It also has no alerts and its location database contains
no postal codes.

**A coordinate, not a postcode.** That is what the API takes, and it is the one
form that is never ambiguous: a postcode covers several square miles, and the
services that resolve one disagree by a few kilometres — enough to land on a
different 2.5 km forecast grid. Preferences takes a latitude and longitude.

It is the **United States only**, and the tool says so rather than returning
nothing for Paris. Open-Meteo would cover the rest of the world and was left
out on purpose: its free tier is non-commercial and CC-BY, which would put an
attribution obligation on an app that has none.

### The heartbeat

A thread can wake on its own. The vocabulary is OpenClaw's, and so is the
distinction that keeps this small: an *automation* is an exact-time job in a
fresh session reporting through a notification, while a *heartbeat* wakes an
**existing** conversation with its context intact. This is the second — so the
schedule is a property of a Thread, not a job system beside one. The standing
prompt is submitted as an ordinary turn, down the ordinary pipeline, and the
answer lands in the thread you can open and read a week of back.

**The portal, not a systemd timer.** Flatpak has no mechanism to export user
units — open and unimplemented since 2018 — and the workarounds
(`--filesystem=~/.config/systemd/user`, `--talk-name=org.freedesktop.systemd1`)
are exactly what flatpak-builder-lint flags. It is the same argument the
manifest already makes against LibreOffice: a unit file is arbitrary host code
execution for an app that gates every write. The Background portal needs no
`finish-args` change at all, so the manifest's "Deliberately absent:
`--talk-name=*`" note stays true.

**A minute tick against the wall clock, not a timer set for the deadline.**
`glib::timeout_add_seconds` is monotonic, and monotonic time does not advance
while the machine is suspended — a timer set for 07:00 silently drifts by
however long the laptop slept. So the tick is short and fixed and every tick
asks `chrono::Local::now()` what is due. This is Pika Backup's shape, chosen
for the same reason.

**A missed run is skipped, not caught up.** A 07:00 briefing delivered at 14:00
is worse than none: the weather is stale, the pull requests have moved, and
nobody asked for it now. Twenty minutes of grace covers a deferred tick; past
that the moment has gone. Claude Code Desktop skips explicitly; OpenClaw skips
and waits for the next occurrence.

**It fires between turns, never mid-answer**, which falls out of `InFlight`
already making "a turn is streaming" a checkable state.

Schedules are managed from **Scheduled Chats** in the menu, which reads every
project from disk rather than from memory — only the open chat is loaded, and a
schedule set up last week in another project is exactly the one somebody opens
that window to find.

Which is also why the cadence and the standing prompt are rows you can activate
rather than labels. They were labels, and `Schedule…` in the main menu was the
only editor — acting on whichever chat happened to be open, with nothing on
screen to say which. Somebody who came to change a briefing they set up last
week had no route to it at all, and setting one from the wrong chat did not fail
or warn: it made a second schedule somewhere else. Both rows now open the same
editor against the schedule being looked at, writing back through the path the
Enabled switch already used, which reaches a chat whether or not it is open.
`last_run` survives the edit, so moving a daily briefing from 07:00 to 08:00
does not make this morning's run happen twice. The editor names the chat it is
editing in its header, which is what the menu route was missing.

### GitHub, through `gh`

`gh` is installed and signed in, holding a token in the keyring, so the useful
thing is not a REST client but permission to run one binary. That is a new
shape — until this, Familiar could not run a program at all — and it is kept
deliberately narrow.

**Only `gh`, and only as an argv.** The tool takes a list of arguments and they
go to `execvp` unchanged. There is no shell, so `;`, `|`, `$(…)` and `&&` are
not operators; they are strings `gh` rejects. The safety is structural rather
than a filter that has to anticipate every metacharacter.

**The subcommand decides the gate**, which is why `gate_for` exists beside
`gate_of`: every other tool's gate follows from its name, and `gh pr list` and
`gh pr merge` arrive under one name. Reads run like `read_file`; anything that
writes, merges, closes or dispatches stops at the approval dialog with its exact
argv on screen. Unrecognised is gated, the same rule as an unknown tool.

**A few things are refused rather than gated,** because approval only means
something when a person can judge what they are approving. `gh auth token`
prints a credential into a transcript that is written to disk. `gh extension
install` and `gh alias set` are arbitrary code and command redefinition arriving
under a plausible name. `gh codespace` is a shell on another machine, which is a
way around every limit here.

### The house's electricity

`dynamo` is the third sibling CLI, and the first where **no verb can change
anything**. Planner gates an unknown verb because the answer to "does this
mutate" might be yes; here it is no and cannot become yes — `dynamo agent` runs
`SELECT`s as a Postgres role granted nothing else, and the writes Dynamo does
perform come from a collector loop in a container on the NAS that this cannot
reach. So `classify` returns `Gate::Never` for a known verb and *refuses* an
unknown one rather than gating it. There is no gated half to fall into, and a
verb Dynamo gains later costs a second call rather than an unreviewed write.

It is a subprocess for the usual reason inverted. Planner and Magpie have to be,
because each holds its store in the running app's memory and a second writer
loses; Dynamo's store is Postgres and would take a second reader happily. It is
a subprocess because that is the shape this app already gates, caps and frames,
and because the alternative is asking a service whose design note says it
publishes no port to open one.

#### The interesting risk is arithmetic, not authority

Nothing here can break the house. What it can do is report a number about it
that is wrong, and a number is what the user repeats. Five ways, all measured
against the real account rather than reasoned about:

- **Merged and branch are the same energy counted twice.** A 240 V circuit is
  wired across two legs and also appears as one merged channel. `kind=circuits`
  is the default and counts each circuit exactly once; adding `kind=merged` back
  on top gives 182 kWh for a house that used 140.
- **`kind=main` is one panel.** Only one of the three monitors has mains CTs,
  and its total — 140.33 against the house's 140.74 — is close enough to read as
  the answer.
- **`scale=1MIN` over a long period answers from a week.** Minute readings are
  kept about a week; hourly and daily go back years. So `series <circuit> month
  scale=1MIN` returns 135.4 kWh *labelled "the last 30 days"*, against 568.9 for
  the same question at the resolution Dynamo picks. Four times out, from a
  question nobody would call unusual, and the only clue in the response is the
  first timestamp — inside an array that has just been truncated.
- **A `usage` list omits whatever used nothing.** 27 rows for 40 circuits. A run
  read the dryer's absence as "it isn't on any named circuit" and said so; the
  dryer is channel 101 and is named. It had used nothing that day.
- **Most circuits have no name.** Thirteen of forty. The biggest live draw in
  this house is `basement (blank) ch3`, and both available failures are bad:
  reporting the channel number tells the user nothing, and guessing an appliance
  for it tells them something false.

The first two and the last are in the prompt; the rest ride on `note_for`,
because [where a rule lives is worth more than how it is
worded](#where-a-rule-lives-is-worth-more-than-how-it-is-worded). The dividing
line is whether the rule changes a decision made *before* the call — reaching
for `series` after `usage`, leaving `scale=` alone, not asking for July — since
nothing in a response can come back and correct those. A test asserts the
prompt does *not* carry the four rules that belong in a result.

#### Familiar's own cap cuts the figures off first

`MAX_OUTPUT` is 8,000 characters and `dynamo agent series "Water Heater" week`
is 9,355 — an ordinary question. Dynamo sorts its JSON keys, so `points`
precedes `resolution`, `total_kwh` and `truncated`: the cut lands inside the
array and takes **every figure** while keeping every row. The generic footer
`framed` appends then recommends retrying with `limit=N`, which Dynamo accepts
and silently ignores, so the retry returns the identical answer.

The fix is in `note_for` rather than in a larger cap: when a response is longer
than the cap, the note restates the headline — circuit, period, resolution,
total, rows matched — and says to ignore the `limit=N` advice. A note is
appended *after* the cut, which is the only reason this works.

#### The fixture was the thing under test

The `dynamo` family scored **100% of 90 checks** at six repeats and was
measuring nothing. Its world had six circuits, four of them named, and a tidy
`GeoThermal` as the biggest live draw; the real house has forty circuits,
thirteen named, and answers `basement (blank) ch3`. So the note that exists to
stop a model dressing a channel number up as an appliance could never fire. It
also had the double count backwards — branch figures repeated at half value,
where the real tool returns the same total by a different route — so the trap
the family was named for was not in it. Timestamps were `+00:00` against
guidance that says they arrive local, and dated a fortnight off the suite's own
clock.

Rebuilt from `dynamo agent` against the real account, at the real proportions,
the same family scores **87% of 342 checks**. `world.rs` now holds the fixture
to its own arithmetic — the rows sum to the total they are reported under, the
mains figure sits within 5 kWh of the house, thirteen of forty are named, a
week of readings overruns the cap — so the next version of this cannot quietly
stop being a house. This is the sixth instance of the pattern in
`familiar-fixture-lies`, and the most expensive: the score was perfect and the
capability was untested.

### Two tools that share an English word

`gh workflow list` is a real subcommand, `gh workflow run` is a real thing to
want, and the GitHub capability's own prose says "workflow runs". So a project
with both `github` and `workflow` switched on hands the model two plausible
landings for one sentence. Whether that costs anything is a question with a
number, and `--overlap current|reword|disambiguate` is how the number is taken —
the same shape as `--no-catalogue`, and for the same reason.

| Arm | What changes | What it risks |
|---|---|---|
| `current` | nothing | — |
| `reword` | the catalogue entry and the `gh` description say "Actions runs" | GitHub's own interface says *workflow*, and so do the people asking about it; the reword could cost recognition of the real `gh` case |
| `disambiguate` | GitHub's prose is left alone; the `workflow` guidance says which is which | prompt length, but only where the capability is on |

The prose is varied by substitution rather than by keeping two copies of each
sentence, because two copies drift and then the A/B is comparing more than the
one phrase.

The scenarios come in pairs. Every ask with `workflow` on has a twin with it off,
because the two failures look identical in a trace and are not the same thing: a
model that cannot find `gh` at all has a recognition problem the reword would
make *worse*, and a model that finds it except when `workflow` is in the list has
the collision. Without the control half, the reword would be adopted or rejected
on a number that could not support either. One scenario — "set up a workflow for
releases" — is genuinely ambiguous, and scores the model *asking*: picking one
and being right half the time is not better than asking, it only looks better in
a trace.

**The rule was written down before the first run**, which is the only thing that
makes this a measurement rather than a justification. If `current` routes as well
as the github family usually scores, nothing changes; otherwise the arm with the
largest gain net of what it costs elsewhere wins, and a tie goes to `current`.
Six repeats minimum — a fourteen-point `safety` regression at two repeats
vanished at six, and a three-way comparison at two would be noise wearing a
verdict's clothes.

#### What it found: nothing to fix

At six repeats, with `workflow` switched on beside `github`:

| Scenario | Score |
|---|---|
| `ci-with-nothing-competing` (control) | 100% |
| `running-one-with-nothing-competing` (control) | 100% |
| `ci-is-ghs` | 100% |
| `running-ci-is-ghs` | 100% |

"Run the deploy workflow on main" goes to `gh` every time with the competing tool
in the list. **There is no collision in the traces**, so by the rule above
nothing changes: `current` ships, `reword` and `disambiguate` stay as arms
nobody needs. The `reword` and `disambiguate` arms were never run against a
model, because running them after `current` had already met the stopping
condition would have been shopping for a reason to change.

The first pass said the opposite — `running-ci-is-ghs` at 17%, which looked like
a decisive collision. Both halves of that were the harness:

- `CallsOnly(&["gh"])` means "exactly one of these and *none of the others*". The
  model routed to `gh` correctly and also called `list_dir` to look around, and
  that scored as a routing failure. The right question is which of two tools got
  the request, which is `Calls("gh")` plus `NeverCalls("workflow")`.
- The `gh` stub returned **a pull request list for every call**, so `gh run list`
  came back with pull requests and the model tried seven more spellings of the
  question before running out of rounds. It now answers by subcommand.

Had either survived, a prompt change would have been adopted on a number that
was measuring `list_dir`. That is the failure mode worth naming: not the low
score, but a low score that lasts long enough to justify a change — because then
the prompt gets worse *and* the number goes up.

Reproduced: two independent six-repeat runs, all four at 100%.

Two scenarios in this family are not about the overlap and are left as real
weaknesses. `steps-are-ours` sits at 83% — sometimes the model does not reach for
`workflow` at all on "set up a workflow for how I go through the release notes".
`genuinely-ambiguous-is-a-question` is the hardest thing here at 62%: asked to
"set up a workflow for releases", the model usually picks a reading rather than
asking which was meant. Neither is something an arm of this experiment would
move, and both are honest numbers rather than harness artefacts — the checks were
loosened once already, to stop `NoTools` scoring a model that oriented itself
with `list_dir` and *then* asked exactly like one that guessed.

### Deliberately not in v1

Voice dictation and vision both belong here eventually and neither is on the
critical path to a usable assistant; whisper is a subprocess and a wire adapter,
images are a content part, and both are additive. Also out: the Cogsworth
WebSocket transport, proactive jobs, MCP into Planner and Stickies (the next
project, not this one), multiple simultaneous threads generating, i18n.

## Architecture

```
src/
  model/                     no GTK, no server — cargo test with no display
    wire.rs                  the OpenAI-compatible request/response shapes
    turn.rs                  TurnStream: stream → TurnState. The wire adapter.
    thread.rs                turns, persistence, messages_for_model
    project.rs               instructions, tools, folder; the default project
    instructions.rs          system prompt composition, cache-prefix order
    memory/                  the vault as memory
      mod.rs                 remember / recall / forget, and applying a plan
      observation.rs         the saved line: kinds, the format, what it is worth
      ambient.rs             what rides in every prompt, and its budget
      ledger.rs              what has been reached for, and when
      harvest.rs             reading a finished turn for anything durable
      dream.rs               nightly consolidation: two passes and the rails
    compaction.rs            token threshold, rolling summary, floor
    voice.rs                 endpointing, continuation, and answers as speech
    tools.rs                 tool declarations and the gate policy
    eval/                    three eval suites: prompt, recall, memory
    settings.rs              persisted preferences; config vs settings vs thread
    office/                  making documents; no GTK, no display, no subprocess
      markup.rs              Markdown → the Block spec every writer consumes
      docx.rs xlsx.rs pptx.rs  OOXML parts
      zip.rs xml.rs          stored-entry archive, and escaping
      pdf.rs                 Blocks → Cairo/Pango → PDF
      skills.rs              SKILL.md text: catalogue always, bodies on demand
  ui/
    application.rs           owns projects, index and the client; the mutator
    window.rs               split view, breakpoint, banners
    sidebar.rs              the project tree: projects and their chats
    project_view.rs         a project's page: instructions, files, chats, runs
    file_tree.rs            a folder, as a tree that opens
    conversation.rs         the scroller of turns
    turn_view.rs            TurnState → FamiliarTurn
    turn.rs                 the turn widget: setters only
    markdown.rs             Brain's spans → TextTags, read-only
    composer.rs             the entry, send/stop, Escape
    approval.rs             the gate dialog
    preferences.rs          AdwPreferencesDialog
    client.rs               libsoup: request, SSE read, cancel
    embedder.rs             the embedding thread; the only other socket
    voice/                  talking to it: five boundaries and a window
      recorder.rs           pw-record on a pipe
      speech.rs             two Parakeet models on the second worker thread
      tts.rs                speech-dispatcher, or /v1/audio/speech
      shortcut.rs           a gnome-settings-daemon custom keybinding
      window.rs             the window the shortcut opens
    style.css
```

**Three buckets for state, distinguished by one question each.** *Can it change
after launch?* No → **Config** (server URL, vault path, feature enables). *Is it
the same for every thread and persisted?* Yes → **Settings** (sampling, thinking
budget, visibility). *Does it belong to one conversation?* Yes → **Thread**
(persona, history, workspace). Precedence on load is CLI flag > saved file >
default, and loading never writes, so a one-off flag does not persist.

**The prompt is composed so the cache prefix is structural, not a convention.**
`instructions::build` takes persona, capabilities, ambient memory and a volatile
date line, and the date lands last by construction because llama-server caches
the longest stable prefix. Ambient memory is semi-volatile and recomputed only
at thread boundaries — never mid-turn — so a fact written during a turn shows up
in Background at the next thread switch and is findable by `recall` until then.
Changing temperature mid-thread rebuilds the request from the cached prompt, so
the KV prefix survives.

**Nothing async, and no HTTP stack.** `libsoup` is the platform's HTTP client,
it ships in the GNOME runtime, and its async calls complete on the GLib main
loop — so a streamed turn is a `glib::spawn_future_local` reading an
`InputStream` line at a time, with no tokio, no channel, and no worker thread.
`serde_json` parses each SSE frame, as it does everywhere else in these repos.
Cancellation is a `gio::Cancellable`, which is the same object the Escape key
and the stop button both trigger.

**Widgets emit intent; `FamiliarApplication` is the only thing that writes a
file or mutates the index.** Same rule as Brain, same reason.

**A spoken question is an ordinary turn and voice is not a mode.** The
microphone, the speech model and the synthesiser are boundaries under
`ui/voice/`; what they produce is a question that goes down the same path a
scheduled run takes — `Chat::Background`, no turn widget, a real chat on disk —
so memory, folding, workflows and the sidebar all apply to it without knowing
where the words came from. Two consequences are worth stating because they
constrain everything else. The shortcut works with the main window closed, so
the spoken path may never touch a `Window`; and the voice register is appended
to the *question* rather than the system prompt, so a chat that mixes typing
and talking keeps one cached prefix instead of alternating between two.

## Testing

The bulk of the tests need neither a display nor a server, because the hard
parts are folds over data. `model/turn.rs` is fed recorded stream frames and
asserted against the parsed `TurnState`, including the malformed and truncated
ones. `model/compaction.rs` is pure functions over a message list — `view`,
`to_summarize` and the threshold — with a fake summarizer standing in for the
one call that needs a server. `model/memory/` runs against a vault in a `tempfile::TempDir` and
asserts the notes Brain would read back — including what a night's consolidation
does to them, which is the one place text is removed unsupervised. `model/instructions.rs` gets a property
test: the volatile line is last, always.

`model/office/` is the same shape: the writers are folds from blocks to bytes,
so a test reads a part straight back out of the archive and asserts on the XML
Word will read. `examples/office.rs` is the other half, and the same seam
`examples/preview.rs` is for the widgets and `examples/memory.rs` is for the two
memory calls that never happen in a turn — it writes one of each file so they
can be opened, because "will Word take this?" is not a question a unit test
answers.

`tests/session.rs` drives a whole thread through a fake transport — send,
stream, tool call, approval, cancel, reload from disk, assert the rendered turn
matches. One `tests/widgets.rs` with a hand-rolled case runner, because GTK is
thread-affine. `./test.sh` runs fmt, clippy `-D warnings` and tests, headless
under Xvfb. `examples/preview.rs` renders a conversation to PNG.

Nothing in the suite talks to a real `llama-server`; the transport is an
injected seam and the wire shapes are recorded fixtures, which is also how a
llama.cpp change gets diagnosed rather than guessed at.

### Evaluating the prompt

The system prompt is the one part of the app that unit tests cannot hold to
account. `instructions.rs` proves the *order* of the sections; nothing proved
that the paragraph about `news` actually stops the model writing a search query
into `topic`. `model/eval/` is that second half, and `examples/eval.rs` runs it.

**It judges the working, not the answer.** No tool is ever run. Every call gets
an invented result shaped like the string `ui::runner` would have returned, so
a pass costs one process talking to one local model — no Exa spend, no network,
no vault, and no dependence on what the weather happens to be doing. What is
scored is which tool the model reached for, in what order, with what arguments,
and what it said while doing it. A scenario asserts things like *`news` before
`web_search` for a question about what has changed*, *`read_skill` before the
first `create_document`*, *`weather` with no coordinates for "is it going to
rain"*, *no second call after the user declines one*.

**Two axes, and only one of them was predicted.** A scenario's checks say what
that question should have looked like. `antipattern.rs` runs over every trace
regardless: an identical call repeated, arguments that are not JSON, a tool that
was never offered, arguing with a decline, thrashing, going quiet after running
six tools, a `<tool_call>` written into prose, a call rescued from the thinking,
a round that produced nothing at all. That second tally is what catches a prompt
change making things worse in a way nobody wrote a scenario for.

Two of those were added because this suite found things the first tally could
not explain, and both are worth stating:

- **`LeftInThinking` counts what the *server* got wrong**, not the model.
  A turn that only answered because `turn::recover_tool_calls` rescued it scored
  the same as one that never needed rescuing, which hid the upstream bug behind
  a passing number.
- **`ProducedNothing` used to be nearly unreachable.** It required the thinking
  to be empty as well as the answer, so the commonest silent round in this suite
  — thinking present, reply lost — was never counted as one. It now means what a
  person would see: no answer and no call.

`--verbose` prints the tail of the thinking whenever a round produces neither.
That is the only evidence such a round leaves, and it is what identified
llama.cpp#22684 as the cause rather than the prompt: the traces were empty by
definition, and the fix was invisible until the thinking was readable.

**A fixture the model can check is a different problem.** Every other stub here
returns something unverifiable — the model cannot know what Exa would really
have found, so a plausible answer is a good answer. Python is not like that: the
model wrote the script, so it can read the output back against it, and a fixture
that answers `print("hello")` with `hello 48213` has told it the interpreter is
broken. One run said exactly that — *"the Python tool is corrupting its output
on every call"* — and spent eight calls probing the sandbox instead of answering.
So `world::python` parses the script rather than guessing at it: a print of
nothing but string literals echoes them, a directory listing gets a listing, a
computed value gets a number carrying the label and the precision the call asked
for, and a script that never prints gets nothing back. Two scenario fixtures had
the same disease in a smaller way — a total 200 out from the bills in the
question, and a first turn referring to a conversation that never happened — and
in both cases the model behaved correctly and the *check* was what failed. That
is the recurring lesson of this harness in its third form: a fixture has to be
wrong in the ways the real tool is wrong, and in no others.

Three more of the same kind turned up together, and the point of listing them is
that each looked like a model failure and none was:

* **The leak detector counted newlines.** A round was recorded as having written
  a tool call into its prose whenever `strip_tool_noise` changed the answer — and
  that function trims, while almost every answer ends in a newline. The
  commonest antipattern in the report, 27 sightings across nine scenarios, had
  never happened once.
* **The real leak was being deleted rather than rescued.** A `<tool_call>` in
  `reasoning_content` is recovered (llama.cpp#22684, below); the identical thing
  in `content` was stripped and dropped, so a call the model *did* make became a
  turn that said nothing and did nothing. Both channels are read now, thinking
  first, under the same guard: only when there is no call and nothing left to
  say, which is what distinguishes making a call from talking about one.
* **`documents/no-skill-for-plain-text` measured the fixture.** It asked for the
  roof contractor's name to be jotted into a file that already named them. The
  model read the file, said it was already there, and was marked down for not
  writing — correct behaviour, scored as failure, twice in three runs. A test now
  asserts the line a scenario adds is not already in the file it adds it to,
  which is the mirror of the test asserting a scenario's subject *is* in the
  world.

The antipattern detector also had to learn that a conversation's tool list is no
longer fixed. Scoring a `use_tools` run against the tool set it *started* with
reported twenty-seven sightings of "called a tool it was not offered" for calls
to a tool the model had been handed, correctly, one round earlier.

**Everything is fixed except the prompt.** The date is `TODAY`, a constant, not
the calendar — half the suite is about time and a score that drifts with the
week cannot tell a regression from a Tuesday. The ambient memory block is a
fixture. The tool results are fixtures. What moves is the prompt: the persona
via `--persona`, and the capability notes by editing them where they live —
`tools::guidance` and the modules it draws from — and rebuilding. The latter is
the usual case, since that is where nearly everything governing tool use is
written. A report records the whole prompt surface it measured, both halves, so
a number six weeks old still has the text attached to it.

**The terminal report cannot answer the question that matters most.** It says
what got worse, which is the right thing to look at after changing a sentence.
It cannot say whether the *expectations are the right ones* — and that question
turned out to be where most of the errors were. Half the low scores in this
suite's history were a check encoding something nobody had agreed to, or a
fixture that already contained the answer, and none of those are visible in a
percentage.

So `--html FILE` writes the run as one self-contained page: every scenario's
ask, its assertions, and the trace of each run, with the failing run open first
and a note box under each scenario. The passing scenarios are in it too, on
purpose — an assertion that always passes is exactly the one nobody ever reads,
and that is how a suite comes to encode a preference nobody chose. Notes are
kept in the reviewer's own browser and leave it only when they press the button.

One file, no network, no build step. The whole `Report` is embedded as JSON and
the page draws itself from it, which means the review surface can never drift
from the run it came from — and that a reviewer needs nothing but a browser.
The payload is escaped for `</`, because the suite's prompt-injection fixtures
contain HTML and one of them contains a closing script tag; unescaped, that ends
the data block early and the page renders blank.

**Sampling is real, so a single run is not a result.** `--repeats` samples each
scenario, a scenario that passes sometimes is reported as *flaky* rather than
counted, and `--baseline` diffs two reports scenario by scenario. Scores are
also grouped by capability, because a change that buys documents at the cost of
the web averages out to nothing overall and should not be allowed to.

A run that never completed is excluded from the score rather than counted as
zero, and a family where *nothing* completed reads as unmeasured rather than as
0%: a `llama-server` that fell over is not a prompt regression, and the report
must not let it look like one. It also retries such a run rather than writing it
off — this machine's GPU watchdog kills the server under a long unbroken load
(`CUDA error: the launch timed out`) and systemd restarts it, which cost a whole
pass before `--retries` existed.

```sh
cargo run --release --example eval -- --repeats 3 --out baseline.json
cargo run --release --example eval -- --persona variant.txt --baseline baseline.json
cargo run --release --example eval -- --filter documents/ --repeats 1 --verbose
```

The suite has a test of its own worth naming: every tool the app can offer must
be asserted about by some scenario, and no scenario may assert about a tool it
did not switch on — a `NeverCalls` on a tool that was never offered passes for
free and would make the suite look better than it is.

### When the server loses the tool call

Qwen 3.5 and 3.6 under llama.cpp regularly emit a complete, well-formed
`<tool_call>` block into `reasoning_content`, leave `tool_calls` absent and
`content` empty, and report `finish_reason: stop`
([ggml-org/llama.cpp#22684]). Nothing in the response says anything went wrong.
The model did the right thing, the parse dropped it, and what the user sees is a
turn that answered with silence.

This is not a rare path. Probed directly against this server on one scenario it
was **five times out of five**, and in the eval it accounted for every failing
run of `planner/ambiguous-is-a-question` — 56% of its checks.

`turn::recover_tool_calls` rescues it, and runs **only when the turn would
otherwise be silent** — no calls and no answer. That bound matters: the model
also writes `<tool_call>` in prose while explaining itself, which `strip_tool_noise`
has always removed, and recovering those would run calls alongside an answer
that already stands on its own.

Two details came from the wire rather than from reasoning about it:

- **The opening tag is usually missing.** llama.cpp consumes `<tool_call>` as a
  parse marker, then fails on what follows and leaks only the remainder, so the
  thinking ends with a bare `<function=…>…</function></tool_call>`. Four silent
  rounds in five looked like that, and a recovery that insisted on the opener
  found nothing in any of them.
- **Arguments arrive escaped.** `[\"search\", \"report\"]` is not JSON.
  Taking it as a plain string would hand the tool one long word where it expects
  a list — a call that runs and fails, which is worse than one that does not run.

Anything that does not parse into a name and a JSON object is left alone. A
garbled block occurs too, and running half a call the model never finished
writing would be worse than running none. For that last case the turn is no
longer silent either: `settle_turn` says the reply did not survive the server's
parsing, because an empty bubble is the one outcome a person can read nothing
into.

The eval scores the rescue separately (`Antipattern::LeftInThinking`), since a
turn that only worked because of it is not the same as one that never needed it.

[ggml-org/llama.cpp#22684]: https://github.com/ggml-org/llama.cpp/issues/22684

### The sibling applications

`planner` and `magpie` are two more tools shaped exactly like `gh`: an argv
handed to `execvp`, no shell, and the *subcommand* decides the gate.
`src/model/planner.rs` and `src/model/magpie.rs` hold the classification, and
each keeps a short list of the verbs that only read — everything absent is
gated, so a list going stale costs an approval click rather than an unreviewed
write to someone's task list.

**A subprocess rather than a crate, and for these two it is not a preference.**
Both applications hold their store in the memory of the running process and
flush it on a tick, so a second writer is silently overwritten within seconds.
`planner agent` and `magpie agent` ride the applications' own command lines —
both set `HANDLES_COMMAND_LINE`, so an invocation is forwarded over D-Bus to the
running instance, which mutates its own store and redraws. Either way there is
exactly one writer.

**Transcribing is the first slow tool in this app, and it needed a new runner.**
`ui::runner::run_slow` differs from `run` in three ways, each of which is a bug
if it is got wrong: standard error stays *separate* (Magpie writes JSON to stdout
and a progress line per percent to stderr, and `run_in`'s `STDERR_MERGE` would
drop those into the middle of the JSON); progress is read line by line **while**
the process runs, rather than collected at the end where nobody would see it;
and there is deliberately **no timeout**, because an hour of conference audio is
an hour of conference audio and a timeout would kill a working job. The wrapper
says what it is waiting for before it starts waiting, and each progress line
replaces the argument on the running tool chip.

**The rules live in the tool's result, not the prompt.** `note_for` appends a
sentence to the response shapes that are easy to report as their own opposite: a
repeating task that is done *and* back on the 10th, a title matching two open
tasks, a `#Project` that does not exist so the task went to the Inbox, a
download that produced audio and no transcript. This is the placement the eval
already argued for elsewhere — a rule read at the moment it applies beats the
same rule carried on every turn that does not need it. The eval's stubs go
through the same `tools::framed` and the same notes, so the harness scores the
string production actually sends.

### The second suite: recall at distance

`--suite recall` grades something the prompt cannot fix. Its scenarios are ten
asks long, which puts them past `keep_recent_turns`, and they plant a fact at
turn two and ask for it at turn ten. Two knobs move that number and `model/eval/
recall.rs` is built to tell them apart.

```sh
cargo run --release --example eval -- --suite recall --compaction off --out ceiling.json
cargo run --release --example eval -- --suite recall --compaction headings --baseline ceiling.json
```

**The model** is why half the scenarios offer no tools at all: a thread that can
reach for `recall` or the web has a second way to look right, and the question
there is what the context alone still holds. That half is the number to compare
across models.

**Compaction** is why the driver folds between turns, through the same
`to_summarize` / `extend` / `view` the application uses. `--compaction model`
is what ships — the real summarizer, against the same server; `headings` is the
offline fallback and the floor; `off` is the ceiling. The gap between arms is
what folding costs, and reports carry which arm they were, because two runs at
different arms are otherwise indistinguishable in the file.

The arms fold whenever there is anything to fold, ignoring `should_fold`. The
gate decides how *often* a real thread folds; these scenarios measure what one
fold *costs*. Ten short turns would never cross 70% of a 175k window, and a
suite that therefore never folded would report a difference of zero and mean
nothing by it.

Where a fact is planted decides whether the fold can carry it, and scenarios sit
on both sides of that line deliberately. `Headings` keeps the first line of each
*user* message and nothing else, so `recall/short-fact` survives a fold and
`recall/buried-fact` — the same distance, the facts three lines down — does not.
A suite where every case failed would not say which part failed.

The remaining two scenarios keep the tools on, because "answer from the thread
rather than looking it up" is guidance like any other and every scenario that
holds it to account today asks at turn one or two. `recall/tooled-fact-at-
distance` fails if the model searches for something it was told; `recall/tooled-
search-at-distance` fails if it has quietly stopped searching at all. Neither is
sound without the other.

**What the first pass changed.** Two of the three findings were about the
harness, not the prompt. The search stubs returned one fixed page list whatever
they were asked, so five differently-worded searches came back byte-identical —
a signal that cannot occur against a real index — and the model escalated to
twenty-five searches in a step. The read stubs succeeded on any path with
plausible fresh material, so gathering never terminated and the model spent
thirteen calls exploring and never wrote the document it had been asked for.
`eval::world` is the answer to both: four notes and nine files, reads answered
against them, and a miss that comes back as a miss. Fixing it moved `documents`
from 76% to 92% without touching a word of the prompt.

The third finding was real, and is now the `Finish the job` note in
`tools::guidance`: the model gathered material and then ended the turn without
doing the work it had gathered for. Measured at +1.0 points overall, and the
first draft of it cost fourteen points of `safety` by telling the model to carry
on without telling it when to stop — so the stop conditions live in the same
note, which is what the test asserts.

**Two samples is not a result.** That `safety` collapse looked real at
`--repeats 2` and disappeared at `--repeats 6` (88% against 86%). Anything under
about six samples is for finding scenarios to look at, not for deciding whether
a change helped.


### The third suite: what it keeps, and what it lets go

`--suite memory` grades the two calls the other suites cannot see, because
neither of them is a turn. The passive reader and the nightly consolidation are
separate generations with no tools, no user watching and no conversation around
them — and they are the two places the assistant changes a person's notes on its
own initiative, which makes them the two most worth grading.

```sh
cargo run --release --example eval -- --suite memory --repeats 3 --out memory.json
cargo run --release --example eval -- --suite memory --filter dream/ --verbose
```

Three families, and half of every one of them scores the assistant **not**
acting — the failure mode of both calls is confidence, not timidity, and a suite
that only rewarded saving and paring would measure a model that does both to
everything.

- **`save/`** — a standing instruction, a preference stated in the middle of
  asking for something else, who someone is, a correction. And which *kind* it
  was filed under, because a preference filed as a passing fact decays out of
  the prompt inside six weeks and the instruction stops being honoured with
  nothing to show for it.
- **`skip/`** — a passing detail, the question that was asked, the assistant's
  own work, a mood, a fact about the world, and a page that tries to get itself
  written into long-term memory.
- **`dream/`** — a healthy memory left alone, a fact said twice collapsed, a
  misfiled preference refiled, a dead line dropped, and the newer of two values
  kept.

What is scored is what the *application* would do, not what the model said. A
reply goes through the same gate, the same parser, the same vetting and the same
`Policy` that run before anything touches a vault — and for a dream case, the
arithmetic pass runs first exactly as it does in production, so the model is
never asked about a line the shipped pipeline has already settled.

**Three of the fixes this suite has bought so far were fixes to the fixtures,
and that is the point of writing the harness down.** The first dream corpora
were three and four observations long, which made two cases unpassable rather
than easy: the policy permits removing a quarter of what is held, and a quarter
of four is one. The first ones dated every use ninety days back, which put every
well-used observation outside the protection window and let a fact wanted twice
be called stale. And a supersession pair aged two hundred days is a pair the
decay floor has already condemned — the case was asking about expiry while
claiming to ask about choice.

### The web pass: where guidance has to live

The `web` family was the weakest at 66%, and taking it to 91% turned on one
thing — **where** an instruction sits, not how it is worded. It is worth writing
down because the wording attempts all failed and looked reasonable while doing
it.

The failure was a research spiral. Asked something open-ended, the model ran
twelve to seventeen searches in a turn, each with a different elaborate query,
and ended with no reply at all. Four formulations of a budget were measured —
"two or three searches is a full effort", a numeric ceiling, a procedural *search
once, then answer* promoted to the first line of the note, the same rule repeated
in the tool description. Every one of them lost. A rule read once, thousands of
tokens earlier, does not compete with the pull of one more query.

What worked was moving the instruction to the end of the tool's own result
(`web::CLOSING_LINE`, and the equivalent tail on a `news` brief). That sentence
is the last thing in the context at the moment the model chooses whether to
search again, and it is there again after every search. **88%, from 78%.** The
matching fix for an empty result — which previously said "try a different angle"
with no floor, and so had none — took it to **91%**, and moved the scenario about
giving up honestly from 50% to 96%.

This is a change to what the application sends, not only to what it is told, and
it earns its place: a real user asking "what's the current thinking on X" was
getting seventeen searches and no answer.

**The count had to leave the prompt entirely.** Even with the closing line, three
scenarios kept spiralling, and the last of the wording ideas — a numeric ceiling
stated four different ways — had by then all been measured and all lost. The rule
now lives in [`web::Budget`], counted in code: a call past the ceiling comes back
without going out, saying the budget is spent and the answer is due. That is the
published result as well as the measured one — a model cannot keep a budget it
cannot count, which is why stating one never worked.

**It is two numbers, not one, since 2026-08-06.** The ceiling was three, which is
the right answer to *how many searches does a question need* and the wrong answer
to *how many may a question have*. A research-heavy ask spent three on its
opening survey; the `fetch_url` for the one page the user had named by name was
queued behind them and refused, and the assistant — asked afterwards why — read
its own budget back correctly and said it should have fetched first. Three is now
`SEARCHES_BEFORE_PRESSURE` and six is `SEARCHES_PER_TURN`. Under three a result
says nothing about the budget at all, because a turn finishing in one search must
not learn that searching is rationed. From the third onward every result ends
with the count and one condition — *name the fact you are still missing, or you
are done* — which is a test the model can apply and fail honestly, where the four
lost prompt wordings were all versions of "try not to search too much". The hard
refusal is unchanged at the wall.

The `fetch_url` sweep is narrower for the same reason. It exists because a model
whose searches are spent will go after the same fact with a URL it invented, and
that is still refused; a page whose *host the user typed* is not that, and runs
whatever the budget says (`web::named_by_user`). Only user-authored text counts —
a URL that arrived in a search result is not a page the user named, and telling
those two apart is the whole job.

What this costs is now what the `web` family measures. The scenarios that cap
calls at three — `semantic-query`, `version-is-not-remembered`,
`cutoff-is-not-an-answer`, `premise-gets-checked`, `a-number-that-moves` — name
the *soft* line on purpose, and they used to be half-enforced by the refusal:
a fourth search came back empty-handed whatever the model intended. Now it runs.
Those checks are unchanged and unprotected, which makes them the honest question:
does a counted, conditional note hold a model that a hard wall used to hold?

It is worth being precise about what that bought, because the headline number
undersells it. The score moved 91% → 93%. But *ran out of rounds* went 11 → 0 and
*used tools and never answered* went 11 → 0: every scenario now ends with a
reply, including the one that had failed `answers` five times in six all the way
through. What remains in the `web` column is no longer behaviour but wording —
the model hedging with "as of my training data" — which only became visible once
it stopped spiralling and started answering.

`Budget` is a number and a string in the model layer, deliberately owning no
enforcement, because two callers have to hold the same line: `ui::application`
before it dispatches, and `examples/eval.rs` before it stubs. A rule the harness
does not also enforce is a rule the score cannot see.

`MAX_TOOL_ROUNDS` came down from 64 to 16 in the same pass. 64 was a runaway
guard and nothing else — a spiral is over in seven rounds and never came near it.
The ceiling cannot be the search budget either, since lowering it far enough to
catch one would cut the document chains it was raised for; the two limits bound
different things.

**The chat template was upgraded and bought nothing measurable.** froggeric v20 →
v21.3 scored 91% against 91%, and the antipattern it was expected to fix — a tool
call written into prose — got *worse* in that sample (14 → 24). It is kept for a
reason the eval cannot see: under v20 the `<|think_off|>` control token was
honoured from **tool-role** messages, so a fetched page could switch the model's
thinking off. v21.3 restricts it to system and user. No scenario would ever have
caught that, which is the argument for reading a changelog as well as a score.

**The fixtures lied in a way the model caught first.** Results were served from
`example.org`, `example.com` and `example.net` — the domains RFC 2606 reserves so
that nothing mistakes them for real sites. The model did not mistake them either.
It said so, in a trace: *"Those results came back as placeholder URLs… I'm not
getting real web results back right now"* — and then kept searching for the real
index it assumed was behind the broken one. That single detail was worth more of
the score than any wording of the prompt, and it explains why the model had also
been refusing to cite URLs it believed were fake. Fixtures now serve from
`stub::HOSTS`, and two tests keep every result and every scenario off the
reserved domains.

**Staleness is now scored.** Eight scenarios ask what the model does with a
question whose answer has moved since training — a version, a price, who holds a
post, a premise the user states from their own stale memory — and two controls
ask what it does with a definition and a sum, because "look it up" is trivially
easy to over-apply and an assistant that searches for the meaning of an HTTP 429
is a worse one.

One result there is a caution about negative instruction. A draft forbade the
excuse by quoting it — *never give your training cutoff as the reason* — and the
model began saying "training cutoff" in scenarios where it never had. The words
were in its context, so it reached for them. The note now says what to do instead
and names none of them, and a test asserts the phrases stay out of it.

## Dependencies

`gtk4`, `libadwaita`, `gio`, `serde`, `serde_json`, `chrono` — Brain's set,
plus two.

**`soup3`.** The platform HTTP client, already in `org.gnome.Sdk` and already
loaded in any GNOME session. The alternative was `reqwest` and a tokio runtime
spun up beside the GLib main loop, with a channel between them, to do what
libsoup does natively on the loop we already have. It needs `libsoup-3.0-dev` at
build time, which is not installed on this machine yet — one `apt install`, and
nothing for Flatpak.

**`brain`, as a git dependency.** Familiar reads and writes Brain's vault, and
two implementations of a file format are two implementations that can disagree —
the failure mode being a mangled note, which is the one thing Brain's design
promises will not happen. Reusing `brain::model` also brings the Markdown
scanner that renders replies. The cost is that Brain's crate exposes `pub mod
ui`, so linking it drags in a GTK half Familiar does not use; the fix is a
three-line `ui` feature in Brain, default on, and it should be made there rather
than worked around here. During co-development a `[patch]` points at
`../brain`.

**`cairo-rs`, `pango` and `pangocairo`.** All three are already in the process
— GTK paints every widget through them — and all three are in `org.gnome.Sdk`,
so making them direct dependencies adds a `Cargo.toml` line and nothing to the
runtime or the package. `cairo::PdfSurface` is what writes a PDF; `v1_16` is
turned on for `set_metadata`, so a generated file carries a real title rather
than showing its filename in every reader's tab. The document writers need no
other crate: a ZIP of stored entries and some XML is not worth a dependency.

No SQLite, and no embedding model. Recall is Brain's index — fuzzy over titles
and aliases, substring and word matching over text. Hybrid semantic recall was
worth building in `llamatui` because a SQLite blob had no other retrieval; here
the retrieval already exists, is tested, and is the same one you use when you
press Ctrl+K in Brain. If it proves too literal in practice, that is a
measurement to take, not a dependency to add up front.

Icons, `.desktop`, metainfo, and cargo+bash packaging follow Stickies.

### Images and documents

Paste an image, drop one, or drop a PDF. Attachments wait as thumbnails under
the entry until you send them, because an attachment you cannot see is one you
cannot take back. They are content-addressed by SHA-256, so the same screenshot
in three chats is stored once, and they live under the project — deleting a
project takes its images with it.

llama-server will not take a PDF: a `data:application/pdf` URL comes back as
*"Invalid uri format"*. So a document becomes text, or pictures, or both — and
**which is a per-page decision**. A report with a typed body and a scanned
appendix is the normal case, and deciding once for the whole file is wrong in
both directions: rasterising a typed page wastes ~750 tokens and can misread a
digit, while extracting a scanned page yields nothing and the model then answers
from an empty string without knowing it.

So every page is extracted, the ones with words are kept as text, and only the
ones without are rendered at 150 DPI and attached as images. Page numbers
survive the whole way, so an answer can cite one — and a page that could not be
included says so *in its place* rather than going missing. The whole thing is
framed as untrusted data, like the memory block, because a PDF is something
somebody else wrote.

## Milestones

1. ~~Model core: wire shapes, `TurnStream` fold, thread persistence, projects,
   instruction composition, settings. No UI, no server. All tested.~~
2. ~~Shell: window, sidebar, conversation view, composer, the libsoup client, a
   streamed answer rendered as plain text. One project, one chat.~~
3. ~~Reading: Brain's scanner over the answer, the thinking disclosure, the
   metrics line, the context-usage bar.~~
4. ~~Chats and projects: the sidebar tree, new/rename/delete, instructions per
   project, reload from disk, the empty and disconnected states.~~
5. ~~Memory: the vault index, `remember`/`recall`/`forget`, the ambient block,
   the file monitor.~~
6. ~~Tools: declarations, chips, the approval dialog,~~ web search and fetch.
7. ~~Compaction, the system note, overflow recovery.~~
8. ~~Preferences, shortcuts dialog, packaging.~~
9. ~~Images: paste, drop, staging, content-addressed storage, and page-aware
   PDF ingestion.~~
10. ~~Workspace: a rooted piece of the filesystem, read ungated and written
    behind the approval dialog.~~
11. ~~Documents: `.docx`, `.xlsx`, `.pptx` and PDF written in-process, PDFs
    merged and split with poppler, and a skill per format loaded on demand.~~
12. ~~GitHub through `gh`, gated by subcommand; the tool chain carried across
    rounds and shrunk rather than dropped when it overflows.~~
13. ~~Weather from the National Weather Service, hourly and daily.~~
14. Heartbeat: ~~the schedule model, the minute tick, the run, the notification
    and the management window~~; the Background portal request and service-mode
    `hold()` so it runs with the window closed.

## Built differently, or not built

Where the finished thing differs from this document, this is what happened.

- **The Flatpak was dropped, and `./install.sh` is the only distribution.**
  Several arguments in this document are made against a sandbox — the
  Background portal rather than a systemd unit, Cairo and Pango rather than
  LibreOffice, `soup3` because it is in `org.gnome.Sdk`. They were written when
  a Flatpak was the intended shape and they still hold on their own merits, so
  they are left as they are; what changed is the premise. The app grew a
  capability surface that is mostly *other programs*: `planner`, `magpie`,
  `gh`, `claude` and `codex`, `podman` for `run_python`, and now `pw-record`
  and `spd-say`. None of them are in the runtime, and reaching them means
  `--talk-name=org.freedesktop.Flatpak` — full host access, which is a larger
  hole than the sandbox was closing. The manifest had also never been built:
  it listed a `cargo-sources.json` nobody generated and named a
  `build-flatpak.sh` nobody wrote. Keeping it would have been keeping a claim
  rather than a package.
- **Push-to-talk was designed for and dropped.** The plan was the
  `GlobalShortcuts` portal, which delivers press *and* release. Measured on
  GNOME 50 / xdg-desktop-portal 1.21.1, `CreateSession` returns `NotAllowed:
  An app id is required`; since portal 1.21 an application identity is
  mandatory and the mechanism a non-Flatpak app uses to declare one,
  `org.freedesktop.host.portal.Registry`, is not exported by this portal
  build — the bus name is owned by nothing. A systemd scope named after the
  app was tried too and changes nothing. So the shortcut is a
  gnome-settings-daemon custom keybinding, which is press-only, listening is a
  toggle, and silence ends an utterance. Reading `/dev/input` directly would
  restore push-to-talk at the cost of putting the user in the `input` group,
  which hands a keylogger to every process they run. Not worth one key.
- **No model routes a spoken question to a chat.** The design considered
  asking the model whether an utterance belonged with an existing
  conversation. It sits on the one path where latency is the whole product,
  and it is wrong *silently* — a question appended to the wrong chat looks
  like nothing at all until the answer is strange. A follow-up window does
  most of the work for nothing, and what it cannot decide, the person can:
  the window names the chat it is continuing and one button starts a new one.
- **Speaking and listening do not overlap.** Full barge-in means an open
  microphone during playback, which means echo cancellation and a permanent
  microphone indicator in the panel. Pressing the shortcut while it talks
  stops it talking and starts listening, which is what interrupting somebody
  is, and it needs neither.

  **This was then built anyway, used, and removed** — so the reason is now a
  measurement rather than a judgement. `Barge` watched the microphone while the
  answer played and triggered a margin above whatever it had learned during a
  settle window. On this desk the assistant's own voice off the speakers reaches
  the microphone at a peak of **0.577**, against **0.578** for the person in
  front of it. The webcam's cancelling holds its median down to 0.014, so most of
  it is gone — but it leaks in bursts, 16% of blocks above 0.32 in unbroken runs
  up to 640 ms, against a `trigger_ms` of 160. There is no threshold in that, and
  the behaviour was accordingly a coin toss decided by which 600 ms the settle
  window happened to cover: a quiet one and it interrupted itself, a loud one and
  it could not be interrupted at all. Both were reported. It also put the tail of
  its own answer into the transcript of the next question, which is the same
  fact wearing a different symptom.

  Audio arriving while it is not listening is now discarded as it arrives. The
  microphone stays open, because the watchdogs that get the window out of a stuck
  state count in blocks of audio rather than carrying timers of their own.

  On headphones the level approach works, and that is the trap: a feature whose
  correctness depends on which output device is plugged in is worse than no
  feature, because it is impossible to be told about usefully.

- **`Store` lives in `project.rs`.** The file tree above named no owner for the
  data directory, and projects and their chats are one on-disk shape reached
  through one slug-to-path check. Splitting them would have put that check
  somewhere a caller could route around it, which is the only thing standing
  between a project name and a path outside the data directory.
- **Thread ids are allocated by the store, not by the clock.** An id is a
  timestamp to the millisecond, which is unique enough for a person clicking
  "New thread" and not unique enough for two in a row — the second silently
  overwrote the first, and a test caught it. More decimal places only move the
  collision, so the store asks the directory which id is free.
- **A failed frame is an `Event`, not an `Err`.** A stream can deliver three
  good frames and then a bad one in the same read, and returning `Result` from
  `push` would throw away the three. Failure arrives in order with everything
  else and the caller decides when to stop.
- **Metrics durations are milliseconds, not `Duration`.** A `Duration` reaches
  a JSON file as `{"secs":0,"nanos":320000000}`, and a thread file is meant to
  be readable. `TurnState` keeps `Duration` for the live fold; only the
  persisted shape converts.
- **Every project opens onto a "New Chat" row.** A tree of chats cannot express
  "go to this project and start something", and a project with no chats yet
  would otherwise be somewhere you could not reach at all. The row is
  activatable but never selectable: leaving the highlight on it would say the
  open conversation is a button.
- **Every row's menu is its own.** A right-click on a chat offers Rename and
  Delete, on a file Open and Move to Trash, on a project its settings and — for
  a named one — deletion. The gesture is bound per row in the factory's `bind`
  and removed in `unbind`, so a recycled row never carries the last row's menu.
  Each item carries the row's key as its action target rather than the widget
  remembering which row was clicked, so a menu left open across a rebuild acts
  on the row it named or on nothing.
- **Clicking a project opens its page; the arrow expands it.** The files, the
  instructions and the schedules are the project, and a folder tree nested in a
  300-pixel sidebar could not be read — an empty one looked like a bug rather
  than like an empty folder.
- **Deleting a chat is an undo, deleting a project is a question.** A chat is
  one file and it is still in memory, so it goes with a toast that can put it
  back. A project takes its chats with it and there is no single file to
  restore, so that one asks first — and says that the folder is not touched,
  because a project that took a folder of real work with it would be
  unforgivable.
- **A file goes to the trash, not to `unlink`.** It is the user's file and they
  can put it back, which is a better answer than a confirmation dialog this
  application would have to be sure about.
- **Memory writes under a marked heading, and only removes what it wrote.**
  `## Noted by Familiar`, one `- ` line per observation, each tagged
  `#familiar` and dated in an HTML comment. `forget` will not touch a line
  without that mark, and no note is ever deleted — an empty section is left
  behind instead. Your files stay yours even when Familiar made them.
- **The web tools are declared and answer that they are unconnected.** Half of
  milestone 6 shipped: the shapes, the chips and the gate are real, and Exa is
  not wired up. A tool that silently returned nothing would teach the model to
  stop using it and one that lied would teach it to trust a lie, so this one
  says what is true.
- **The icon was valid SVG that never loaded.** gdk-pixbuf sniffs roughly the
  first 256 bytes to decide a file is SVG; a three-line header comment pushed
  `<svg>` to byte 268 and the app grid showed a blank tile. There is now a test
  that loads every shipped icon and asserts the element lands before byte 256.
- **Deleting the open thread used to resurrect it.** `delete_thread` removed the
  file and then called `new_thread`, which begins by *saving the open thread* —
  writing the deleted one straight back under a toast saying it had gone.
  Splitting out `start_fresh_thread`, which does not save, fixed it.
- **The prompt size shown is the whole prompt, not the prefill.**
  `timings.prompt_n` counts only the tokens actually processed, so a turn that
  hit the prompt cache reports 23 for a 5,000-token conversation — true about
  the prefill, and wrong about how full the context is. llama.cpp reports
  `cache_n` alongside it and the whole prompt is the sum, so that is what the
  metrics line and the context bar use — and the caption shows the split, which
  makes the KV cache visible instead of guessable. The prefill *rate* still
  comes from what was actually processed.
- **The default persona no longer asks for brevity.** "A one-line question gets
  a one-line answer" read as an instruction to think less as well as write less,
  and a reasoning model took it. It now asks the model to match the depth of the
  question and says out loud that the user can see how long it thought.
- **Web search is Exa's REST API, not its MCP server.** The value in Claude
  Code's Exa plugin is the *skill* — describe the page you want rather than
  typing the question, use a category, run different angles rather than
  synonyms — and that is prompt text, which works without an MCP client.
  `/search` returns page text inline, so one call does search-and-read and the
  model synthesises, which is what the plugin's subagent does with a fleet a 27B
  has no room for.
- **`recall` hides Familiar's own section from itself.** The first live test
  had the model report "a note that he was noted by Familiar" — it had read the
  `## Noted by Familiar` heading as a fact about the person. Recall now returns
  the note's own prose plus the observations as a list, and never the
  scaffolding around them.
- **Applying a fold runs inside `build_request`.** That is the only place the
  whole history is assembled, so there is nowhere else it could run. Computing
  one runs in `fold_if_needed`, off the back of `settle_turn` — the seam where
  no turn is in flight and a second request to the server costs nobody a wait.
- **A turn is several requests.** A turn that calls a tool sends again with the
  results, up to six rounds. `InFlight` accumulates the answer, the thinking
  and the calls across rounds so it is still one turn on screen and one turn on
  disk.
- **List bullets stay visible in an answer.** Brain keeps them because a text
  view has no glyph to put in their place while editing; here the reason is
  stronger. The scanner's spans are offsets into the model's text, so swapping
  `- ` for `•` would shift every offset after it and mis-style the rest of the
  answer. The buffer holds exactly what the model wrote, and only the syntax is
  hidden.
- **The thinking duration is measured in `TurnStream`, not the widget.** It is
  the only thing holding a clock, and putting it there means the number is
  persisted with the turn — so a reopened thread says "Thought for 4s" rather
  than losing it. `TurnMetrics` gained a `thinking_ms` for the same reason.
- **The answer view is transparent.** A `GtkTextView` paints the theme's view
  background, which made prose sit in a visible box on the page. It is styled
  to `background: none` on both the `textview` and its `text` node — the second
  is easy to miss and is why the first attempt only half worked.
- **A collapsed `GtkExpander` does not keep its child in the widget tree**, so
  the reasoning is read back through the widget rather than by walking it. This
  is why `Turn` grew a `thinking_text()` accessor.
- **`examples/preview.rs` is how the UI gets looked at.** GNOME refuses a
  screenshot to a non-interactive caller — the D-Bus call returns
  `AccessDenied` — so "does this look right?" is answered by building the real
  widgets and painting them offscreen with a `WidgetPaintable`, exactly as
  Brain does it. It caught two layout bugs on its first run, and later the
  answer-in-a-box above. Like Brain's, it grows the window until something is
  drawn: a `WidgetPaintable` declines to draw a scroller whose content
  overflows it, so a fixed height silently produced no file at all.
- **The composer stays usable while an answer streams.** The design implies one
  turn at a time and that is still true of the transport, but nothing is gained
  by locking the entry: you can type the next question while the current one
  finishes. Only the button changes, from send to stop.
- **Widget visibility is asserted with `get_visible`, not `is_visible`.** The
  latter is ancestor-aware, and a window that is never presented — which is
  every window in the tests, since mapping one needs a compositor — reports
  everything inside it as invisible.
- **Only a tag followed by a call is stripped from an answer.** The leak guard
  originally ate everything after any `<tool_call>`, which meant an answer
  *explaining* what a leak looks like lost its second half — a worse bug than
  the one being fixed.
- **Documents ride on the workspace switch, not their own.** They are written
  into the workspace and read out of it, so a separate top-level toggle could
  be on while the thing it writes to was off — eight tools that fail on every
  call, which is how a model learns to stop using them. The Preferences row is
  nested under Workspace and goes insensitive with it.
- **The PDF is drawn a line at a time, not a paragraph at a time.** A Pango
  layout painted whole cannot straddle a page boundary, so a paragraph longer
  than the space left either overflows the margin or jumps to the next page and
  leaves a hole. Walking the layout's lines and breaking between them is the
  only version that survives a document of arbitrary length, and it is why
  `pdf::Cursor` exists.
- **`pdf::write` returns the page count.** It is the one fact about the result
  that neither the model nor the user can see without opening the file, and
  Cairo puts page objects in compressed object streams, so counting them
  afterwards would mean carrying an inflater. Reporting "4 pages" is also what
  makes the tool chip say something rather than merely succeed.
- **A turn gets 64 rounds of tools, and is told when it is on the last one.**
  The limit was six, set when the longest sensible chain was search → read →
  write. Reading four documents and writing a deck is six rounds before a single
  malformed argument or declined approval is retried, so real work hit the
  ceiling — and hitting it settled the turn with whatever answer text had
  accumulated, which for a model mid-chain is none. The user got a row of chips,
  a red line, and no reply, and the model was never told why, because
  `set_failure` only draws a widget. The ceiling is now a runaway guard rather
  than a budget (Escape already cancels a turn the user has lost patience with),
  and the results of the last permitted round carry a note telling the model to
  answer with what it has. A truncation became an ending.
- **What bounds a long turn is the context window, not the round count.** Tool
  results accumulate across rounds and compaction cannot fold them, because it
  runs only at turn boundaries — and overflow recovery is skipped once an
  approved tool has run, since retrying would repeat its side effects. So
  `read_pdf` gets 12,000 characters where a *dropped* PDF gets 40,000: one is
  attached to a single question, the other stacks up to sixty-four times with
  nothing able to reclaim it. Where the text is cut the note names the page it
  stopped at and `read_pdf` takes a `pages` range, so a long document is read in
  pieces rather than being a dead end.
- **The schedule model is hand-rolled, and cron was not the alternative.** The
  hard part is not parsing `0 7 * * 1-5`; it is deciding, given a last run, a
  time now and a laptop that was asleep between them, whether to fire. No cron
  crate answers that, so a dependency would have bought the easy half. Neither
  ChatGPT's tasks nor Claude Code's scheduler exposes cron as its primary model
  either — both ship the preset list this one mirrors.
- **A new schedule starts its clock at `now`, not at the epoch.** A daily 07:00
  set up at 09:00 must wait for tomorrow, and a `last_run` of `None` treated as
  "infinitely overdue" would fire immediately and then once for every occurrence
  since 1970.
- **Lateness is measured from the scheduled moment, not from the last run.**
  Otherwise a fortnight of downtime makes every occurrence look impossibly
  overdue and nothing ever fires again.
- **`last_run` is recorded before the turn, not after.** If the answer never
  arrives — the server is down, the turn is cancelled — the schedule still has
  to move on, or every tick for the next twenty minutes tries again.
- **The first weather station in a grid is regularly not reporting.** The
  documentation's advice is to cache the first station in `/gridpoints/…/stations`
  permanently. For grid `ILN 74,80` that is `KOSU`, whose latest observation is
  a 404 — so the tool would have reported no current conditions, every time, for
  a location whose *second* station answers perfectly. The list is walked until
  one replies. Three more things in that API cost a bug each: coordinates longer
  than four decimal places 301-redirect to the truncated form; observations are
  metric while forecasts from the same document set are imperial; and every
  observation field may be null, because stations drop readings routinely.
- **The forecast is fetched before the observation, and the observation may
  fail.** A grid with no reporting station still answers the question, and
  losing the week to a silent weather station would be losing the useful half to
  the optional one.
- **The tool chain used to be forgotten every round.** `build_request` took the
  *current* round's messages as `extra`, so on round three the model could not
  see what round one found: it re-ran the call or answered from nothing.
  `InFlight` accumulated the calls for the screen and the transcript, which is
  what the design section promised, and never for the prompt. `exchanges` now
  carries the whole chain into every request, which is what makes a long chain
  add up to anything — and is what made the round cap and the result budgets
  matter in the first place.
- **An overflow mid-chain empties tool results rather than reaching for the
  floor.** `reduce_to_floor` throws away the record that the calls happened,
  which is exactly why it may never run after an approved tool: the model would
  redo the write. `shrink_tool_results` keeps every assistant `tool_calls`
  message and every `tool` message paired with it and replaces only the bulk
  with a note saying the call ran and can be repeated. That is safe after a
  write, which is when recovery matters most. Each attempt keeps fewer results
  whole; the floor is what is left when there is nothing to shrink.
- **A note the assistant has to invent goes in `Familiar/`.** New notes used to
  land at the vault root, and a few weeks of use left the root indistinguishable
  from the notes you wrote yourself. Writes are scoped; reads never were —
  `recall` runs over the whole index and always has. A subject that *already*
  has a note anywhere is still written there, because the assistant joining your
  note is the point and a rival copy under `Familiar/` would be the failure.
- **A numbered list needs a counter in the PDF and not in the `.docx`.** The
  parser takes `1. ` off the text and into the block type, which is right for
  Word — its numbering definition renumbers a list from the style, and a number
  in the text would be drawn twice. The PDF painter has no such machinery, so
  the first version drew a bullet glyph for both kinds and silently turned
  "1. 2. 3." into three identical dots. Only rendering the example document and
  looking at it found that, which is the argument for `examples/office.rs`.
- **The archive stores its entries rather than deflating them.** Method 0 has
  been legal since 1989 and every reader takes it, so the alternative was a
  DEFLATE compressor — a dependency, or several hundred lines of Huffman coding
  — to save kilobytes on files that are kilobytes. It also means the tests can
  read a part straight back out of the bytes Word will read, with no second
  implementation of the format to disagree with the first.
- **There is no reader for the Office formats.** Reading a `.docx` somebody
  else wrote means inflating it, which is the compressor problem again, and no
  tool needs to. `read_pdf` reads PDFs and says so; a model that asks to read
  back a `.docx` is told plainly it cannot.
- **The extension is checked, never corrected.** A `.docx` written to
  `report.txt` opens as a wall of gibberish and neither the model nor the user
  can tell the tool was at fault — while silently renaming what was asked for
  hides the mistake from both. The refusal names the path it should have been.
- **A package test, separate from the per-format ones.** Each writer asserts
  its own parts are present, which is not the same as the file opening: what
  makes Word offer to *repair* one is a relationship pointing at a part that
  is not there, or a part with no declared content type. One test walks all
  three archives and checks both, because that class of bug is invisible in
  correct XML.

### Talking to it: the gate is a measurement, not a taste

Silence ends an utterance, because the shortcut is press-only. So a number
decides when somebody stopped talking, and every version of that number has been
wrong in a way that only a microphone could show. What follows is the measurement
the current one comes from, because the numbers alone read as arbitrary and the
next person to change a microphone will need it. Levels are on the curve
`ui::voice::recorder::level` produces — RMS raised to 0.4 — over 40 ms blocks.

A silent room and seven seconds of ordinary talking, on this desk, through the
Insta360 with its AI noise cancelling **on**, which is how it normally runs:

| | silent room | speech |
|---|---|---|
| median | 0.124 | 0.343 |
| p90 | 0.221 | 0.511 |
| p99 | 0.357 | 0.562 |
| rolling-window p25 | 0.073 | 0.225 |
| rolling-window minimum | 0.017 | 0.026 |

Four things follow, and three of them contradict what the code said before.

**A rolling minimum is not a room.** It reads 0.017 in a room whose actual level
is 0.124, because this microphone emits *silence interrupted by noise* — a tenth
of its blocks in a quiet room are exactly 0.000. So `room + margin` fell below
`floor` on every block, `gate()` returned the constant 0.20 forever, and the
adaptive machinery contributed nothing while the room it was adapting to sat
above that floor 15% of the time. The room is now the window's **25th
percentile**: low enough to sit under speech, which has gaps, high enough that
occasional true silence cannot drag it to zero.

**The ceiling is set by the gaps in speech, not by how loud a voice is.** Silence
under the gate is what ends an utterance, so a gate above the quiet moments
between words ends it mid-sentence. At a gate of 0.35 the longest gap inside
ordinary speech is 640 ms; at 0.40 it is 1320 ms, past `hangover_ms`. `loud` came
*down* from 0.42 to 0.34 for that reason — the old value was justified as "this
loud is a voice", which is true and beside the point.

**The old gate had a 10% margin and needed 300%.** At 0.20 the longest quiet run
in a silent room is 880 ms against an 800 ms hangover, so a noise burst usually
reset the silence counter first and `Ended` never fired: the microphone stayed
open for two minutes. At 0.30 that run is 1720 ms. `margin` carries the working
range now (0.21 above a measured p25 puts the gate at 0.23 in a quiet room and
0.30 in this one) and `floor` is a sanity minimum rather than the value silently
always used.

**Cumulative speech cannot say whether the words are yours.** `has_speech()`
never resets, so a few seconds of any real room made it true and every word the
streaming model hallucinated on room noise then reset `Spoken`'s clock. Measured
under the pessimistic assumption that the live model emits words for every chunk:
twenty of thirty-one chunks of a *silent* room were credited to the speaker and
the microphone never closed. `Endpointer::heard_you` asks instead how much of the
last 1.5 s was above the gate — at most 200 ms for a silent room, at least 720 ms
for speech, so `attributable_ms` sits at 400 in a gap nothing lands in. Under the
same pessimistic assumption it now credits none of them and gives up at 8 s.

**Where the old numbers came from is worth stating, because it is an easy mistake
to repeat.** The figure the first set was reasoned from — "the microphone reads
0.01" — is real, but it measures *the assistant's own voice arriving off the
speakers with the webcam cancelling it*. That is the right number for asking
whether there is an echo to beat, and it was then used as though it were the
room. **The room's idle floor had never actually been measured**, and it is
0.124. One number standing in for another is how a floor of 0.20 came to be
described as "well over a quiet room" while sitting *under* this one's p90.

The same substitution is what put barge-in on a floor of 0.18 and, once that was
corrected, is what finally removed the feature: retaking the number it should
always have been measured against — the assistant's own voice *as the microphone
hears it* — gave a peak of 0.577 against 0.578 for a person, and no threshold
lives in that. See "Speaking and listening do not overlap" above for the removal.
Two rounds of tuning were spent on a threshold that could not exist, because the
thing it separated had never been measured.

**Everything above is one microphone in one room, and that is the honest status
of it.** The shapes are general — a percentile beats a minimum, a ceiling is set
by speech's gaps, recency beats a running total — but the constants are this
desk's, with this webcam, cancelling on.

Two failures here are not about levels at all. **A capture that dies stalls
everything**: every way out of `Listening` is driven by a block of audio
arriving, including the endpointer's own patience, so `pw-record` exiting — a
source that no longer exists, with stderr silenced — left the window listening
forever with nothing to say so. `Recorder` now reports a pipe that closes, and a
timer catches the other case, a process that stays alive and delivers nothing.
And **the live model's last part-chunk was thrown away**: the streaming encoder
emits nothing until it has a whole 560 ms, so the last thing anybody said sat in
`pending` and never reached the screen. Scribe had the identical hole, where it
took the end off every dictation rather than off a preview.

## Settled

- App id `us.hagreli.Familiar`, binary `familiar`, GObject classes `Familiar*`.
- Thinking is shown, stored, and never replayed.
- Memory is Brain's vault. There is no second store and no import step.
- The assistant only appends to notes, and only removes what it appended.
- Transcripts are files. There is no database.
- The wire adapter is one module, so the Cogsworth WebSocket transport is a
  sibling of it rather than a rewrite.
- Projects exist from v1 even with one of them, because retrofitting a
  workspace concept underneath a flat chat list is a migration.
