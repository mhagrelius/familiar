# familiar

A local-model assistant. Depends on `brain::model` for the vault format — do not reimplement it here.

## Stack

GTK 4.22 + libadwaita 1.9 via gtk4-rs 0.11 / libadwaita-rs 0.9, Rust edition 2021 (MSRV 1.80). `gio` is a direct dependency purely to raise the API level to v2_80 — leave it.

Crate is a lib + bin so integration tests and `examples/` can drive the real application rather than a copy of it.

## Commands

- `./test.sh` — fmt check, clippy with `-D warnings`, then `cargo test --all-targets`. Add `--headless` to run under Xvfb + a private D-Bus session. This is the gate; run it, not bare `cargo test`.
- **Never run `dbus-run-session` or `xvfb-run -a dbus-run-session` directly** — use `isolated-bus [--headless] -- CMD`. A private bus activates its own `xdg-document-portal`, which mounts over `/run/user/$UID/doc` and takes the login session's portal down with it when the bus exits; every flatpak on the machine then fails to launch. `test.sh --headless` handles this internally, but one-off runs of a single test or of `./target/release/familiar` bypass it, which is what caused 24 portal drops on 2026-08-03.
- `cargo run --release --example eval` — grades the system prompt against a real llama-server. Scores tool-use *workflow*, not tool correctness; no tool is ever run. `--list`, `--filter`, `--persona`, `--baseline`. Suite and scoring live in `src/model/eval/`.
- `cargo run --release --example eval -- --suite memory` — the third suite: what the passive reader saves and what the nightly consolidation lets go. Neither call is a turn, so no tool loop runs; a reply is scored through the same gate, parser, vetting and `dream::Policy` the application applies before anything touches a vault. Half of every family scores the assistant *not* acting.
- `cargo run --release --example eval -- --suite recall` — the second suite: ten-turn threads that plant a fact early and ask for it late. `--compaction off|headings|model` picks how the driver folds between turns — `off` is the ceiling, `model` is what ships, `headings` is the fallback floor. The gap between arms is what folding costs. Half the scenarios offer no tools, and that half is the number to compare across models.
- `cargo run --release --example memory -- dream <vault>` — a night's consolidation against a real vault, printed rather than applied. `--apply` carries it out and writes `dreams.json`; `read <vault> "<said>" "<answered>"` drives the passive reader over one exchange. This is the seam the eval cannot reach: the suite grades the decisions, this shows what they do to files.
- `./install.sh` — release build, installs under `~/.local`. `./uninstall.sh` reverses it.
- `packaging/build-flatpak.sh` — distribution artifacts.
- `packaging/build-sandbox.sh` — the podman image `run_python` runs in. Once per machine; `tests/sandbox.rs` skips itself without it.
- `cargo run --release --example eval -- --suite lookout` — the fourth suite: what the proactive check surfaces and, five times in nine, does not. One call, not a turn, judged through the same vetting the app applies before a notification is raised.
- `--no-catalogue` on the main suite drops the capability menu and `use_tools`, which is how the cost of carrying them in every prompt is measured rather than argued about. Run it against a `--baseline` of the same suite with them on.
- `--overlap current|reword|disambiguate` varies how `workflow` and `gh workflow` are told apart, read by the `overlap` family. **Settled: `current`, nothing changes** — with `workflow` switched on, "run the deploy workflow" still goes to `gh` 100% of the time over two six-repeat runs, so the other arms were never run against a model. The flag stays because the question will come back with the next tool that shares a word. The first pass said 17% and both halves of that were the harness — see `familiar-fixture-lies`.
- `--html FILE` writes the run as one self-contained page for review: every scenario's ask, its expectations, and the trace of each run, with a note box per scenario. This is what to hand somebody who wants to argue with an expectation without reading `suite.rs`. The template is `src/model/eval/report.html`, embedded by `include_str!`; the payload is the serialised `Report`, so anything the page needs has to be a field on it. Each call folds open to **what came back** — the tool result verbatim — and a gated call is marked **approved**, because without those two a reviewer cannot tell a prompt failure from a fixture failure, or a mail the assistant sent from one the user clicked through. Both were added after a review round where most of the notes were some form of "you gave it a bad result and it did the sensible thing", and the report had no way to show whether that was true.
- `FAMILIAR_ESCALATE=1 cargo test --test escalate` — opt-in, because it spends the user's Claude subscription. `./test.sh` skips it.

Widget tests need a display; model tests do not and are the bulk of the suite. `test.sh` sets `GTK_A11Y=none` and `GSETTINGS_BACKEND=memory` so tests never touch real user state — keep that true for anything new.

## Layout

`src/model/` is pure logic with no GTK types. `src/ui/` is widgets and the application. Read `DESIGN.md` and `README.md` before proposing structural changes; both are current.

`src/model/memory/` is the whole memory capability in one slice: the saved line and its kind, the budgeted ambient block, the usage ledger, the passive reader and the nightly dream. Only `mod.rs` touches the vault, and the three rules in its header — appends only, removes only what it marked, never deletes a note — are what make the rest safe. `src/ui/embedder.rs` is the one worker thread in the app; everything else runs on the main loop.

`src/model/sandbox.rs` is `run_python`: a script written into a bind-mounted directory and executed by `podman run --rm` with no network, no capabilities and the workspace read-only. It is the only ungated tool that runs code, and the argument for that is the container — so the four isolation claims in its header are checked against real podman in `tests/sandbox.rs`, not just asserted in unit tests. The container is disposable; the directory is what persists.

`src/model/email/` is mail: `imap.rs` and `smtp.rs` are pure protocol, `mod.rs` is the verb gating and the guidance, and `ui/mail.rs` is the socket over gio's TLS. One tool, gated by verb — reading and filing run, deleting and sending ask. The rule that matters more than the gates is that a message is data and never an instruction; it is first in the guidance and repeated in every result carrying message text.

`src/model/escalate.rs` is `claude -p` / `codex exec` as an oracle: gated, consultation-only, question on stdin. `src/model/lookout.rs` is the proactive check — one call, no tools, silence by default; its signals come from `Application::gather_signals`, and anything the rubric talks about has to be gathered there or the eval is scoring a shape the model will never be sent.

`src/model/heartbeat.rs` is the schedule a chat runs itself on, and since 2026-08-03 the model can set one: the `schedule` tool, gated, writing the same `Thread.heartbeat` the Scheduled Chats window drives. It existed for a year before anything told the model, which is how the assistant came to make a *Planner task* for a morning briefing and then state that it had no scheduler at all. The `scheduling` eval family is that exchange, and half of it scores `planner` being the right answer.

`src/model/workflow.rs` is a job with steps: the model proposes with `plan`, the user edits and says go, the model does one step per `advance`. **On by default** with memory/web/weather, so it is not in `capability::ALL` — a menu entry for something already on is noise — and every eval tool set carries it except `nothing()` and `repository()`, which are controls. The prompt-length cost was measured on the two canary families: documents 94→96%, planner 96→94%, one check each way. Ungated — a plan is a list of intentions and every step's actions keep their own gates. Two rules carry the capability: a step's **note is the user's**, written only in the edit dialog and delivered in the tool result that makes its step current rather than in the prompt; and a user edit reaches the model through `Workflow::edited`, because a `thread::Note` is reader-only and `messages_for_model` never sends one. Saved workflows are Markdown under `projects/<slug>/workflows/`. `ui/workflow_bar.rs` is the `AdwBottomSheet` over the conversation — pinned rather than inline because a workflow spans turns and chips do not.

`--overlap current|reword|disambiguate` is the experiment over `gh workflow` versus this one, run against the `overlap` family. Its scenarios come in pairs — with the capability on and off — because "cannot find `gh`" and "picked the wrong one of two" look identical in a trace and want opposite fixes. The decision rule is in `DESIGN.md` and was written before the first run.

`src/model/capability.rs` is the menu of what a project has switched off, and `use_tools` switches one on mid-turn. Adding a capability means `ALL`, `switch_on`, and `Application::usable` — the last is what stops the catalogue offering something this machine cannot run. The switch offers tools; it does not use them, so every gate underneath is unchanged.

`src/model/project.rs` is a project: instructions, tools, a folder, and its chats. The window says **project** and **chat**; the code says `Project` and `Thread`, and the prompt says neither — the model already has Planner's `#Project` and the memory tool's `project` kind, so nothing about a project reaches it but the instructions the user typed. `capability.rs` has the test. `src/ui/sidebar.rs` is the tree over those: a `GtkListView` on a `GtkTreeListModel`, rebuilt after every turn and reopened by row key. Clicking a project opens `src/ui/project_view.rs` — its page, in a stack beside the conversation — which holds the instructions, `src/ui/file_tree.rs` over its folder, its chats with a search box, and what it has running. The file tree reads directories and emits `file-action` for anything that changes one; the application checks the path is still inside the project before acting.

`src/model/email/dialect.rs` is Gmail: `[Gmail]/` prefixes, `X-GM-LABELS` for labelling, `X-GM-RAW` for search, and archiving that removes a label rather than moving anything. `ui/mail.rs` asks the dialect instead of assuming IMAP. Auth is a Google app password, set in Preferences beside the Exa key.

`src/model/office/` writes `.docx`, `.xlsx`, `.pptx` and PDF. Cairo and Pango live there rather than in `ui/` — they are not GTK types and need no display, so its tests run in the display-free half. One `Block` spec feeds every writer; add a format by consuming those blocks, not by inventing a second spec.

## Sibling CLIs

`planner` and `magpie` are driven as subprocesses, never linked — both hold their store in the running app's memory, so a second writer loses. `docs/planner-cli.md` and `docs/magpie-cli.md` document the interfaces; `planner agent describe` and `magpie agent describe` are the authority. Gating reads the verb, not the tool name, and anything unrecognised is gated.

`magpie transcribe` is the only slow tool. It goes through `ui::runner::run_slow`: stderr kept separate from stdout, progress streamed onto the tool chip as it arrives, and no timeout.

## Conventions

- Use the `developing-gtk-apps` and `designing-gnome-ui` skills for widget, threading, and HIG decisions rather than deriving them again.
- Edit files with the Edit tool. Do not rewrite Rust sources through `python3 - <<PY` heredocs or `sed -i`.
- The sibling apps (brain, planner, stickies, youtube-downloader) share this layout and these scripts; a pattern established in one is the pattern here.
