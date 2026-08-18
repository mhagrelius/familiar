# Dynamo's agent CLI

[Dynamo](../../dynamo) is the sibling that reads three Siemens Inhab energy
monitors out of Emporia's cloud into Postgres on the NAS. It exposes
`dynamo agent <verb>`, a **read-only** JSON interface. This is what it does and
how Familiar drives it.

## Why a subprocess, and why it is easier than the other two

Planner and Magpie are subprocesses because they *have* to be: each holds its
store in the memory of the running app, so a second writer loses. Dynamo has no
such problem — its store is Postgres, which is built for concurrent readers.

It is a subprocess anyway, for two better reasons. It is the shape Familiar
already gates, spawns, caps and frames, so nothing new is needed on this side.
And the alternative — Dynamo serving HTTP — would mean opening a port on a
service whose design note says, in as many words, that it publishes none. A CLI
keeps that true.

So: spawn `dynamo`, do not link it, and do not ask it for a socket.

## Everything reads

**This is the first sibling tool where no verb can change anything.** Planner's
gating asks whether a verb mutates and gates the unknown ones because the answer
might be yes; here the answer is no and cannot become yes. `agent` runs
`SELECT`s as a Postgres role that has been granted nothing else — the grant is
what makes it a fact rather than a promise — and the writes Dynamo does perform
come from a collector loop running in a container on the NAS, which `agent`
cannot reach.

So `classify` returns `Gate::Never` for every verb it knows, and refuses the
ones it does not rather than gating them. A verb Dynamo gains later costs a
"that is not a verb" and a second call, not an unreviewed write.

## Running it

The same shape as `planner`: an argument list handed to `execvp`, no shell.

```
dynamo agent usage yesterday kind=circuits
```

is the argv `["dynamo", "agent", "usage", "yesterday", "kind=circuits"]`.

`dynamo` must be on `PATH`; its `install.sh` puts it in `~/.local/bin`. It reads
`~/.config/dynamo/config.json` for the database — host, port, database, user,
password — and the user there should be the read-only role.

**No `--flags`.** Arguments are positional words and `key=value` pairs. Dynamo
has no GOption launcher to placate; the rule is kept so that all three sibling
CLIs answer to the same grammar and a model does not have to remember which is
which.

## The verbs

`dynamo agent describe` is the authority, and prints its own list. As of
2026-08-17:

| Verb | Arguments | Answers |
| --- | --- | --- |
| `describe` | — | this |
| `channels` | — | every measured circuit, its name and which monitor it is on |
| `now` | — | what each circuit is drawing, in watts |
| `usage` | `<period> [kind=…]` | energy by circuit over a period, kWh, biggest first |
| `series` | `<circuit> <period> [scale=…]` | one circuit's readings over a period |

Periods are `today`, `yesterday`, `week`, `month`, `year`, `all`, **and there is
nothing else**. `usage july` and `usage 2026-07` are refused with the list; there
is no calendar month and no date range. `month` means the last 30 days and
`year` the last 365, counted back from now. A day is a **calendar** day:
"yesterday" is midnight to midnight, not the twenty-four hours before now.

`scale=` takes `1MIN`, `15MIN`, `1H` or `1D` and refuses anything else — a real
run wrote `scale=1M` and got a `bad-request` back saying so.

### Asking for minutes over a long period gets you a week

**Minute readings are kept for about a week**; hourly and daily reach back to
January 2025. Dynamo does not refuse a mismatch, and it does not relabel the
answer either:

```
series "Water Heater" month            → 568.941 kWh, 1H, from 20 July
series "Water Heater" month scale=1MIN → 135.449 kWh, 1MIN, from 11 August
```

Both call themselves `"period": "the last 30 days"`. The second is four times
too small and the only thing in the response that says so is the first
timestamp — which lives inside a `points` array long enough to be truncated.
Leave `scale` off unless the period is short.

## The thing that will produce a wrong answer

**A merged channel is the sum of two branch legs**, because a 240 V circuit is
wired across both. The dryer exists as legs `11` and `12` *and* as merged
channel `101`, "Clothes Dryer". Adding merged and branch figures together
counts every large appliance twice and inflates the house by however much they
draw.

`kind=circuits` is the default and is the safe set: merged channels, plus the
branch legs that belong to no merge. That is every circuit exactly once.

The four kinds, measured on one day (31 July 2026):

| `kind` | Total | What it is |
| --- | --- | --- |
| `circuits` (default) | 140.738 | every circuit exactly once |
| `branch` | 140.738 | the same energy, reached by summing legs |
| `merged` | 41.636 | the 240 V circuits alone |
| `main` | 140.330 | one monitor's mains CTs |

**`branch` and `circuits` come to the same number**, because a merged channel
*is* its legs — so asking for branch is not the mistake. The mistake is adding
`merged` to either, which gives 182 kWh for a house that used 140. And `main`
is close enough to the house to read as it while covering one panel of three.

A `usage` list also **leaves out any circuit that used nothing** — 27 rows for
40 circuits on that day. Absence means zero, not unmonitored; `channels` is
what settles which.

This is the note the guidance leads with, because it is the one way to read
this data confidently and wrongly.

## Response shape

JSON on stdout, always, including for refusals — `{"ok": false, "error": …,
"message": …}` with exit status 0. A model that is told "the tool failed" when
the truth is "no circuit is called that, here is how to list them" will report
the wrong thing.

**Keys come back alphabetically**, which matters more than it should. In a
`series` answer `points` sorts before `resolution`, `total_kwh` and `truncated`,
so anything long enough to hit Familiar's own 8,000-character cap loses every
figure and keeps every row. `dynamo agent series "Water Heater" week` is 9,355
bytes, so this is an ordinary question rather than an edge case, and
`dynamo::note_for` restates the headline after the cut for exactly that reason.

`usage` and `series` carry `matched`, `count` and `truncated`, the same fields
Planner uses, so the same page-not-the-whole-list note applies — with one
difference worth stating: **`total_kwh` is the whole period even when the rows
are a page.** It is the shape over time that is incomplete, not the total.

`series` answers `{"ok": false, "error": "ambiguous", "candidates": […]}` when a
name matches more than one circuit. **This is a partial match, not a duplicate
name** — no two circuits in this house share one. Asking for "geothermal" gets
eight candidates and asking for "kitchen" gets six, and the shape of the list is
the part that matters: a 240 V circuit appears three times under one name, once
as the merged channel and once for each leg. A leg is exactly half the circuit,
so guessing one is a wrong answer that survives a sanity check.
