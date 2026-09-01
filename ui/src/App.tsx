import { useCallback, useEffect, useState } from "react";
import { api, events, type RecorderStatus, type SessionSummary } from "./api";
import { Recorder } from "./Recorder";
import { Library } from "./Library";
import { SettingsPanel } from "./Settings";

type Tab = "record" | "library" | "settings";

export function App() {
  const [tab, setTab] = useState<Tab>("record");
  const [status, setStatus] = useState<RecorderStatus | null>(null);
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refreshSessions = useCallback(async () => {
    try {
      setSessions(await api.listSessions());
    } catch (err) {
      setError(String(err));
    }
  }, []);

  useEffect(() => {
    api.status().then(setStatus).catch((err) => setError(String(err)));
    refreshSessions();

    // The recorder can also be driven from the tray and the ⌘⇧R hotkey, so the
    // UI follows pushed status rather than owning it.
    const unlisteners = [
      events.onStatus(setStatus),
      events.onSaved(async (id) => {
        await refreshSessions();
        setSelected(id);
        setTab("library");
      }),
    ];
    return () => {
      unlisteners.forEach((p) => p.then((off) => off()));
    };
  }, [refreshSessions]);

  return (
    <div className="app">
      <nav className="tabs">
        <button className={tab === "record" ? "active" : ""} onClick={() => setTab("record")}>
          Record
          {status?.recording && <span className="dot" aria-label="recording" />}
        </button>
        <button className={tab === "library" ? "active" : ""} onClick={() => setTab("library")}>
          Library
          {sessions.length > 0 && <span className="count">{sessions.length}</span>}
        </button>
        <button className={tab === "settings" ? "active" : ""} onClick={() => setTab("settings")}>
          Settings
        </button>
      </nav>

      {error && (
        <div className="banner error" role="alert">
          {error}
          <button onClick={() => setError(null)}>Dismiss</button>
        </div>
      )}

      <main>
        {tab === "record" && <Recorder status={status} onError={setError} />}
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
