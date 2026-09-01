import { useEffect, useState } from "react";
import {
  api,
  formatSpan,
  type PermissionReport,
  type RecorderStatus,
} from "./api";

interface Props {
  status: RecorderStatus | null;
  onError: (message: string) => void;
}

/** The record button, the readiness checks, and the live capture state. */
export function Recorder({ status, onError }: Props) {
  const [permissions, setPermissions] = useState<PermissionReport | null>(null);
  const [microphones, setMicrophones] = useState<{ id: string; label: string }[]>([]);
  const [device, setDevice] = useState<string>("");
  const [narrate, setNarrate] = useState(false);
  const [elapsed, setElapsed] = useState(0);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.permissions().then(setPermissions).catch((err) => onError(String(err)));
    api.listMicrophones().then((list) => {
      setMicrophones(list);
      setDevice(list.find((m) => m.isDefault)?.id ?? list[0]?.id ?? "");
    });
  }, [onError]);

  // A visible timer, ticking from the recorder's own start time rather than a
  // local counter — so it stays right even if this view mounted mid-recording.
  useEffect(() => {
    if (!status?.recording || !status.startedAt) {
      setElapsed(0);
      return;
    }
    const startedAt = status.startedAt;
    const tick = () => setElapsed(Date.now() - startedAt);
    tick();
    const timer = setInterval(tick, 200);
    return () => clearInterval(timer);
  }, [status?.recording, status?.startedAt]);

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

  const recording = status?.recording ?? false;
  const micOn = status?.microphone.state === "on";
  const micError =
    status?.microphone.state === "error" ? status.microphone.detail.message : null;

  return (
    <section className="recorder">
      <div className="stage">
        <button
          className={`record ${recording ? "on" : ""}`}
          disabled={busy}
          onClick={() => run(() => (recording ? api.stop() : api.start(narrate)))}
        >
          <span className="glyph" />
          {recording ? "Stop" : "Record"}
        </button>

        <div className="readout">
          <div className="timer">{recording ? formatSpan(elapsed) : "Ready"}</div>
          <div className="muted">
            {recording
              ? `${status?.eventCount ?? 0} events captured`
              : "⌘⇧R starts and stops from anywhere"}
          </div>
        </div>

        {recording && (
          <button className="ghost danger" disabled={busy} onClick={() => run(api.discard)}>
            Discard
          </button>
        )}
      </div>

      <div className="panel">
        <label className="row">
          <input
            type="checkbox"
            checked={recording ? micOn : narrate}
            disabled={busy}
            onChange={(e) =>
              recording
                ? run(() => api.setMicrophone(e.target.checked, device || undefined))
                : setNarrate(e.target.checked)
            }
          />
          <span>
            <strong>Narrate</strong>
            <small>
              Say what you are doing. Transcribed on this machine — the audio never leaves it.
            </small>
          </span>
        </label>

        {microphones.length > 0 && (
          <label className="row">
            <span className="label">Microphone</span>
            <select value={device} onChange={(e) => setDevice(e.target.value)} disabled={busy}>
              {microphones.map((mic) => (
                <option key={mic.id} value={mic.id}>
                  {mic.label}
                </option>
              ))}
            </select>
          </label>
        )}

        {micError && <p className="warn">Microphone: {micError}</p>}
      </div>

      <div className="panel">
        <h2>What gets recorded</h2>
        <ul className="captures">
          <li>App and window switches</li>
          <li>Window and document titles</li>
          <li>The browser pages you visit</li>
          <li>Short previews of what you copy</li>
          <li>Screen stills, kept only when the screen changes</li>
        </ul>
        <p className="muted">
          All of it stays on this machine. Nothing is sent anywhere until you press Analyse,
          and then only to the endpoint you configured in Settings.
        </p>
        <p className="warn">
          Do not record, type, paste or narrate passwords, tokens or API keys.
        </p>
      </div>

      {permissions && permissions.warnings.length > 0 && (
        <div className="panel">
          <h2>Readiness</h2>
          {permissions.warnings.map((warning) => (
            <p key={warning} className="warn">
              {warning}
            </p>
          ))}
          {permissions.screenRecording !== "granted" && (
            <button
              className="ghost"
              onClick={() =>
                run(async () => {
                  await api.requestScreenRecording();
                  setPermissions(await api.permissions());
                })
              }
            >
              Grant Screen Recording
            </button>
          )}
        </div>
      )}
    </section>
  );
}
