import { useCallback, useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  api,
  events,
  formatSpan,
  type FixedValue,
  type SessionDetail,
  type SessionSummary,
  type SkillPlan,
} from "./api";

interface Props {
  sessions: SessionSummary[];
  selected: string | null;
  onSelect: (id: string | null) => void;
  onChanged: () => void;
  onError: (message: string) => void;
}

export function Library({ sessions, selected, onSelect, onChanged, onError }: Props) {
  const [detail, setDetail] = useState<SessionDetail | null>(null);
  const [plan, setPlan] = useState<SkillPlan | null>(null);
  const [values, setValues] = useState<FixedValue[]>([]);
  const [progress, setProgress] = useState<string>("");
  const [busy, setBusy] = useState(false);
  const [feedback, setFeedback] = useState("");

  const load = useCallback(
    async (id: string) => {
      try {
        setDetail(await api.loadSession(id));
      } catch (err) {
        onError(String(err));
      }
    },
    [onError],
  );

  useEffect(() => {
    if (!selected) {
      setDetail(null);
      return;
    }
    // A different recording means the previous one's draft plan is irrelevant.
    setPlan(null);
    setValues([]);
    setFeedback("");
    load(selected);
  }, [selected, load]);

  useEffect(() => {
    const unlisten = events.onAgentProgress((p) => setProgress(p.message));
    return () => {
      unlisten.then((off) => off());
    };
  }, []);

  const run = async (action: () => Promise<unknown>) => {
    setBusy(true);
    setProgress("Working…");
    try {
      await action();
    } catch (err) {
      onError(String(err));
    } finally {
      setBusy(false);
      setProgress("");
    }
  };

  return (
    <section className="library">
      <aside className="sessions">
        {sessions.length === 0 && <p className="muted pad">No recordings yet.</p>}
        {sessions.map((session) => (
          <button
            key={session.id}
            className={`session ${selected === session.id ? "active" : ""}`}
            onClick={() => onSelect(session.id)}
          >
            <strong>{session.title ?? "Untitled recording"}</strong>
            <small>
              {new Date(session.startedAt).toLocaleString()} ·{" "}
              {formatSpan((session.stoppedAt ?? session.startedAt) - session.startedAt)}
            </small>
            <div className="badges">
              {session.narrated && <span className="badge">narrated</span>}
              {session.hasAnalysis && <span className="badge ok">analysed</span>}
              {session.hasSkill && <span className="badge ok">skill</span>}
              <span className="badge muted">{session.frameCount} frames</span>
            </div>
          </button>
        ))}
      </aside>

      <div className="detail">
        {!detail && <p className="muted pad">Select a recording.</p>}

        {detail && (
          <>
            <header className="detail-head">
              <h1>{detail.analysis?.title ?? detail.summary.title ?? "Untitled recording"}</h1>
              <button
                className="ghost danger"
                disabled={busy}
                onClick={() =>
                  run(async () => {
                    await api.deleteSession(detail.summary.id);
                    onSelect(null);
                    onChanged();
                  })
                }
              >
                Delete
              </button>
            </header>

            {busy && <p className="progress">{progress}</p>}

            {detail.needsTranscription && (
              <div className="panel">
                <h2>Narration</h2>
                <p className="muted">
                  This recording has narration that has not been transcribed. Analysis waits for
                  it — your own words are the clearest statement of what you were doing.
                </p>
                <button
                  disabled={busy}
                  onClick={() =>
                    run(async () => {
                      await api.transcribe(detail.summary.id);
                      await load(detail.summary.id);
                      onChanged();
                    })
                  }
                >
                  Transcribe on this machine
                </button>
              </div>
            )}

            {!detail.analysis && !detail.needsTranscription && (
              <div className="panel">
                <h2>Analyse</h2>
                <p className="muted">
                  Send this recording to your configured model to reconstruct what you did.
                </p>
                <button
                  disabled={busy}
                  onClick={() =>
                    run(async () => {
                      await api.analyze(detail.summary.id);
                      await load(detail.summary.id);
                      onChanged();
                    })
                  }
                >
                  Analyse
                </button>
              </div>
            )}

            {detail.analysis && (
              <div className="panel">
                <h2>What you did</h2>
                <p className="intent">{detail.analysis.intent}</p>
                <p className="muted">
                  {detail.analysis.intentRationale} · confidence{" "}
                  {detail.analysis.intentConfidence} · revision {detail.analysis.revision} ·{" "}
                  {detail.analysis.model}
                </p>
                <ol className="steps">
                  {detail.analysis.steps.map((step) => (
                    <li key={step.id}>
                      <strong>{step.title}</strong>
                      {step.detail && <p>{step.detail}</p>}
                      <small className="muted">
                        {step.apps.join(", ")}
                        {step.startMs != null && ` · ${formatSpan(step.startMs)}`} ·{" "}
                        {step.confidence}
                      </small>
                    </li>
                  ))}
                </ol>

                <div className="feedback">
                  <textarea
                    placeholder="Not quite right? Say what to fix — 'step 3 is irrelevant', 'you missed the export'."
                    value={feedback}
                    onChange={(e) => setFeedback(e.target.value)}
                  />
                  <button
                    className="ghost"
                    disabled={busy || !feedback.trim()}
                    onClick={() =>
                      run(async () => {
                        await api.reviseAnalysis(detail.summary.id, feedback);
                        setFeedback("");
                        await load(detail.summary.id);
                      })
                    }
                  >
                    Re-analyse with this feedback
                  </button>
                </div>
              </div>
            )}

            {detail.analysis && (
              <div className="panel">
                <h2>Build a skill</h2>
                {!plan && (
                  <button
                    disabled={busy}
                    onClick={() =>
                      run(async () => {
                        const proposed = await api.planSkill(detail.summary.id);
                        setPlan(proposed);
                        setValues(proposed.values);
                      })
                    }
                  >
                    Propose a plan
                  </button>
                )}

                {plan && (
                  <>
                    <p className="intent">{plan.description}</p>
                    {plan.generalization && <p className="muted">{plan.generalization}</p>}

                    {values.length > 0 && (
                      <div className="values">
                        <h3>Fixed values</h3>
                        <p className="muted">
                          These are hard-coded into the skill. Edit any of them before building.
                        </p>
                        {values.map((value, index) => (
                          <label key={value.id} className="row">
                            <span className="label">{value.name}</span>
                            <input
                              value={value.value}
                              onChange={(e) => {
                                const next = [...values];
                                next[index] = { ...value, value: e.target.value };
                                setValues(next);
                              }}
                            />
                          </label>
                        ))}
                      </div>
                    )}

                    <ol className="steps">
                      {plan.steps.map((step, index) => (
                        <li key={index}>
                          <span className={`kind ${step.kind}`}>{step.kind}</span>{" "}
                          <strong>{step.title}</strong>
                          <p>{step.text}</p>
                          {step.tool && <small className="muted">via {step.tool}</small>}
                        </li>
                      ))}
                    </ol>

                    <div className="actions">
                      <button
                        disabled={busy}
                        onClick={() =>
                          run(async () => {
                            await api.buildSkill(detail.summary.id, values);
                            await load(detail.summary.id);
                            onChanged();
                          })
                        }
                      >
                        Install skill
                      </button>
                      <button
                        className="ghost"
                        disabled={busy}
                        onClick={() =>
                          run(async () => {
                            const dir = await open({ directory: true });
                            if (typeof dir !== "string") return;
                            await api.buildSkill(detail.summary.id, values, dir);
                            await load(detail.summary.id);
                            onChanged();
                          })
                        }
                      >
                        Export to a folder…
                      </button>
                      <button className="ghost" disabled={busy} onClick={() => setPlan(null)}>
                        Discard plan
                      </button>
                    </div>
                  </>
                )}
              </div>
            )}

            {detail.skill && (
              <div className="panel">
                <h2>Skill: {detail.skill.name}</h2>
                <p className="muted">{detail.skill.description}</p>
                <pre className="skill-body">{detail.skill.body}</pre>
              </div>
            )}

            {detail.narration && detail.narration.segments.length > 0 && (
              <div className="panel">
                <h2>Narration</h2>
                <ul className="narration">
                  {detail.narration.segments.map((segment, index) => (
                    <li key={index}>
                      <span className="muted">{formatSpan(segment.atMs)}</span> {segment.text}
                    </li>
                  ))}
                </ul>
              </div>
            )}

            <details className="panel">
              <summary>Deterministic reconstruction (no model involved)</summary>
              <pre className="description">{detail.description}</pre>
            </details>
          </>
        )}
      </div>
    </section>
  );
}
