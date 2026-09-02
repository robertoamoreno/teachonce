import { useCallback, useEffect, useState } from "react";
import { api, events, type RecorderStatus, type SessionSummary } from "./api";
import { isTauri, serverKey, setServerKey } from "./transport";
import { Recorder } from "./Recorder";
import { Library } from "./Library";
import { SettingsPanel } from "./Settings";

type Tab = "record" | "library" | "settings";

export function App() {
  // In a browser there is nothing to record: the library is the front door.
  const [tab, setTab] = useState<Tab>(isTauri ? "record" : "library");
  const [status, setStatus] = useState<RecorderStatus | null>(null);
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [hasKey, setHasKey] = useState(isTauri || serverKey() !== "");

  const refreshSessions = useCallback(async () => {
    try {
      setSessions(await api.listSessions());
    } catch (err) {
      setError(String(err));
    }
  }, []);

  useEffect(() => {
    if (!hasKey) return;
    api.status().then(setStatus).catch((err) => setError(String(err)));
    refreshSessions();

    // The recorder can also be driven from the tray and the ⌘⇧R hotkey, so the
    // UI follows pushed status rather than owning it. On the server the same
    // channel carries job updates, which change what the library shows.
    const unlisteners = [
      events.onStatus(setStatus),
      events.onSaved(async (id) => {
        await refreshSessions();
        setSelected(id);
        setTab("library");
      }),
      events.onJob(() => {
        refreshSessions();
      }),
    ];
    return () => {
      unlisteners.forEach((p) => p.then((off) => off()));
    };
  }, [refreshSessions, hasKey]);

  if (!hasKey) {
    return (
      <KeyGate
        onDone={(key) => {
          setServerKey(key);
          setHasKey(true);
        }}
      />
    );
  }

  return (
    <div className="app">
      <nav className="tabs">
        {isTauri && (
          <button className={tab === "record" ? "active" : ""} onClick={() => setTab("record")}>
            Record
            {status?.recording && <span className="dot" aria-label="recording" />}
          </button>
        )}
        <button className={tab === "library" ? "active" : ""} onClick={() => setTab("library")}>
          Library
          {sessions.length > 0 && <span className="count">{sessions.length}</span>}
        </button>
        <button className={tab === "settings" ? "active" : ""} onClick={() => setTab("settings")}>
          Settings
        </button>
        {!isTauri && <span className="tabs-note">TeachOnce Server</span>}
      </nav>

      {error && (
        <div className="banner error" role="alert">
          {error}
          <button onClick={() => setError(null)}>Dismiss</button>
        </div>
      )}

      <main>
        {tab === "record" && isTauri && <Recorder status={status} onError={setError} />}
        {tab === "library" && (
          <Library
            sessions={sessions}
            selected={selected}
            onSelect={setSelected}
            onChanged={refreshSessions}
            onError={setError}
          />
        )}
        {tab === "settings" && <SettingsPanel onError={setError} />}
      </main>
    </div>
  );
}

/** The browser's front door: the server's API key, kept in this browser only. */
function KeyGate({ onDone }: { onDone: (key: string) => void }) {
  const [key, setKey] = useState("");
  return (
    <div className="gate">
      <form
        className="panel"
        onSubmit={(e) => {
          e.preventDefault();
          if (key.trim()) onDone(key.trim());
        }}
      >
        <h2>TeachOnce Server</h2>
        <p className="muted">
          Enter the API key this server printed when it started. It is shown again under
          Settings → Server in this UI and stays in this browser only.
        </p>
        <label className="row">
          <span className="label">API key</span>
          <input
            type="password"
            autoFocus
            value={key}
            placeholder="tk_…"
            onChange={(e) => setKey(e.target.value)}
          />
        </label>
        <div className="actions">
          <button type="submit" disabled={!key.trim()}>
            Open the library
          </button>
        </div>
      </form>
    </div>
  );
}
