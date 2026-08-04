# Planner's agent CLI

[Planner](https://github.com/mhagrelius/planner) is the sibling task app. It
exposes `planner agent <verb>`, a JSON interface for reading and changing tasks
from outside its window. This is what it does and how Familiar should drive it.

Nothing here is wired up yet. This documents the interface so the tool can be
added deliberately rather than guessed at.

## Why a subprocess and not a crate

Familiar depends on `brain::model` for the vault, so a `planner::model`
dependency would be the obvious echo of that. It would be wrong here.

Brain's vault is Markdown files: two processes can read and write them
independently because each note is its own file and edits do not overlap.
Planner's store is one JSON document that the running app holds **entirely in
memory** and flushes on a two-second tick. A second process writing that file
is overwritten by the app's next save, silently, within two seconds.

`planner agent` avoids this by riding Planner's own command line. Planner sets
`HANDLES_COMMAND_LINE`, so when the app is running the invocation is forwarded
to it over D-Bus, and the *running* instance answers, mutates its own store,
saves, and redraws. When it is not running, the invoked process becomes the
primary instance and does the work itself. Either way there is exactly one
writer.

So: spawn `planner`, do not link it.

## Running it

The same shape as `gh` in `src/model/github.rs` — an argument list handed to
`execvp`, no shell:

```
planner agent list due: today | overdue
```

is the argv `["planner", "agent", "list", "due:", "today", "|", "overdue"]`.
There is no shell, so `|` here is a literal argument that Planner's query
parser reads as its own or-operator, and `;`, `$(…)` and `&&` are inert
strings. Keep it that way.

`planner` must be on `PATH`; its `install.sh` puts it in `~/.local/bin`.

**Output.** One JSON object on stdout, nothing on stderr, exit 0. On failure,
a JSON object with `"ok": false` and exit 1. `help` is the one exception: it
prints text, because it is meant to be read.

**Arguments are positional words and `key=value` pairs. There are no
`--flags`** — GOption parses the command line before Planner's own code runs
and rejects any option it was not told about in advance. Do not invent flags;
they will be refused by the launcher, not by the verb.

## The verbs

Read `planner agent help` for the authoritative list, and `planner agent
describe` for the same thing as JSON — name, usage, arguments, what it returns,
and a `mutates` boolean. **`describe` is the right source for a tool
definition**: generating from it means Planner gaining a verb does not require
an edit here.

| Verb | | |
|---|---|---|
| `overview` | reads | projects, sections, labels, saved filters, counts |
| `list [query] [limit=N]` | reads | tasks matching a filter query |
| `show <task>` | reads | one task with description, reminders, subtasks |
| `search <text>` | reads | tasks, projects and labels by substring |
| `add <line>` | writes | a task from a quick-add line |
| `subtask <parent> <line>` | writes | a task under another |
| `complete <task>` | writes | tick off |
| `reopen <task>` | writes | un-tick |
| `delete <task>` | writes | delete, with subtasks |
| `update <task> <field=value>…` | writes | change fields |
| `add-project <name> [parent=]` | writes | |
| `rename-project <project> <name>` | writes | |
| `remove-project <project>` | writes | deletes every task in it |

Sections cannot be created from here. `update` files a task into one that
exists, and `overview` lists them.

## Gating

`mutates` in `describe` maps onto `Gate`:

- `Gate::Never` — `overview`, `list`, `show`, `search`, `help`, `describe`.
  These only read, and are as safe as `read_file`.
- `Gate::Always` — everything else. These change the user's task list.

Do not gate on the verb name in a list maintained here; read `mutates`. If it
cannot be read for some reason, gate. `remove-project` deserves particular
care: it deletes the project's subprojects and every task in all of them, and
Planner has no undo reachable from the CLI. The response says how much went,
which is a poor substitute for asking first.

## The two languages

Planner deliberately does not offer a separate set of fields. Creating uses the
same quick-add line the app's dialog parses; listing uses the same filter query
its sidebar runs. `planner agent help` prints both in full. In brief:

**Quick-add line** — tokens anywhere in the line, stripped from the title:

```
Email Sam about the lease #Work /Admin @email p2 friday 9am !30m
```

`#Project` `/Section` `@label` `p1`–`p4` `!30m`, plus dates (`today`,
`tomorrow`, `next friday`, `27th`, `in 3 days`, `end of month`, `9am`) and
repeats (`every other monday`, `every 3 weeks`, `every weekday`, `every! 3
days` — the `!` form repeats from the day you complete it).

**Filter query** — `&`, `|`, `!` and parentheses over terms:

```
due: today | overdue          #Work & p1          ##Home & @errand & !p4
overdue     no date     pinned     recurring     completed     p1
```

Completed tasks are excluded unless the query says `completed`.

## Things that will bite

**A repeating task is not finished by being completed.** `complete` returns
`"outcome": "completed-and-repeats"` with a `next_due` date instead of
`"done"`.

```json
{"ok": true, "action": "completed", "task": {…},
 "outcome": "completed-and-repeats", "next_due": "2026-08-06"}
```

The completion **succeeded** — `ok` is true and the action is `completed`. The
task is open again on a later date, which is why the embedded task has no
`completed` field. Say both: "done, and it's back on the 6th." Reporting it as
a reschedule ("I moved that to Thursday") tells the user the opposite of what
they asked for; reporting it as plain `done` leaves them thinking a task is
finished when it is still in their list.

**Ambiguity is an error, not a guess.** A task reference matching more than one
open task returns `"error": "ambiguous"` with a `candidates` array of ids,
titles and context. Ask the user, or pass an id. Ids come back on every task in
every response, so prefer passing the id you were already given over re-naming
a task by its title.

**A `#Project` that does not exist is not created.** The task lands in the
Inbox instead. Check `project` in the response rather than assuming the line
was understood — this is where a misspelling shows up. Labels are the opposite:
`@label` creates the label if it is new.

**`add` returns the task as it was actually parsed.** The date and project came
from reading prose, so read the response back rather than restating what you
asked for. There is no need to call `show` afterwards.

**`update` reports what changed.** `applied` lists the fields that actually
moved; setting a field to the value it already had leaves it empty. That is a
no-op, not a failure.

**Lists are capped at 50 and say so.** `count` is what came back, `matched` is
how many there were, and `truncated` is `true` when they differ. Nothing is
dropped silently. Raise it with `limit=N` when you genuinely need more, but a
context window is the reason the default is not higher.

**Errors are structured.** `error` is a stable kebab-case kind to branch on —
`not-found`, `ambiguous`, `bad-query`, `bad-date`, `unknown-verb`,
`unknown-field`, `bad-value`, `read-only`, `refused` — and `message` is a
sentence to relay. Many carry `hint`, which usually says exactly what to do
next.

## A worked sequence

```
planner agent overview
  → the projects and labels that exist, so a later #Project is a real one

planner agent list due: today | overdue
  → what the user is being asked about

planner agent add Ring the plumber #Home @errand p1 tomorrow
  → check task.project is "Home" and task.due is the date you meant

planner agent complete 'Ring the plumber'
  → check outcome; "completed-and-repeats" means it is back on next_due
```
