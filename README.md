# Skill Recorder (Rust)

**Record yourself doing a task once, then turn it into a skill your AI agent can repeat.**

A Rust + Tauri reimplementation of the idea behind
[microsoft/skill-recorder](https://github.com/microsoft/skill-recorder), with one
structural change: **everything runs locally except the analysis step, and that
step points at any OpenAI-compatible endpoint you configure.**

```
capture ──► local reconstruction ──► [ your endpoint ] ──► review ──► SKILL.md
  100% local        100% local          the only hop        you       local
```

## The privacy boundary

There is exactly one crate in this workspace that can open a network connection:
`skillrec-agent`. Screen capture, window tracking, clipboard, frame selection and
speech-to-text all live in crates that have no HTTP client compiled into them at
all. That is not a policy statement — it is checkable:

```bash
cargo tree -p skillrec-capture   | grep -c reqwest   # 0
cargo tree -p skillrec-narration | grep -c reqwest   # 1, only to fetch Whisper weights
cargo tree -p skillrec-agent     | grep -c reqwest   # 1, the analysis endpoint
```

Nothing is sent anywhere until you press **Analyse**, and then only the timeline,
the events, the narration text, and any screen frames the model explicitly asks
to look at.

## Configuring the model

Settings → Model endpoint. Anything speaking the OpenAI chat-completions API:

| Server | Base URL | Notes |
|---|---|---|
| Ollama | `http://localhost:11434/v1` | Fully local. The default (`qwen3:8b`) has tools but no vision. |
| LM Studio | `http://localhost:1234/v1` | Fully local. |
| llama.cpp | `http://localhost:8080/v1` | No `/models` route; the connection test handles that. |
| OpenAI | `https://api.openai.com/v1` | Needs a real key. |
| vLLM / OpenRouter / … | your URL | Same contract. |

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
| Screen stills | 1000 ms | Screen Recording |
| Microphone | continuous, toggleable | Microphone |

**No video is recorded.** Screen stills are hashed with a difference hash and kept
only when the screen actually changed, or every 5 s as a heartbeat. A ten-minute
recording is typically a few dozen JPEGs — no encoder, no container, no ffmpeg,
and nothing to decode later.

**2. Reconstruct** — on stop, the event stream is segmented into ordered steps
(a new step when you change app, or change host inside a browser) and written as
`bundle.json` + `description.md`. No model involved. This is what the describer's
`get_timeline` tool returns, and the fallback if you never configure an endpoint.

**3. Transcribe** *(if you narrated)* — whisper.cpp with Metal acceleration, on
this machine. Analysis refuses to run on untranscribed narration: your own words
are the clearest statement of intent in the recording.

**4. Analyse** — a tool-calling agent reconstructs your intent and the ordered
steps. It reads the timeline, then the narration, then events where something is
unclear, and looks at frames *only* where events leave real ambiguity. You review
it, edit it directly, or send natural-language feedback for another pass.

**5. Build** — two phases with you in between. The model proposes a plan: how it
generalizes your single run, the fixed values it hard-codes (as `{{token}}`
fields you can edit), and the ordered steps, each tagged `calculation` or
`action`. You approve, and it writes the `SKILL.md`. Your edited values win — the
model does not get a second say.

## Layout

```
crates/core         events, session store, timeline, config     53 tests
crates/capture      screen, window, clipboard, audio (macOS)    41 tests
crates/narration    whisper.cpp transcription                    8 tests
crates/agent        OpenAI-compatible client + agent loop      42 + 11 tests
crates/recorder     the lifecycle state machine                  6 tests
src-tauri           commands, tray, hotkey
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
cargo test --workspace                              # 161 tests
cargo clippy --workspace --all-targets              # clean
cargo run -p skillrec-capture --example smoke       # 4s live capture, prints what it saw
```

## On disk

```
~/Library/Application Support/com.skillrecorder.app/
  settings.json
  models/ggml-small.bin
  sessions/<id>/
    session.json  events.jsonl  frames/*.jpg  frames.json
    audio/*.wav   narration.json
    bundle.json   description.md
    analysis.json skill.json
```

Built skills go to `~/.config/skills/<name>/SKILL.md`, or a folder you pick.

## Keep secrets out of recordings

Don't record, type, paste, show or narrate passwords, tokens, API keys or other
confidential information. Clipboard capture stores only formats, length, a hash
and a 120-character preview — but a screen still captures whatever was on screen.

## License

MIT.
