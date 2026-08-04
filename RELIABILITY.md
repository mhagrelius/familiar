# Getting good work out of a small local model

Familiar runs a 27B model on one desktop GPU. It has no frontier model to fall
back on, no cloud to retry against, and no second opinion. Everything below
exists because that constraint is real: a 27B model will do the right thing most
of the time and something strange the rest of the time, and the difference
between a usable assistant and an irritating one is almost entirely in what the
program does about the rest of the time.

This is the whole of that. `DESIGN.md` explains how the app is built and why;
this explains what keeps the model behaving, and how we know it works.

## What "a stock prompting harness" usually means

Most of the tool-calling apps you can read the source of are the same shape: one
big system prompt with the tools described in it, a loop that runs whatever the
model asks for, and results pasted straight back. That shape works with a
frontier model, because a frontier model covers for it. At 27B it produces four
recognisable failures, and all four are things we measured here rather than read
about:

- **The turn that never ends.** Asked something open-ended, the model searches,
  and searches, and searches, and never writes an answer. We recorded seventeen
  searches in one turn.
- **The turn that ends too early.** It reads the file, reports that it read the
  file, and stops — with the actual work undone.
- **The prompt nobody can hold to account.** The system prompt is the largest
  single influence on behaviour and the only part of most codebases with no test
  over it. Change a paragraph, ship it, find out later.
- **The context that quietly fills up.** Tool results accumulate until a long
  turn dies, usually mid-write.

Everything below is a countermeasure to one of those.

## Making the model behave

### The prompt is assembled, not written

`model/instructions.rs` composes four sections in a fixed order: persona,
capability notes, ambient memory, then the volatile line. Two properties come out
of that ordering and both matter.

**The volatile part goes last, by construction.** Today's date is the only thing
in the prompt that changes daily. Put it at the top and every request invalidates
the server's cached prefix and re-prefills thousands of tokens; put it at the
bottom and everything above it stays cached. This is a measurable latency
difference on every single turn, and it is enforced by the type — `Prompt` has a
`volatile` field, so there is no way to compose the sections in the wrong order.

**Capability notes are owned by the module that owns the tool.** The paragraph
telling the model how to use `news` lives in `model/news.rs`, next to the code
that implements it. Turning a tool on and telling the model it exists cannot
drift apart, and adding a tool does not mean editing a central prompt file that
nobody wants to touch.

### Guidance goes where the model will actually read it

This is the single most useful thing we learned, and it was expensive.

The model kept ignoring a rule in the system prompt. We rewrote it four ways — a
plain sentence, a numeric ceiling, a procedural *do this, then that* promoted to
the very first line, and the same rule duplicated into the tool's description.
All four lost.

What worked was moving the rule to the **end of the tool's own result**. That
sentence is the last thing in the model's context at the moment it decides
whether to call the tool again, and it reappears after every call. Same words,
different position: **78% to 88%** on the affected scenarios.

So the app writes guidance into the things it hands back, not only into the
briefing at the top. A search result ends by telling the model it now has what it
needs. An empty search result ends by telling it that saying "I found little" is
a complete answer. A news brief ends by saying it already merged several sources
so there is nothing to double-check.

### When words fail, the program counts

Position was not enough on its own. Some questions still produced a spiral, so
the count came out of language entirely.

`web::Budget` allows three searches per turn. The fourth call does not run: it
returns immediately saying the budget is spent and the answer is due. The model
cannot exceed it, because it is arithmetic rather than instruction.

There is published work saying exactly this — a model cannot keep a budget it
cannot count — and Qwen's own CLI agrees in practice: its system prompt contains
no numeric tool budget at all, and enforces the limit in the runtime. We arrived
at it by measurement first and found the agreement afterwards.

The effect was larger than the score suggests. The score moved 91% to 93%. But
*ran out of rounds* went from 11 to **0**, and *used tools and never answered*
went from 11 to **0**. Every scenario now ends with a reply.

Two details worth copying if you build something similar. The refusal is worded
as a finished state, not a failure — a model told a tool *failed* will retry it,
so it is told the work is done instead. And `Budget` is a number and a string in
the model layer that enforces nothing itself, because two callers have to hold
the same line: the running app, and the test harness. A rule the harness does not
also enforce is a rule the tests cannot see.

### Two limits, bounding different things

`MAX_TOOL_ROUNDS` is 16 — the runaway guard. It was 64, which was useless for
this purpose: a search spiral is over in seven rounds and never came near it. But
it cannot be lowered to search-budget size either, because a real chain — read
four documents, write a deck, retry a malformed argument, wait for an approval —
legitimately runs to six or more, and an earlier limit of six cut those off
mid-work. So the search budget bounds searching and the round cap bounds
everything else.

When the round cap is hit the model is told so and asked to conclude, rather than
having the turn cut off silently. Without that, hitting the ceiling settled the
turn with whatever text had accumulated, which mid-chain is usually none — the
user got a row of tool chips and no reply.

### Finish the job, and know when to stop

The commonest failure in the first measured pass was stopping halfway. The
capability notes now say to treat a tool's answer as the middle of the work, and
— in the same paragraph — list the four conditions that mean the turn is
genuinely finished.

Both halves ship together, and a test enforces that. The first version said only
"finish the job" and cost fourteen points of safety score: told to carry on
without being told when to stop, the model treated a declined approval as an
obstacle to work around.

### Say what does not need looking up

The prompt tells the model to assume its own knowledge has gone stale — versions,
prices, whoever currently holds a post — and to check the user's premise too,
since they may be remembering the same out-of-date thing.

It also says, in the same breath, what does *not* move: definitions, mathematics,
how a protocol works, what an error code means. "Look it up" is trivially easy to
over-apply, and an assistant that searches for the meaning of an HTTP 429 is
worse, not better. The eval carries two scenarios whose entire job is to fail if
the model starts searching for settled facts.

**Never quote the phrase you want avoided.** A draft said *never give your
training cutoff as the reason*. The model promptly began saying "training cutoff"
in scenarios where it never had — the words were in its context, so it reached
for them. The rule is now stated positively and names none of the phrases, and a
test asserts they stay out of the prompt.

## Safeguards

### Nothing that changes anything runs without a person

Every tool is one of three shapes, and the shape *is* the boundary: in-process
over the vault, network egress, or local and mutating. `Gate::Always` means a
person approves the specific call, with its arguments shown in full — approving
something you cannot see is not approving it.

The gate is not advice. It is what the application consults before running
anything, and the only way to add a tool is to declare which shape it is.

For `gh`, the gate is decided from the *arguments*, not the tool name: reading a
pull request and merging one are the same tool. `gate_for` classifies the command
line, and anything it does not recognise is refused rather than waved through.

### Declining is a normal answer

Saying no does not error, and does not end the turn. The model is told it was
declined, and carries on with what it can. The prompt explicitly teaches it not
to re-run a declined call another way and not to ask twice — which is what turns
the approval dialog into a steering wheel rather than a stop button.

### Everything from outside is data, never instruction

Web pages, file contents, notes, documents, images, news items — all of it is
delimited and labelled as untrusted where it enters the prompt, and the
capability notes repeat the point per capability. A forum post in a news brief is
what somebody said, not an instruction to the assistant.

`fetch_url` refuses anything that is not `http` or `https` before a request is
built, so a `file:///etc/passwd` never leaves the machine. There is a test named
after exactly that.

Reading and writing are confined to the workspace, and a path outside it is
refused rather than escalated.

### Running out of room degrades, it does not fail

Tool results accumulate in every request, so long turns really do fill the
window. Rather than dying, the app empties the oldest tool results and keeps the
newest — preserving every "this call happened" record and its pairing, so the
chain still reads as *these ran, here is what came back*, and only the bulk goes.

That distinction matters after a write: the recovery that throws away the record
of a call is never allowed to run after an approved tool, because the model would
redo the side effects. The safe one is the one available when recovery matters
most.

Tools that return bulk text also have their own budgets — characters per search
result, items per brief — because the alternative is one tool call crowding out
the conversation it was supposed to inform.

## Knowing it works

A system prompt is the largest single influence on behaviour and, in most
projects, the only part with no test over it. `model/eval/` is that test.

### It scores the working, not the answer

No tool is ever executed. Every call gets an invented result shaped like the
string the real runner would have returned. What is scored is which tool the
model reached for, in what order, with what arguments, and what it said while
doing it — because tool *correctness* is already unit-tested, and what was
unproven was tool *use*.

A full pass is one process talking to one local model: no API spend, no network,
no vault, no dependence on what the weather is doing.

### Two axes, and the second one catches what you did not predict

A scenario asserts what that question should have looked like. Separately,
detectors run over every trace regardless of scenario: an identical call
repeated, malformed arguments, a tool that was never offered, arguing with a
decline, thrashing, going quiet after six tool calls, a tool call written into
prose.

That second tally is what caught the spiral. No scenario said "do not search
seventeen times" — the antipattern counter did.

### Everything is fixed except the prompt

The date is a constant, not the calendar: half the suite is about time, and a
score that drifts with the week cannot tell a regression from a Tuesday. The
memory block is a fixture. The tool results are fixtures. What moves is the
prompt. Every report records the whole prompt surface it measured, so a number
from six weeks ago still has the text attached to it.

### Sampling is real

`--repeats` samples each scenario; a scenario that passes sometimes is reported
as *flaky* rather than counted; `--baseline` diffs two reports scenario by
scenario; scores group by capability, so a change that buys documents at the cost
of the web cannot average out to nothing.

**Anything under about six samples is for finding scenarios to look at, not for
deciding whether a change helped.** A fourteen-point safety regression looked
completely real at two samples and vanished at six.

A run the server never completed is excluded rather than counted as zero, and a
capability where *nothing* completed reads as unmeasured rather than 0% — a
crashed server is not a prompt regression and the report must not let it look
like one.

### The harness is held to the same standard as the app

This is the part most easily skipped, and skipping it means measuring your own
fixtures. Five separate times, a "prompt problem" turned out to be the harness
lying:

1. **Identical results for every query.** Five differently-worded searches came
   back byte-identical — impossible against a real index — so the model escalated
   to twenty-five searches. It was behaving *correctly* given the evidence.
2. **Reads that never missed.** Every path returned plausible fresh material, so
   exploring never terminated and the model never got to the work. Fixing it
   moved `documents` from 76% to 92% **without changing a word of the prompt**.
3. **A placeholder where real content belonged.** The model read a stub that
   described a skill instead of being one, and stopped.
4. **Results that described an answer without containing one.** "A detailed
   walkthrough that answers this directly" — with no answer in it. Handed a page
   with no facts on it, the model did the reasonable thing and searched again.
5. **`example.org`.** The model spotted this before we did and said so, in a
   trace: *"Those results came back as placeholder URLs… I'm not getting real web
   results back right now."* RFC 2606 reserves those domains precisely so nothing
   mistakes them for real sites, and the model did not mistake them either — it
   concluded the tool was broken and kept hunting for the real index. That one
   detail was worth more of the score than any wording of the prompt, and it also
   explained why the model had been refusing to cite URLs it believed were fake.

The rule that came out of it: **suspect the fixtures before the prompt when a
family scores badly and the traces show thrash.** A stub has to be wrong in the
ways a real tool is wrong, not in new ones. Tests now enforce the specific traps
— no fixture may serve a page from a reserved domain, every offered tool must be
asserted about by some scenario, and no scenario may assert about a tool it did
not switch on, since a `NeverCalls` on a tool that was never offered passes for
free and would make the suite look better than it is.

### What it has actually bought

Measured against the same suite, same model, same fixed date:

| | |
|---|---|
| whole suite, stock prompt | 88% |
| after the first measured pass | 90% |
| after the web pass | 92% |
| the `web` capability alone | 72% → 93% |

And the numbers that matter more than the score: *ran out of rounds* and *used
tools and never answered* are both at zero, from eleven each.

Not every change paid. Upgrading the chat template scored 91% against 91%, and
the antipattern it was supposed to fix got worse. It was kept anyway, for a
reason the eval structurally cannot see: the older template honoured a
thinking-control token from **tool-role** messages, so a fetched web page could
switch the model's thinking off. No scenario would ever have caught that — which
is the argument for reading a changelog as well as a score.

## The short version

- Put guidance where the model reads it *at the moment of the decision*, not only
  at the top.
- When wording keeps losing, stop wording it and count it in code.
- Word a refusal as a finished state, or the model retries it.
- Never quote a phrase you want the model to avoid.
- Test the prompt, sample it properly, and group the score by capability.
- Suspect your fixtures first — five times out of five, early on, they were the
  problem.
