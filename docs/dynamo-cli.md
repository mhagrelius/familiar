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

Periods are `today`, `yesterday`, `week`, `month`, `year`, `all`. A day is a
**calendar** day: "yesterday" is midnight to midnight, not the twenty-four hours
before now.

## The thing that will produce a wrong answer

**A merged channel is the sum of two branch legs**, because a 240 V circuit is
wired across both. The dryer exists as legs `11` and `12` *and* as merged
channel `101`, "Clothes Dryer". Adding merged and branch figures together
counts every large appliance twice and inflates the house by however much they
draw.

`kind=circuits` is the default and is the safe set: merged channels, plus the
branch legs that belong to no merge. That is every circuit exactly once.
`kind=branch` and `kind=merged` exist for questions that genuinely want one
side, and `kind=main` is the mains CTs — which only **one of the three
monitors** has, so a "whole house" total from it covers that panel alone.

This is the note the guidance leads with, because it is the one way to read
this data confidently and wrongly.

## Response shape

JSON on stdout, always, including for refusals — `{"ok": false, "error": …,
"message": …}` with exit status 0. A model that is told "the tool failed" when
the truth is "no circuit is called that, here is how to list them" will report
the wrong thing.

`usage` and `series` carry `matched`, `count` and `truncated`, the same fields
Planner uses, so the same page-not-the-whole-list note applies. `series` answers
`{"ok": false, "error": "ambiguous", "candidates": […]}` when a name matches
more than one circuit, which happens: two channels on different monitors are
deliberately named the same thing.
