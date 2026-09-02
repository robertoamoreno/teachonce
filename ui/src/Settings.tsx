import { useEffect, useState } from "react";
import { openUrl, revealItemInDir } from "@tauri-apps/plugin-opener";
import { api, events, type AppInfo, type ConnectionTest, type ServerInfo, type Settings } from "./api";
import { isTauri, setServerKey } from "./transport";

/** Open a link the way each host can: the opener plugin in the app, a tab in a browser. */
function openLink(url: string, onError: (message: string) => void) {
  if (isTauri) openUrl(url).catch((err) => onError(String(err)));
  else window.open(url, "_blank", "noopener");
}

/** Hosted transcription services speaking the OpenAI audio API. */
const TRANSCRIPTION_PRESETS = [
  { label: "OpenAI", baseUrl: "https://api.openai.com/v1", model: "whisper-1", key: "openai" },
  { label: "Groq", baseUrl: "https://api.groq.com/openai/v1", model: "whisper-large-v3-turbo", key: "groq" },
];

/** Endpoints people actually run, so the common cases are one click away. */
const PRESETS = [
  { label: "Ollama", baseUrl: "http://localhost:11434/v1", model: "qwen3:8b", key: "ollama" },
  { label: "LM Studio", baseUrl: "http://localhost:1234/v1", model: "qwen2-vl-7b-instruct", key: "lm-studio" },
  { label: "llama.cpp", baseUrl: "http://localhost:8080/v1", model: "local-model", key: "llama-cpp" },
  { label: "OpenAI", baseUrl: "https://api.openai.com/v1", model: "gpt-4o", key: "openai" },
];

export function SettingsPanel({ onError }: { onError: (message: string) => void }) {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [test, setTest] = useState<ConnectionTest | null>(null);
  const [whisper, setWhisper] = useState<{ cached: boolean; approxMb: number } | null>(null);
  const [download, setDownload] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(false);
  const [about, setAbout] = useState<AppInfo | null>(null);
  const [server, setServer] = useState<ServerInfo | null>(null);
  const [serverTest, setServerTest] = useState<string | null>(null);
  const [showKey, setShowKey] = useState(false);

  useEffect(() => {
    api.getSettings().then(setSettings).catch((err) => onError(String(err)));
    api.whisperStatus().then(setWhisper).catch(() => undefined);
    api.appInfo().then(setAbout).catch(() => undefined);
    if (!isTauri) api.serverInfo().then(setServer).catch(() => undefined);
    const unlisten = events.onDownload((p) => setDownload(p.fraction));
    return () => {
      unlisten.then((off) => off());
    };
  }, [onError]);

  if (!settings) return <p className="muted pad">Loading…</p>;

  const patch = (next: Partial<Settings>) => {
    setSettings({ ...settings, ...next });
    setSaved(false);
    setTest(null);
  };

  const run = async (action: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await action();
    } catch (err) {
      onError(String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="settings">
      <div className="panel">
        <h2>Model endpoint</h2>
        <p className="muted">
          Analysis is the one step that leaves this machine, and it goes wherever you point it.
          Anything speaking the OpenAI chat-completions API works.
        </p>

        <div className="presets">
          {PRESETS.map((preset) => (
            <button
              key={preset.key}
              className="ghost"
              onClick={() =>
                patch({ llm: { ...settings.llm, baseUrl: preset.baseUrl, model: preset.model } })
              }
            >
              {preset.label}
            </button>
          ))}
        </div>

        <label className="row">
          <span className="label">Base URL</span>
          <input
            value={settings.llm.baseUrl}
            placeholder="http://localhost:11434/v1"
            onChange={(e) => patch({ llm: { ...settings.llm, baseUrl: e.target.value } })}
          />
        </label>
        <label className="row">
          <span className="label">Model</span>
          <input
            value={settings.llm.model}
            onChange={(e) => patch({ llm: { ...settings.llm, model: e.target.value } })}
          />
        </label>
        <label className="row">
          <span className="label">API key</span>
          <input
            type="password"
            value={settings.llm.apiKey}
            onChange={(e) => patch({ llm: { ...settings.llm, apiKey: e.target.value } })}
          />
        </label>
        <label className="row">
          <span className="label">Reasoning</span>
          <select
            value={settings.llm.reasoningEffort || "default"}
            onChange={(e) => patch({ llm: { ...settings.llm, reasoningEffort: e.target.value } })}
          >
            <option value="default">Model default</option>
            <option value="none">None — fastest, recommended for thinking models like qwen3</option>
            <option value="low">Low</option>
            <option value="medium">Medium</option>
            <option value="high">High</option>
          </select>
        </label>
        <p className="muted hint">
          Sent as reasoning_effort. A thinking model spends most of a turn reasoning before it
          answers; on a laptop that is the difference between seconds and minutes. A server that
          rejects the field is detected and it is dropped automatically.
        </p>
        <label className="row">
          <input
            type="checkbox"
            checked={settings.llm.vision}
            onChange={(e) => patch({ llm: { ...settings.llm, vision: e.target.checked } })}
          />
          <span>
            <strong>This model can see images</strong>
            <small>
              Off means the describer works from events and narration only, and never offers
              itself the screen frames. Many local vision models cannot call tools at all —
              if yours rejects them, analysis still works but falls back to a slower path.
            </small>
          </span>
        </label>

        <div className="actions">
          <button disabled={busy} onClick={() => run(async () => setTest(await api.testConnection(settings)))}>
            Test connection
          </button>
          <button
            className="ghost"
            disabled={busy}
            onClick={() =>
              run(async () => {
                setSettings(await api.saveSettings(settings));
                setSaved(true);
              })
            }
          >
            Save
          </button>
          {saved && <span className="ok-text">Saved</span>}
        </div>

        {test && (
          <p className={test.reachable ? "ok-text" : "warn"}>
            {test.message}
            {test.models.length > 0 && (
              <>
                {" "}
                <span className="muted">({test.models.length} models available)</span>
              </>
            )}
          </p>
        )}
      </div>

      <div className="panel">
        <h2>Narration</h2>
        <p className="muted">
          Speech-to-text runs on this machine through whisper.cpp unless you choose a hosted
          service. The language applies to both.
        </p>
        <label className="row">
          <span className="label">Transcribe with</span>
          <select
            value={settings.narration.backend}
            onChange={(e) =>
              patch({
                narration: {
                  ...settings.narration,
                  backend: e.target.value as Settings["narration"]["backend"],
                },
              })
            }
          >
            <option value="local">This machine — whisper.cpp, audio never leaves it</option>
            <option value="hosted">A hosted service — audio is uploaded to it</option>
          </select>
        </label>
        <label className="row">
          <span className="label">Language</span>
          <input
            value={settings.narration.language}
            placeholder="auto"
            onChange={(e) =>
              patch({ narration: { ...settings.narration, language: e.target.value } })
            }
          />
        </label>

        {settings.narration.backend === "local" && (
          <>
            <label className="row">
              <span className="label">Model</span>
              <select
                value={settings.narration.model}
                onChange={(e) =>
                  patch({
                    narration: {
                      ...settings.narration,
                      model: e.target.value as Settings["narration"]["model"],
                    },
                  })
                }
              >
                <option value="tiny">tiny — fastest, roughest (75 MB)</option>
                <option value="base">base (142 MB)</option>
                <option value="small">small — recommended (466 MB)</option>
                <option value="medium">medium (1.5 GB)</option>
                <option value="large-v3-turbo">large-v3-turbo — best (1.6 GB)</option>
              </select>
            </label>
            {whisper && (
              <p className="muted">
                {whisper.cached
                  ? "Weights are downloaded."
                  : `Not downloaded yet (~${whisper.approxMb} MB).`}
              </p>
            )}
            {download !== null && download < 1 && (
              <p className="progress">Downloading… {Math.round(download * 100)}%</p>
            )}
            <button
              className="ghost"
              disabled={busy}
              onClick={() =>
                run(async () => {
                  await api.downloadWhisper();
                  setWhisper(await api.whisperStatus());
                  setDownload(null);
                })
              }
            >
              Download weights now
            </button>
          </>
        )}

        {settings.narration.backend === "hosted" && (
          <>
            <p className="warn">
              With this on, the narration audio of each recording you transcribe is uploaded to
              the endpoint below. Nothing is sent until you press Transcribe on a recording.
            </p>
            <div className="presets">
              {TRANSCRIPTION_PRESETS.map((preset) => (
                <button
                  key={preset.key}
                  className="ghost"
                  onClick={() =>
                    patch({
                      narration: {
                        ...settings.narration,
                        hosted: {
                          ...settings.narration.hosted,
                          baseUrl: preset.baseUrl,
                          model: preset.model,
                        },
                      },
                    })
                  }
                >
                  {preset.label}
                </button>
              ))}
            </div>
            <label className="row">
              <span className="label">Base URL</span>
              <input
                value={settings.narration.hosted.baseUrl}
                placeholder="https://api.openai.com/v1"
                onChange={(e) =>
                  patch({
                    narration: {
                      ...settings.narration,
                      hosted: { ...settings.narration.hosted, baseUrl: e.target.value },
                    },
                  })
                }
              />
            </label>
            <label className="row">
              <span className="label">Model</span>
              <input
                value={settings.narration.hosted.model}
                placeholder="whisper-1"
                onChange={(e) =>
                  patch({
                    narration: {
                      ...settings.narration,
                      hosted: { ...settings.narration.hosted, model: e.target.value },
                    },
                  })
                }
              />
            </label>
            <label className="row">
              <span className="label">API key</span>
              <input
                type="password"
                value={settings.narration.hosted.apiKey}
                placeholder="optional for a self-hosted server"
                onChange={(e) =>
                  patch({
                    narration: {
                      ...settings.narration,
                      hosted: { ...settings.narration.hosted, apiKey: e.target.value },
                    },
                  })
                }
              />
            </label>
            <p className="muted hint">
              Anything speaking the OpenAI transcription API: OpenAI, Groq, or a self-hosted
              server. Audio is sent as 16-bit WAV in five-minute parts.
            </p>
          </>
        )}

        <div className="actions">
          <button
            className="ghost"
            disabled={busy}
            onClick={() =>
              run(async () => {
                setSettings(await api.saveSettings(settings));
                setSaved(true);
              })
            }
          >
            Save
          </button>
        </div>
      </div>

      {isTauri && (
        <div className="panel">
          <h2>Server</h2>
          <p className="muted">
            Optional. A TeachOnce server processes recordings you submit to it with its own model
            endpoint and shows them in a browser. Nothing is sent until you press Submit on a
            recording.
          </p>
          <label className="row">
            <span className="label">Server URL</span>
            <input
              value={settings.server.baseUrl}
              placeholder="http://192.168.1.20:7777"
              onChange={(e) => {
                patch({ server: { ...settings.server, baseUrl: e.target.value } });
                setServerTest(null);
              }}
            />
          </label>
          <label className="row">
            <span className="label">API key</span>
            <input
              type="password"
              value={settings.server.apiKey}
              placeholder="tk_… as printed by the server"
              onChange={(e) => {
                patch({ server: { ...settings.server, apiKey: e.target.value } });
                setServerTest(null);
              }}
            />
          </label>
          <div className="actions">
            <button
              disabled={busy || !settings.server.baseUrl.trim()}
              onClick={() =>
                run(async () => {
                  try {
                    setServerTest(await api.testServer(settings.server));
                  } catch (err) {
                    setServerTest(String(err));
                  }
                })
              }
            >
              Test
            </button>
            <button
              className="ghost"
              disabled={busy}
              onClick={() =>
                run(async () => {
                  setSettings(await api.saveSettings(settings));
                  setSaved(true);
                })
              }
            >
              Save
            </button>
            {saved && <span className="ok-text">Saved</span>}
          </div>
          {serverTest && (
            <p className={serverTest.startsWith("Connected") ? "ok-text" : "warn"}>{serverTest}</p>
          )}
        </div>
      )}

      {!isTauri && (
        <div className="panel">
          <h2>Server</h2>
          <p className="muted">
            Every app and browser presents this one key. Rotating it locks out everyone until
            they enter the new one, including this browser, which is updated automatically.
          </p>
          {server && (
            <dl className="kv">
              <dt>Version</dt>
              <dd>{server.version}</dd>
              <dt>Recordings</dt>
              <dd>
                <code>{server.dataDir}</code> · {server.sessions} on disk
              </dd>
              <dt>API key</dt>
              <dd>
                <code>{showKey ? server.apiKey : "•".repeat(12)}</code>
                <button className="ghost small" onClick={() => setShowKey(!showKey)}>
                  {showKey ? "Hide" : "Show"}
                </button>
              </dd>
            </dl>
          )}
          <div className="actions">
            <button
              className="ghost danger"
              disabled={busy}
              onClick={() =>
                run(async () => {
                  const { apiKey } = await api.rotateApiKey();
                  setServerKey(apiKey);
                  setServer(await api.serverInfo());
                  setShowKey(true);
                })
              }
            >
              Rotate key
            </button>
            <button
              className="ghost"
              disabled={busy}
              onClick={() => {
                setServerKey("");
                window.location.reload();
              }}
            >
              Forget key in this browser
            </button>
          </div>
        </div>
      )}

      {isTauri && (
      <div className="panel">
        <h2>What to capture</h2>
        <p className="muted">
          A source you turn off is never started, so it never asks macOS for its permission.
        </p>
        {(
          [
            ["appActivity", "App switches", "Which application is in front. No permission needed."],
            ["windowTitles", "Window titles", "Needs Screen Recording."],
            ["browserUrls", "Browser URLs", "Needs a one-time Automation grant per browser."],
            ["clipboard", "Clipboard copies", "Formats, length and a 120-character preview only."],
            ["screenFrames", "Screen stills", "Kept only when the screen changes. Needs Screen Recording."],
          ] as const
        ).map(([key, label, hint]) => (
          <label key={key} className="row">
            <input
              type="checkbox"
              checked={settings.capture[key]}
              onChange={(e) => patch({ capture: { ...settings.capture, [key]: e.target.checked } })}
            />
            <span>
              <strong>{label}</strong>
              <small>{hint}</small>
            </span>
          </label>
        ))}
        <button
          className="ghost"
          disabled={busy}
          onClick={() =>
            run(async () => {
              setSettings(await api.saveSettings(settings));
              setSaved(true);
            })
          }
        >
          Save
        </button>
      </div>
      )}

      <div className="panel about">
        <h2>About {about?.name ?? "TeachOnce"}</h2>
        <p className="muted">
          {about
            ? `Version ${about.version} · ${about.identifier} · ${about.license} · by ${about.author}`
            : "Loading…"}
        </p>
        <p>
          Record yourself doing a task once, answer a few questions about it, and hand your agent
          the skill. Capture, reconstruction and transcription run on this machine; only the
          analysis step talks to the model endpoint you choose, and only when you press Analyse.
        </p>
        {about && (
          <dl className="kv">
            <dt>Recordings</dt>
            <dd>
              <code>{about.dataDir}</code>
              {isTauri && (
                <button
                  className="ghost small"
                  onClick={() =>
                    revealItemInDir(about.dataDir).catch((err) => onError(String(err)))
                  }
                >
                  Show in Finder
                </button>
              )}
            </dd>
            <dt>Skills</dt>
            <dd>
              <code>{about.skillsDir}</code>
            </dd>
          </dl>
        )}
        <p className="muted">
          Built with Tauri, React, whisper.cpp, xcap, arboard and cpal. Began as a Rust port of the
          idea behind Microsoft's open-source Skill Recorder.
        </p>
        <div className="actions">
          <button
            className="ghost"
            onClick={() =>
              openLink(about?.repository ?? "https://github.com/robertoamoreno/teachonce", onError)
            }
          >
            Source on GitHub
          </button>
        </div>
      </div>
    </section>
  );
}
