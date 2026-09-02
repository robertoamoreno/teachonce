# TeachOnce

**Teach it once. Record yourself doing a task, answer a few questions about it, and hand your agent the skill.**

By Roberto Moreno · [github.com/robertoamoreno/teachonce](https://github.com/robertoamoreno/teachonce)

TeachOnce began as a Rust + Tauri reimplementation of the idea behind
[microsoft/skill-recorder](https://github.com/microsoft/skill-recorder), with one
structural change: **everything runs locally except the analysis step, and that
step points at any OpenAI-compatible endpoint you configure.**

```
capture ──► local reconstruction ──► [ your endpoint ] ──► review ──► SKILL.md
  100% local        100% local          the only hop        you       local
```

## The privacy boundary

Two library crates can open a network connection: `skillrec-agent`, the analysis
endpoint, and `skillrec-narration`, which downloads Whisper weights and, only if
you turn it on, uploads narration to a hosted transcriber. Screen capture, window
tracking, clipboard and frame selection live in crates that have no HTTP client
compiled into them at all. That is not a policy statement — it is checkable:

```bash
cargo tree -p skillrec-capture   | grep -c reqwest   # 0
cargo tree -p skillrec-narration | grep -c reqwest   # 1, Whisper weights + optional hosted transcription
cargo tree -p skillrec-agent     | grep -c reqwest   # 1, the analysis endpoint
```

Nothing is sent anywhere until you press **Analyse**, and then only the timeline,
the events, the narration text, and any screen frames the model explicitly asks
to look at. Two other outbound paths exist and both are off until you configure
them in Settings: hosted transcription uploads narration audio when you press
**Transcribe**, and a TeachOnce server receives a whole recording when you press
**Submit**.

## Configuring the model

Settings → Model endpoint. Anything speaking the OpenAI chat-completions API:

| Server | Base URL | Notes |
|---|---|---|
| Ollama | `http://localhost:11434/v1` | Fully local. The default (`qwen3:8b`) has tools but no vision. |
| LM Studio | `http://localhost:1234/v1` | Fully local. |
| llama.cpp | `http://localhost:8080/v1` | No `/models` route; the connection test handles that. |
| OpenAI | `https://api.openai.com/v1` | Needs a real key. |
| vLLM / OpenRouter / … | your URL | Same contract. |

**Reasoning.** Thinking models such as `qwen3` spend most of a turn reasoning before
they answer; on a laptop that is the difference between a five-second turn and a
three-minute one. Settings → Reasoning → **None** sends `reasoning_effort: none`,
which Ollama honours. A server that rejects the field is detected and it is dropped
for the rest of the session, so the setting is safe to leave on. The same goes
for `temperature`: analysis turns ask for 0.1, and a reasoning model that only
takes its default (gpt-5, the o-series, behind OpenAI or LiteLLM) refuses that
once, after which the client sends none.

Turn off **"This model can see images"** for a text-only model. The describer then
never offers itself the frame tools and works from events and narration alone,
rather than burning turns on calls the server will reject.

**Tools versus vision.** On Ollama these are often mutually exclusive: `qwen3:8b`
supports tool calling but cannot see, and `qwen2.5vl:7b` can see but hard-400s
any request carrying `tools`. When a server rejects tools outright the agent
detects it, re-sends without the field, and puts the tool schemas in the prompt
instead — slower and less reliable, but it works. The default favours the
tool-capable model, since most steps are explained by events and narration
without ever needing a frame.

The client is written to be permissive about how servers actually behave — tool
calls with no `id`, `arguments` as an object instead of a JSON string, ignored
`tool_choice`, models that answer in prose instead of calling the tool. See
`crates/agent/tests/agent_loop.rs`, which reproduces each of those on the wire.

## How it works

**1. Record** — ⌘⇧R from anywhere, the tray, or the button. Collectors run on
their own threads at intervals chosen from what each signal costs:

| Signal | Interval | Permission |
|---|---|---|
| App switches, window titles | 1000 ms (1600 ms in a browser) | Screen Recording |
| Browser URL (AppleScript) | on change, ≥1500 ms apart | Automation, per browser |
| Clipboard | 700 ms | none |
| Screen stills (one display, chosen in Settings) | 1000 ms | Screen Recording |
| Microphone | continuous, toggleable | Microphone |

**No video is recorded.** Screen stills are hashed with a difference hash and kept
only when the screen actually changed, or every 5 s as a heartbeat. A ten-minute
recording is typically a few dozen JPEGs — no encoder, no container, no ffmpeg,
and nothing to decode later. Every retained still is visible in the library, so
you can see exactly what a model could be shown before you analyse.

**One display.** Stills come from the primary display unless Settings → What to
capture → Display names another. The choice is stored by the display's name as
System Settings shows it, so it survives replugging and reboots (macOS hands out
a new display id every time). If the chosen display is not connected when a
recording starts, the primary display stands in and the log says so; the chosen
one is picked back up within ten seconds of being plugged in.

**2. Reconstruct** — on stop, the event stream is segmented into ordered steps
(a new step when you change app, or change host inside a browser) and written as
`bundle.json` + `description.md`. No model involved. This is what the describer's
`get_timeline` tool returns, and the fallback if you never configure an endpoint.

**3. Transcribe** *(if you narrated)* — whisper.cpp with Metal acceleration, on
this machine, by default. Analysis refuses to run on untranscribed narration: your
own words are the clearest statement of intent in the recording.

Settings → Narration → **Transcribe with** can instead point at a hosted service
speaking the OpenAI transcription API (OpenAI `whisper-1`, Groq
`whisper-large-v3-turbo`, or a self-hosted server). That is the one path on which
narration audio leaves the machine, it is off by default, and the Transcribe button
says where the audio will go before you press it. Audio is re-encoded as 16-bit WAV
and uploaded in five-minute parts, each stamped back onto the session clock.

**4. Analyse** — a tool-calling agent reconstructs your intent and the ordered
steps. It reads the timeline, then the narration, then events where something is
unclear, and looks at frames *only* where events leave real ambiguity. You review
it, edit it directly, or send natural-language feedback for another pass.

**Debrief** — a recording shows one run of the happy path. Right after analysis
a second pass asks you up to five questions it cannot answer from the evidence:
what happens when a step fails, why you chose one option, what varies from run
to run, what must already be true, how you know it is done, and what an
unexplained specific is for. Answer in a sentence or skip. Your answers are
stored with the analysis, survive a re-analysis, and the builder treats them as
facts: exceptions become explicit handling, decisions become rules, variables
become inputs.

**5. Build** — two phases with you in between. The model proposes a plan: how it
generalizes your single run, the fixed values it hard-codes (as `{{token}}`
fields you can edit), and the ordered steps, each tagged `calculation` or
`action`. You approve — or ask for changes in plain language and get a revised
plan — and it writes the `SKILL.md`. Your edited values win — the model does not
get a second say.

**Pages are facts, not paraphrase.** Every address the recording visited is
stamped onto the analysis steps by time, straight from the events, and shown
under each step. When the plan comes back, any visited page it neither pins as a
value nor mentions is listed under **Visited, but not in the plan** with an
*Add as value* button. The planner still decides what is fixed and what varies
per run — a chat page with an id in it is not a fixed value — but nothing you
visited can drop out of the skill without you seeing it.

## Layout

```
crates/core         events, session store, timeline, pages      73 tests
crates/capture      screen, window, clipboard, audio (macOS)    43 tests
crates/narration    whisper.cpp + hosted transcription          15 tests
crates/agent        client, agent loop, describer, debrief     54 + 14 tests
crates/recorder     the lifecycle state machine                  9 tests
crates/server       HTTP API, upload, pipeline, embedded UI      9 tests
src-tauri           commands, tray, hotkey, server submit        3 tests (one against a live server, ignored by default)
ui                  React 19
```

The Tauri layer is a thin adapter over the library crates, so the pipeline is
testable without a window and a CLI could be built on the same crates.

## Running it

Requires Rust 1.90+, Node 20+, Xcode command line tools and cmake (for
whisper.cpp), on macOS 13+.

```bash
npm install
npm run tauri dev      # or: npm run tauri build
```

macOS will ask for Screen Recording on first capture, and for Automation the
first time you record with a given browser in front.

```bash
cargo test --workspace                              # 219 tests
cargo clippy --workspace --all-targets              # clean
cargo run -p skillrec-capture --example smoke       # 4s live capture, prints what it saw
```

## The server

`teachonce-server` is the same pipeline without the recorder: the app zips a
recording and submits it, the server reconstructs, transcribes, analyses and
debriefs it with the server's own model endpoint, and the same React UI runs in
a browser to review, answer, plan and build. Recordings live in the app's folder
layout under the server's data directory, so every library crate works
unchanged.

```bash
npm run build                                  # the UI the server embeds
cargo run -p teachonce-server -- --bind 0.0.0.0:7777
```

`--data-dir` chooses where recordings, `server.json` and built skills live; the
default is the server's own application-support folder. The first start generates a shared API key, prints it, and stores it in
`server.json` next to the recordings. In the app, Settings → Server takes the
URL and that key; every recording then has a **Submit to server** button. In a
browser, the server asks for the key once and keeps it in that browser; Settings
→ Server shows it and can rotate it.

The API is the app's command surface: `POST /api/rpc/<command>` with a JSON
body, `POST /api/sessions/upload` with a zip, `GET /api/sessions/<id>/skill.zip`
for a built skill as `<name>/SKILL.md` (the browser's **Download skill** button),
and `GET /api/events` for server-sent progress, all behind
`Authorization: Bearer <key>`. It is plain
HTTP: keep it on a trusted network or put it behind a TLS reverse proxy before
exposing it further. The server never records anything; it only receives what an
app chose to send.

## Building a distributable

```bash
PATH=/usr/bin:$PATH npm run tauri build
# → target/release/bundle/macos/TeachOnce.app
# → target/release/bundle/dmg/TeachOnce_0.2.1_aarch64.dmg
# add `-- --target universal-apple-darwin` for a DMG that also runs on Intel Macs
```

The `PATH` prefix matters on a machine with pyenv or Homebrew: both ship a
Python `xattr` that shadows Apple's, and the bundler's `xattr -cr` fails on it.
The bundle is ad-hoc signed (`signingIdentity: "-"`), which is enough to run but
not to pass Gatekeeper on another Mac: the recipient opens it once via System
Settings → Privacy & Security → *Open Anyway*, or clears quarantine with
`xattr -dr com.apple.quarantine /Applications/TeachOnce.app`. A Developer ID
certificate plus notarization removes that step. `Info.plist` next to
`tauri.conf.json` carries the microphone and Apple Events usage descriptions a
packaged app needs; without them macOS denies both silently.

Whisper weights and a model endpoint are not bundled: the other Mac needs
Ollama (or any endpoint you configure) for analysis, and downloads the weights
on first transcription.

## On disk

```
~/Library/Application Support/ai.teachonce.app/
  settings.json
  models/ggml-small.bin
  sessions/<id>/
    session.json  events.jsonl  frames/*.jpg  frames.json
    audio/*.wav   narration.json
    bundle.json   description.md
    analysis.json skill.json
```

Built skills go to `~/.config/skills/<name>/SKILL.md`, or a folder you pick —
`~/.claude/skills` for Claude Code. The server's **Download skill** hands you the
same folder as a zip.

Recordings made while the app was still called Skill Recorder lived under
`com.skillrecorder.app`; the first launch of TeachOnce moves that folder into
place. macOS ties Screen Recording, Microphone and Automation grants to the
bundle identifier, so they need granting again after the rename. Settings →
About shows the version, where recordings live, and a button to reveal them.

## Keep secrets out of recordings

Don't record, type, paste, show or narrate passwords, tokens, API keys or other
confidential information. Clipboard capture stores only formats, length, a hash
and a 120-character preview — but a screen still captures whatever was on screen.

## License

MIT © 2026 Roberto Moreno. See [LICENSE](LICENSE).
