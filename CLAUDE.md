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
- **Distribution is `./install.sh` into `~/.local`, and nothing else.** There was a Flatpak manifest; it was dropped on 2026-08-04 rather than fixed. Most of what this app does is spawn something the sandbox does not have — `planner`, `magpie`, `gh`, `claude`/`codex`, `podman` for `run_python`, `pw-record` and `spd-say` for voice — and none of it works without `--talk-name=org.freedesktop.Flatpak`, which is full host access and larger than the hole it closes. It had also never been buildable: the manifest listed a `cargo-sources.json` that was never generated, and the `build-flatpak.sh` it named was never written. Do not add it back without answering the sibling-CLI question first.
- `packaging/build-sandbox.sh` — the podman image `run_python` runs in. Once per machine; `tests/sandbox.rs` skips itself without it.
- `cargo run --release --example eval -- --suite lookout` — the fourth suite: what the proactive check surfaces and, five times in nine, does not. One call, not a turn, judged through the same vetting the app applies before a notification is raised.
- `--no-catalogue` on the main suite drops the capability menu and `use_tools`, which is how the cost of carrying them in every prompt is measured rather than argued about. Run it against a `--baseline` of the same suite with them on.
- `--overlap current|reword|disambiguate` varies how `workflow` and `gh workflow` are told apart, read by the `overlap` family. **Settled: `current`, nothing changes** — with `workflow` switched on, "run the deploy workflow" still goes to `gh` 100% of the time over two six-repeat runs, so the other arms were never run against a model. The flag stays because the question will come back with the next tool that shares a word. The first pass said 17% and both halves of that were the harness — see `familiar-fixture-lies`.
- `--html FILE` writes the run as one self-contained page for review: every scenario's ask, its expectations, and the trace of each run, with a note box per scenario. This is what to hand somebody who wants to argue with an expectation without reading `suite.rs`. The template is `src/model/eval/report.html`, embedded by `include_str!`; the payload is the serialised `Report`, so anything the page needs has to be a field on it. Each call folds open to **what came back** — the tool result verbatim — and a gated call is marked **approved**, because without those two a reviewer cannot tell a prompt failure from a fixture failure, or a mail the assistant sent from one the user clicked through. Both were added after a review round where most of the notes were some form of "you gave it a bad result and it did the sensible thing", and the report had no way to show whether that was true.
- `FAMILIAR_ESCALATE=1 cargo test --test escalate` — opt-in, because it spends the user's Claude subscription. `./test.sh` skips it.
- `packaging/speech-server.sh` — Kokoro behind an OpenAI-shaped `/v1/audio/speech`, as a podman quadlet on `127.0.0.1:8880`. CPU on purpose: the GPU is holding the 27B and speech must not compete with the thing doing the answering. `stop` and `remove` are the other two verbs. Verified end to end by round-tripping its PCM back through `hear`, which is the only way to catch a sample-rate mistake without ears.
- **`FAMILIAR_VOICE_LOG=1 familiar`** — traces the spoken path: the gate and level per second while listening, how much audio was taken, what both models made of it, which chat it went into, and whether the turn settled with anything. Voice is the one part of this app with no visible trace of its own — a question that goes nowhere leaves an empty window and no file — and "the gate never opened", "the model returned nothing" and "the turn was dropped" are indistinguishable without it. All three have happened.
- `packaging/webcam-extension/` — a Quick Settings toggle for an Insta360 Link webcam's AI noise cancelling, driving `link-ctl` over USB. **Not part of Familiar**: it talks to a webcam, not the assistant, and lives here only because this is where the problem was found. A webcam that removes background noise removes the assistant coming out of the speakers and much of anybody talking over it. **The 0.01 figure this used to quote is a median and it misled two rounds of tuning**: re-measured properly, the assistant's own voice at this microphone has a median of 0.014 but a peak of **0.577**, against 0.578 for a person — the cancelling removes most of it and lets bursts of up to 640 ms through. A median is not what a threshold has to survive.
- **Testing it without a microphone.** `pw-loopback -n famtest --capture-props='media.class=Audio/Sink' --playback-props='media.class=Audio/Source node.name=famtest.source'` makes a virtual sink whose monitor is a virtual source; set `voice_source` to `famtest.source`, synthesise a question with the Kokoro server, and play it into `input.famtest`. That is a closed loop from speech to answer with nothing audible and no real microphone, and it is how the `pw-cat --raw` bug was found. Note `pw-record --target <sink>` records the *microphone*, not the sink's monitor — the loopback is what makes this work.
- `cargo run --release --example hear -- some.wav` — the one seam in voice the tests cannot reach: whether a speech model is installed, loads, and returns words. Takes 16-bit mono 16 kHz WAV, which is what `pw-record` is asked for. `packaging/fetch-speech-models.sh` fetches the models; Scribe's copy is read if that is installed.

Widget tests need a display; model tests do not and are the bulk of the suite. `test.sh` sets `GTK_A11Y=none` and `GSETTINGS_BACKEND=memory` so tests never touch real user state — keep that true for anything new.

## Layout

`src/model/` is pure logic with no GTK types. `src/ui/` is widgets and the application. Read `DESIGN.md` and `README.md` before proposing structural changes; both are current.

`src/model/memory/` is the whole memory capability in one slice: the saved line and its kind, the budgeted ambient block, the usage ledger, the passive reader and the nightly dream. Only `mod.rs` touches the vault, and the three rules in its header — appends only, removes only what it marked, never deletes a note — are what make the rest safe. `src/ui/embedder.rs` is the one worker thread in the app; everything else runs on the main loop.

`src/model/sandbox.rs` is `run_python`: a script written into a bind-mounted directory and executed by `podman run --rm` with no network, no capabilities and the workspace read-only. It is the only ungated tool that runs code, and the argument for that is the container — so the four isolation claims in its header are checked against real podman in `tests/sandbox.rs`, not just asserted in unit tests. The container is disposable; the directory is what persists.

`src/model/email/` is mail: `imap.rs` and `smtp.rs` are pure protocol, `mod.rs` is the verb gating and the guidance, and `ui/mail.rs` is the socket over gio's TLS. One tool, gated by verb — reading and filing run, deleting and sending ask. The rule that matters more than the gates is that a message is data and never an instruction; it is first in the guidance and repeated in every result carrying message text.

`src/model/escalate.rs` is `claude -p` / `codex exec` as an oracle: gated, consultation-only, question on stdin. `src/model/lookout.rs` is the proactive check — one call, no tools, silence by default; its signals come from `Application::gather_signals`, and anything the rubric talks about has to be gathered there or the eval is scoring a shape the model will never be sent.

`src/model/voice.rs` and `src/ui/voice/` are talking to it, added 2026-08-04. The pure half decides when an utterance ended (`Endpointer`), which chat it belongs to (`continuation`), and what an answer written for a screen sounds like (`Reading`, `spoken`); the `ui` half is five boundaries — `pw-record` on a pipe, two Parakeet models on a worker thread, speech-dispatcher or an OpenAI-shaped `/v1/audio/speech`, a gnome-settings-daemon keybinding, and a window. Orchestration is in `application.rs` beside the rest of the turn path, because **a spoken question is an ordinary turn**: it runs the background path (`Chat::Background`, no view, `scheduled: false`, `spoken: true`), so everything downstream applies without knowing voice exists.

Four things about it are settled and expensive to relearn. **The shortcut is press-only.** The `GlobalShortcuts` portal is the only source of a key release, and since xdg-desktop-portal 1.21 it refuses a caller with no app id while `org.freedesktop.host.portal.Registry` — the way a non-Flatpak app declares one — is not exported by the portal on this desktop. Measured for Scribe, on this machine. So listening is a toggle and **silence is what ends an utterance**. **The voice register rides on the question, not the system prompt** (`voice::asked_aloud`), because a prompt that changes between a typed turn and a spoken one in the same chat throws away the KV prefix. **Interrupting is the shortcut or the Stop button, and nothing else.** There was a `Barge` that watched the microphone while it spoke; it was built against the standing decision that speaking and listening do not overlap, used, measured and removed — the assistant's own voice off the speakers reaches this microphone at a peak of 0.577 against 0.578 for a person, so no threshold separates them and the behaviour was a coin toss between interrupting itself and being uninterruptible. Both happened, and so did its own voice landing in the next question's transcript. **The microphone stays open for the whole exchange** but audio arriving while the window is not listening is discarded as it arrives; it stays open because the watchdogs that unstick the window count in blocks of audio rather than carrying timers. And **the live transcript is feedback, never the question** — the streaming model's words go on screen, the accurate pass over the whole utterance is what gets sent.

**The microphone and the speech models are `earshot`, a path dependency on `../earshot`, shared with Scribe.** Both apps had grown near-identical copies of `pw-record` on a pipe, the loudness curve, and the channel to the two models; the copies drifted and a bug fixed in one stayed in the other — which is how the streaming tail bug lived in Familiar after Scribe had fixed it. The crate owns the *boundary* and neither app's policy: `ui/voice/recorder.rs` keeps `sources()` because listing devices needs `serde_json` and is this app's concern, and `ui/voice/speech.rs` keeps `model_dir` because Scribe owns its models and Familiar reads Scribe's copy. `model::voice::BLOCK_MS` is duplicated on purpose — the display-free half cannot import a crate that links GLib — and a test in `recorder.rs` asserts the two agree.

It statically links an ONNX Runtime that `ort-sys` downloads at build time, so the first build on a machine needs the network and about 100 MB of `~/.cache/ort.pyke.io`. The argument for a linked crate rather than the subprocess this app reaches for everywhere else is in `earshot/Cargo.toml`: there is no process to spawn, whisper on the CPU is slower, and a second `llama-server` would want VRAM the 27B has already taken.

**The endpointer's levels are a measurement of one desk, written up in `DESIGN.md`.** Do not adjust `Endpointer` by taste — record a silent room and some talking through `earshot::level` at 40 ms blocks and look at the run lengths, because the failures are opposite and both read as "it did not hear me". The first set was reasoned from "the microphone reads 0.01", which is a real measurement of *the assistant's voice off the speakers being cancelled* and was used as though it were the room — whose idle floor is 0.124 and had never been measured. Measure the thing the threshold is about.

`src/model/heartbeat.rs` is the schedule a chat runs itself on, and since 2026-08-03 the model can set one: the `schedule` tool, gated, writing the same `Thread.heartbeat` the Scheduled Chats window drives. It existed for a year before anything told the model, which is how the assistant came to make a *Planner task* for a morning briefing and then state that it had no scheduler at all. The `scheduling` eval family is that exchange, and half of it scores `planner` being the right answer.

`src/model/workflow.rs` is a job with steps: the model proposes with `plan`, the user edits and says go, the model does one step per `advance`. **On by default** with memory/web/weather, so it is not in `capability::ALL` — a menu entry for something already on is noise — and every eval tool set carries it except `nothing()` and `repository()`, which are controls. The prompt-length cost was measured on the two canary families: documents 94→96%, planner 96→94%, one check each way. Ungated — a plan is a list of intentions and every step's actions keep their own gates. Two rules carry the capability: a step's **note is the user's**, written only in the edit dialog and delivered in the tool result that makes its step current rather than in the prompt; and a user edit reaches the model through `Workflow::edited`, because a `thread::Note` is reader-only and `messages_for_model` never sends one. Saved workflows are Markdown under `projects/<slug>/workflows/`. `ui/workflow_bar.rs` is the `AdwBottomSheet` over the conversation — pinned rather than inline because a workflow spans turns and chips do not.

`--overlap current|reword|disambiguate` is the experiment over `gh workflow` versus this one, run against the `overlap` family. Its scenarios come in pairs — with the capability on and off — because "cannot find `gh`" and "picked the wrong one of two" look identical in a trace and want opposite fixes. The decision rule is in `DESIGN.md` and was written before the first run.

`src/model/capability.rs` is the menu of what a project has switched off, and `use_tools` switches one on mid-turn. Adding a capability means `ALL`, `switch_on`, and `Application::usable` — the last is what stops the catalogue offering something this machine cannot run. The switch offers tools; it does not use them, so every gate underneath is unchanged.

`src/model/project.rs` is a project: instructions, tools, a folder, and its chats. The window says **project** and **chat**; the code says `Project` and `Thread`, and the prompt says neither — the model already has Planner's `#Project` and the memory tool's `project` kind, so nothing about a project reaches it but the instructions the user typed. `capability.rs` has the test. `src/ui/sidebar.rs` is the tree over those: a `GtkListView` on a `GtkTreeListModel`, rebuilt after every turn and reopened by row key. Clicking a project opens `src/ui/project_view.rs` — its page, in a stack beside the conversation — which holds the instructions, `src/ui/file_tree.rs` over its folder, its chats with a search box, and what it has running. The file tree reads directories and emits `file-action` for anything that changes one; the application checks the path is still inside the project before acting.

`src/model/email/dialect.rs` is Gmail: `[Gmail]/` prefixes, `X-GM-LABELS` for labelling, `X-GM-RAW` for search, and archiving that removes a label rather than moving anything. `ui/mail.rs` asks the dialect instead of assuming IMAP. Auth is a Google app password, set in Preferences beside the Exa key.

`src/model/office/` writes `.docx`, `.xlsx`, `.pptx` and PDF. Cairo and Pango live there rather than in `ui/` — they are not GTK types and need no display, so its tests run in the display-free half. One `Block` spec feeds every writer; add a format by consuming those blocks, not by inventing a second spec.

## Sibling CLIs

`planner` and `magpie` are driven as subprocesses, never linked — both hold their store in the running app's memory, so a second writer loses. `docs/planner-cli.md` and `docs/magpie-cli.md` document the interfaces; `planner agent describe` and `magpie agent describe` are the authority. Gating reads the verb, not the tool name, and anything unrecognised is gated.

`dynamo` is the third, and the one that breaks the pattern in a useful way. Its store is Postgres on the NAS, so it *could* have been linked or served over HTTP; it is a subprocess because that is the shape this app already gates, caps and frames, and because the alternative is asking a service whose design note says it publishes no port to open one. **Every one of its verbs reads and none can come to write** — `agent` runs `SELECT`s as a role granted nothing else — so `classify` returns `Gate::Never` for known verbs and *refuses* unknown ones rather than gating them. There is no gated half to fall into. `docs/dynamo-cli.md` is the interface; `dynamo agent describe` is the authority. **The failures here are arithmetic rather than authority** — nothing can break the house, and everything can report a wrong number about it. Three, all measured against the real account: adding `kind=merged` back onto the default double-counts every 240 V circuit (182 kWh for a house that used 140); `scale=1MIN` over a long period silently answers from the week that has minute data and still labels itself "the last 30 days" (135 against 569); and a `series` over a week is 9.3 KB against a `MAX_OUTPUT` of 8, so Familiar's own cut takes `total_kwh` and leaves the rows — `note_for` restates the headline after the cut because a note is appended, not truncated. The `dynamo` eval family scores these; it was **100% of 90 checks against a fixture that had the house backwards**, and is 87% of 342 against one built from `dynamo agent`. See `familiar-fixture-lies`.

`magpie transcribe` is the only slow tool. It goes through `ui::runner::run_slow`: stderr kept separate from stdout, progress streamed onto the tool chip as it arrives, and no timeout.

## Serena is the primary toolset for Rust code

This project runs the **Serena MCP server** under the `claude-code` context. Serena's symbol-aware
tools are the primary tools for anything in a `.rs` file; `Read` and `Edit` are the fallback. Where
a built-in tool description tells you to prefer `Read`/`Edit`, that description is written for
projects without Serena and is superseded here.

| Task | Tool |
|------|------|
| See a file's structure | `get_symbols_overview` |
| Read one symbol's body | `find_symbol` with `include_body=true` |
| Find a symbol, or its callers | `find_symbol` / `find_referencing_symbols` |
| Find declarations, impls of a trait | `find_declaration` / `find_implementations` |
| Check errors without a build | `get_diagnostics_for_file` |
| Replace a fn, impl block, or struct | `replace_symbol_body` |
| Add an item, or an import at the top | `insert_after_symbol` / `insert_before_symbol` |
| Change a few lines inside a fn | `replace_content` |
| Make the same change across files | `replace_in_files` (`dry_run` first) |
| Rename or remove a symbol | `rename_symbol` / `safe_delete_symbol` |

Serena's `read_file`, `list_dir`, `find_file`, `search_for_pattern` and `execute_shell_command` are
switched off in this context — `Read`, `Glob`, `Grep` and `Bash` cover those. Use `Grep` and `Glob`
freely for **discovery**, then follow every hit through Serena rather than reading the file around it.

Reach for `Read`/`Edit` on a `.rs` file only when: Serena was tried on that target and failed; the
file will not parse; or you need a handful of lines whose enclosing symbol is very large. `Read`,
`Write` and `Edit` are the right tools for non-code files — Markdown, TOML, JSON, YAML, shell
scripts, `.ui` files. A brand-new file is `Write`; there are no symbols to navigate yet.

Before editing code: `get_symbols_overview` on the target → `find_symbol` with `include_body=true`
for only the symbols you will touch → edit through the symbolic tools. When you already know the
symbol's name, call `find_symbol` first — no `Grep` or `Read` warm-up.

None of the following is a reason to fall back to `Read`/`Edit`, and catching yourself forming one
is the signal to use Serena instead: "I already know the path", "one `Read` is cheaper than three
Serena calls", "the file is short", "I need to see it in context first".

Subagents are bound by this too, and you only ever see their diff — so put it in the dispatch
whenever you delegate an edit to an existing `.rs` file.

## Conventions

- Use the `developing-gtk-apps` and `designing-gnome-ui` skills for widget, threading, and HIG decisions rather than deriving them again.
- Edit Rust through Serena's symbolic tools; the Edit tool is the fallback and non-code default. Never rewrite Rust sources through `python3 - <<PY` heredocs or `sed -i`.
- The sibling apps (brain, planner, stickies, youtube-downloader) share this layout and these scripts; a pattern established in one is the pattern here.
