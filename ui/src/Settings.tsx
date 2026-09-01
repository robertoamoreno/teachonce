import { useEffect, useState } from "react";
import { api, events, type ConnectionTest, type Settings } from "./api";

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

  useEffect(() => {
    api.getSettings().then(setSettings).catch((err) => onError(String(err)));
    api.whisperStatus().then(setWhisper).catch(() => undefined);
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
        <h2>Narration model</h2>
        <p className="muted">
          Speech-to-text runs entirely on this machine through whisper.cpp. The weights are
          downloaded once.
        </p>
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
        {whisper && (
          <p className="muted">
            {whisper.cached ? "Weights are downloaded." : `Not downloaded yet (~${whisper.approxMb} MB).`}
          </p>
        )}
        {download !== null && download < 1 && (
          <p className="progress">Downloading… {Math.round(download * 100)}%</p>
        )}
        <button className="ghost" disabled={busy} onClick={() => run(async () => {
          await api.downloadWhisper();
          setWhisper(await api.whisperStatus());
          setDownload(null);
        })}>
          Download weights now
        </button>
      </div>

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
    </section>
  );
}
