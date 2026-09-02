import { useCallback, useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  api,
  events,
  formatSpan,
  type Analysis,
  type AnalysisStep,
  type DebriefReply,
  type FixedValue,
  type FrameRecord,
  type JobStatus,
  type SessionDetail,
  type SessionSummary,
  type SkillPlan,
} from "./api";
import { isTauri } from "./transport";

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
  const [planFeedback, setPlanFeedback] = useState("");
  const [progress, setProgress] = useState<string>("");
  const [busy, setBusy] = useState(false);
  // When the current model job started, so the progress line can show how
  // long it has been waiting: a local model can sit in one turn for minutes.
  const [busySince, setBusySince] = useState<number | null>(null);
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (busySince === null) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [busySince]);
  const [feedback, setFeedback] = useState("");
  const [editing, setEditing] = useState(false);

  // The recording on screen right now. A slow analysis, transcription or plan
  // can finish after the user has moved to another recording, and its result
  // must land on the one it belongs to — or nowhere — not on whatever is open.
  const selectedRef = useRef(selected);
  useEffect(() => {
    selectedRef.current = selected;
  }, [selected]);

  const load = useCallback(
    async (id: string) => {
      try {
        const loaded = await api.loadSession(id);
        if (selectedRef.current === id) setDetail(loaded);
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
    setPlanFeedback("");
    setFeedback("");
    setEditing(false);
    load(selected);
  }, [selected, load]);

  useEffect(() => {
    const unlisten = events.onAgentProgress((p) => setProgress(p.message));
    return () => {
      unlisten.then((off) => off());
    };
  }, []);

  // On the server, a submitted recording is processed in the background; its
  // status arrives here, and the detail is reloaded when the pipeline moves on.
  const [job, setJob] = useState<JobStatus | null>(null);
  useEffect(() => {
    setJob(detail?.job ?? null);
  }, [detail]);
  useEffect(() => {
    if (isTauri) return;
    const unlisten = events.onJob((update) => {
      if (update.id !== selectedRef.current) return;
      setJob(update);
      if (update.phase === "done" || update.phase === "failed") load(update.id);
    });
    return () => {
      unlisten.then((off) => off());
    };
  }, [load]);
  const jobActive = job !== null && job.phase !== "done" && job.phase !== "failed";

  const run = async (action: () => Promise<unknown>) => {
    setBusy(true);
    setBusySince(Date.now());
    setProgress("Working…");
    try {
      await action();
    } catch (err) {
      onError(String(err));
    } finally {
      setBusy(false);
      setBusySince(null);
      setProgress("");
    }
  };
  const elapsed = busySince === null ? 0 : Math.max(0, Math.round((now - busySince) / 1000));

  /**
   * Take a proposed or refined plan on board. A value the user already edited
   * stays edited unless the model changed that value itself in the refinement —
   * the user's retargeting should not be undone by asking for an unrelated change.
   */
  const adoptPlan = (next: SkillPlan, previous: SkillPlan | null, forSession: string) => {
    if (selectedRef.current !== forSession) return;
    const merged = next.values.map((value) => {
      const before = previous?.values.find((v) => v.id === value.id);
      const edited = values.find((v) => v.id === value.id);
      const untouchedByModel = before !== undefined && before.value === value.value;
      return untouchedByModel && edited ? { ...value, value: edited.value } : value;
    });
    setPlan(next);
    setValues(merged);
    setPlanFeedback("");
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
            <strong>{session.title || "Untitled recording"}</strong>
            <small>
              {new Date(session.startedAt).toLocaleString()} ·{" "}
              {formatSpan((session.stoppedAt ?? session.startedAt) - session.startedAt)}
            </small>
            <div className="badges">
              {session.narrated && <span className="badge">narrated</span>}
              {session.hasAnalysis && <span className="badge ok">analysed</span>}
              {session.hasSkill && <span className="badge ok">skill</span>}
              {session.submitted && <span className="badge">on server</span>}
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
              <h1>
                {detail.analysis?.title || detail.summary.title || "Untitled recording"}
              </h1>
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

            {busy && (
              <p className="progress">
                {progress}
                {elapsed >= 5 && <span className="muted"> · {formatSpan(elapsed * 1000)}</span>}
              </p>
            )}
            {!busy && job && jobActive && (
              <p className="progress">
                Server is working on this recording: {job.message}
              </p>
            )}
            {!busy && job && job.phase === "failed" && (
              <p className="warn">
                The server's pipeline stopped: {job.message}{" "}
                <button
                  className="ghost small"
                  onClick={() => run(async () => api.processSession(detail.summary.id))}
                >
                  Run again
                </button>
              </p>
            )}

            {isTauri && detail.serverUrl && (
              <div className="panel">
                <h2>Server</h2>
                <p className="muted">
                  {detail.summary.submitted
                    ? `Submitted to ${detail.summary.submitted.server} on ${new Date(detail.summary.submitted.at).toLocaleString()}. Submitting again replaces that copy.`
                    : `Send this recording — events, frames, narration and any analysis so far — to ${detail.serverUrl}. The server processes it with its own model endpoint.`}
                </p>
                <div className="actions">
                  <button
                    disabled={busy}
                    onClick={() =>
                      run(async () => {
                        setProgress("Uploading the recording…");
                        await api.submitSession(detail.summary.id);
                        await load(detail.summary.id);
                        onChanged();
                      })
                    }
                  >
                    {detail.summary.submitted ? "Submit again" : "Submit to server"}
                  </button>
                  {detail.summary.submitted && (
                    <button
                      className="ghost"
                      onClick={() =>
                        openUrl(detail.summary.submitted!.server).catch((err) => onError(String(err)))
                      }
                    >
                      Open the server
                    </button>
                  )}
                </div>
              </div>
            )}

            {detail.needsTranscription && (
              <div className="panel">
                <h2>Narration</h2>
                <p className="muted">
                  This recording has narration that has not been transcribed. Analysis waits for
                  it — your own words are the clearest statement of what you were doing.
                </p>
                {detail.transcribeVia === "hosted" && (
                  <p className="warn">
                    Settings point transcription at {detail.transcribeHost || "a hosted service"}.
                    Pressing Transcribe uploads this recording's narration audio there.
                  </p>
                )}
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
                  {detail.transcribeVia === "hosted"
                    ? `Transcribe via ${detail.transcribeHost || "hosted service"}`
                    : "Transcribe on this machine"}
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
                      const id = detail.summary.id;
                      await api.analyze(id);
                      await load(id);
                      onChanged();
                      // The debrief is a second model pass. If it fails, the
                      // analysis stands and the panel offers to ask again.
                      try {
                        await api.debriefQuestions(id);
                        await load(id);
                      } catch (err) {
                        onError(String(err));
                      }
                    })
                  }
                >
                  Analyse
                </button>
              </div>
            )}

            {detail.analysis && editing && (
              <AnalysisEditor
                analysis={detail.analysis}
                busy={busy}
                onCancel={() => setEditing(false)}
                onSave={(patch) =>
                  run(async () => {
                    await api.editAnalysis(detail.summary.id, patch);
                    setEditing(false);
                    await load(detail.summary.id);
                    onChanged();
                  })
                }
              />
            )}

            {detail.analysis && !editing && (
              <div className="panel">
                <div className="panel-head">
                  <h2>What you did</h2>
                  <button className="ghost small" disabled={busy} onClick={() => setEditing(true)}>
                    Edit
                  </button>
                </div>
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
                        onChanged();
                      })
                    }
                  >
                    Re-analyse with this feedback
                  </button>
                </div>
              </div>
            )}

            {detail.analysis && !editing && (
              <DebriefPanel
                analysis={detail.analysis}
                busy={busy}
                onAsk={() =>
                  run(async () => {
                    const id = detail.summary.id;
                    await api.debriefQuestions(id);
                    await load(id);
                  })
                }
                onSave={(answers) =>
                  run(async () => {
                    const id = detail.summary.id;
                    await api.answerDebrief(id, answers);
                    await load(id);
                  })
                }
              />
            )}

            {detail.analysis && (
              <div className="panel">
                <h2>Build a skill</h2>
                {!plan && (
                  <button
                    disabled={busy}
                    onClick={() =>
                      run(async () => {
                        const id = detail.summary.id;
                        adoptPlan(await api.planSkill(id), null, id);
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

                    <div className="feedback">
                      <textarea
                        placeholder="Want the plan changed? Say how — 'make the repo a value', 'drop step 2', 'use gh instead of the browser'."
                        value={planFeedback}
                        onChange={(e) => setPlanFeedback(e.target.value)}
                      />
                      <button
                        className="ghost"
                        disabled={busy || !planFeedback.trim()}
                        onClick={() =>
                          run(async () => {
                            const id = detail.summary.id;
                            adoptPlan(await api.planSkill(id, planFeedback), plan, id);
                          })
                        }
                      >
                        Refine the plan with this feedback
                      </button>
                    </div>

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
                      {isTauri && (
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
                      )}
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
                {!isTauri && (
                  <div className="actions">
                    <button
                      className="ghost"
                      disabled={busy}
                      onClick={() => run(async () => api.downloadSkill(detail.summary.id, detail.skill!.name))}
                    >
                      Download skill
                    </button>
                    <span className="muted">
                      A zip holding {detail.skill.name}/SKILL.md. Unpack it into ~/.claude/skills to install it.
                    </span>
                  </div>
                )}
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

            <Filmstrip sessionId={detail.summary.id} frames={detail.frames} onError={onError} />

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

/** Direct edits to the analysis: no model involved, saved as a new revision. */
function AnalysisEditor({
  analysis,
  busy,
  onCancel,
  onSave,
}: {
  analysis: Analysis;
  busy: boolean;
  onCancel: () => void;
  onSave: (patch: { title: string; intent: string; steps: AnalysisStep[] }) => void;
}) {
  const [title, setTitle] = useState(analysis.title);
  const [intent, setIntent] = useState(analysis.intent);
  const [steps, setSteps] = useState<AnalysisStep[]>(analysis.steps.map((step) => ({ ...step })));

  const update = (index: number, patch: Partial<AnalysisStep>) =>
    setSteps(steps.map((step, i) => (i === index ? { ...step, ...patch } : step)));
  const remove = (index: number) => setSteps(steps.filter((_, i) => i !== index));
  const move = (index: number, delta: number) => {
    const target = index + delta;
    if (target < 0 || target >= steps.length) return;
    const next = [...steps];
    [next[index], next[target]] = [next[target], next[index]];
    setSteps(next);
  };
  const add = () =>
    setSteps([
      ...steps,
      { id: "", title: "", detail: "", apps: [], evidence: [], confidence: "medium" },
    ]);

  // Ids stay dense and in order, whatever was removed or moved.
  const renumbered = () => steps.map((step, index) => ({ ...step, id: `s${index + 1}` }));
  const complete = intent.trim() !== "" && steps.every((step) => step.title.trim() !== "");

  return (
    <div className="panel edit-form">
      <h2>Edit what you did</h2>
      <p className="muted">
        Direct edits — the model is not involved. The skill builder works from what you save here.
      </p>
      <label className="row">
        <span className="label">Title</span>
        <input value={title} onChange={(e) => setTitle(e.target.value)} placeholder="2 to 5 words" />
      </label>
      <label className="row">
        <span className="label">Intent</span>
        <input
          value={intent}
          onChange={(e) => setIntent(e.target.value)}
          placeholder="One sentence naming the goal"
        />
      </label>
      <ol className="steps">
        {steps.map((step, index) => (
          <li key={index} className="edit-step">
            <input
              value={step.title}
              placeholder="What you did, past tense: 'Copied the enterprise tier price'"
              onChange={(e) => update(index, { title: e.target.value })}
            />
            <textarea
              value={step.detail}
              placeholder="Detail (optional)"
              onChange={(e) => update(index, { detail: e.target.value })}
            />
            <div className="tools">
              <button className="ghost small" disabled={index === 0} onClick={() => move(index, -1)}>
                Move up
              </button>
              <button
                className="ghost small"
                disabled={index === steps.length - 1}
                onClick={() => move(index, 1)}
              >
                Move down
              </button>
              <button className="ghost small danger" onClick={() => remove(index)}>
                Remove
              </button>
              <small className="muted">
                {step.apps.join(", ")}
                {step.startMs != null && ` · ${formatSpan(step.startMs)}`}
              </small>
            </div>
          </li>
        ))}
      </ol>
      <div className="actions">
        <button className="ghost" onClick={add}>
          Add a step
        </button>
        <button
          disabled={busy || !complete}
          onClick={() => onSave({ title, intent, steps: renumbered() })}
        >
          Save
        </button>
        <button className="ghost" disabled={busy} onClick={onCancel}>
          Cancel
        </button>
      </div>
    </div>
  );
}

/**
 * The debrief: questions the recording could not answer, and the user's replies.
 *
 * Open questions get a box each; settled ones show as question and answer with
 * a way to change the answer. Nothing is sent until Save, so a half-finished
 * debrief costs nothing.
 */
function DebriefPanel({
  analysis,
  busy,
  onAsk,
  onSave,
}: {
  analysis: Analysis;
  busy: boolean;
  onAsk: () => void;
  onSave: (answers: DebriefReply[]) => void;
}) {
  const open = analysis.debrief.filter((q) => !q.answer && !q.skipped);
  const settled = analysis.debrief.filter((q) => q.answer || q.skipped);
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [skips, setSkips] = useState<Record<string, boolean>>({});
  const [changing, setChanging] = useState<Record<string, boolean>>({});

  // A new round means new ids; drafts from the old one must not leak onto it.
  const round = analysis.debrief.map((q) => `${q.id}:${q.answer ?? ""}:${q.skipped}`).join("|");
  useEffect(() => {
    setDrafts({});
    setSkips({});
    setChanging({});
  }, [round]);

  const replies = (): DebriefReply[] => {
    const out: DebriefReply[] = [];
    for (const q of open) {
      const text = (drafts[q.id] ?? "").trim();
      if (text) out.push({ id: q.id, answer: text, skipped: false });
      else if (skips[q.id]) out.push({ id: q.id, answer: null, skipped: true });
    }
    for (const q of settled) {
      if (!changing[q.id]) continue;
      const text = (drafts[q.id] ?? "").trim();
      if (text && text !== (q.answer ?? "")) out.push({ id: q.id, answer: text, skipped: false });
    }
    return out;
  };
  const pending = replies().length;

  if (analysis.debrief.length === 0) {
    return (
      <div className="panel">
        <h2>Debrief</h2>
        <p className="muted">
          The recording shows one run. A few questions about exceptions, decisions and what
          varies are what make the skill work on every run.
        </p>
        <button className="ghost" disabled={busy} onClick={onAsk}>
          Ask me about this recording
        </button>
      </div>
    );
  }

  return (
    <div className="panel debrief">
      <div className="panel-head">
        <h2>
          Debrief
          {open.length > 0 && <span className="count-pill">{open.length} to answer</span>}
        </h2>
        <button className="ghost small" disabled={busy} onClick={onAsk}>
          Ask more
        </button>
      </div>
      {open.length > 0 && (
        <p className="muted">
          A sentence or two each, or skip. Your answers go into the skill as facts about the task.
        </p>
      )}
      <ol className="questions">
        {open.map((q) => (
          <li key={q.id} className={`question ${skips[q.id] ? "skipped" : ""}`}>
            <div className="q-head">
              <span className={`kind ${q.kind}`}>{q.kind}</span>
              <strong>{q.question}</strong>
            </div>
            {q.why && (
              <small className="muted why">
                {q.why}
                {q.stepId ? ` · ${q.stepId}` : ""}
              </small>
            )}
            <textarea
              placeholder="Your answer"
              value={drafts[q.id] ?? ""}
              disabled={busy || !!skips[q.id]}
              onChange={(e) => setDrafts({ ...drafts, [q.id]: e.target.value })}
            />
            <label className="skip">
              <input
                type="checkbox"
                checked={!!skips[q.id]}
                disabled={busy}
                onChange={(e) => setSkips({ ...skips, [q.id]: e.target.checked })}
              />
              Skip this one
            </label>
          </li>
        ))}
        {settled.map((q) => (
          <li key={q.id} className="question settled">
            <div className="q-head">
              <span className={`kind ${q.kind}`}>{q.kind}</span>
              <strong>{q.question}</strong>
            </div>
            {changing[q.id] ? (
              <textarea
                value={drafts[q.id] ?? q.answer ?? ""}
                disabled={busy}
                onChange={(e) => setDrafts({ ...drafts, [q.id]: e.target.value })}
              />
            ) : (
              <p className={q.answer ? "answer" : "answer muted"}>{q.answer ?? "Skipped"}</p>
            )}
            {!changing[q.id] && (
              <div>
                <button
                  className="ghost small"
                  disabled={busy}
                  onClick={() => {
                    setChanging({ ...changing, [q.id]: true });
                    setDrafts({ ...drafts, [q.id]: q.answer ?? "" });
                  }}
                >
                  {q.answer ? "Change" : "Answer"}
                </button>
              </div>
            )}
          </li>
        ))}
      </ol>
      {pending > 0 && (
        <div className="actions">
          <button disabled={busy} onClick={() => onSave(replies())}>
            Save {pending} {pending === 1 ? "answer" : "answers"}
          </button>
        </div>
      )}
    </div>
  );
}

/** How many thumbnails to pull over IPC at a time. A long recording can hold 600. */
const FRAME_BATCH = 24;

/** The retained screen stills, loaded lazily and only once the strip is opened. */
function Filmstrip({
  sessionId,
  frames,
  onError,
}: {
  sessionId: string;
  frames: FrameRecord[];
  onError: (message: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [shown, setShown] = useState(FRAME_BATCH);
  const [images, setImages] = useState<Record<string, string>>({});
  const [zoomed, setZoomed] = useState<FrameRecord | null>(null);
  // Files already asked for, so a re-render never re-reads a frame from disk.
  const requested = useRef(new Set<string>());

  useEffect(() => {
    setOpen(false);
    setShown(FRAME_BATCH);
    setImages({});
    setZoomed(null);
    requested.current = new Set();
  }, [sessionId]);

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    const wanted = frames.slice(0, shown);
    (async () => {
      for (const frame of wanted) {
        if (cancelled) return;
        if (requested.current.has(frame.file)) continue;
        requested.current.add(frame.file);
        try {
          const url = await api.readFrame(sessionId, frame.file);
          setImages((prev) => ({ ...prev, [frame.file]: url }));
        } catch (err) {
          requested.current.delete(frame.file);
          if (!cancelled) onError(String(err));
          return;
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [open, shown, frames, sessionId, onError]);

  useEffect(() => {
    if (!zoomed) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setZoomed(null);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [zoomed]);

  if (frames.length === 0) return null;

  return (
    <details
      className="panel"
      open={open}
      onToggle={(e) => setOpen((e.currentTarget as HTMLDetailsElement).open)}
    >
      <summary>Screen frames ({frames.length}) — stills kept only when the screen changed</summary>
      <div className="filmstrip">
        {frames.slice(0, shown).map((frame) => (
          <button
            key={frame.file}
            className="frame"
            title={frame.file}
            onClick={() => images[frame.file] && setZoomed(frame)}
          >
            {images[frame.file] ? (
              <img src={images[frame.file]} alt={`Screen at ${formatSpan(frame.atMs)}`} />
            ) : (
              <div className="frame-placeholder" />
            )}
            <small>
              {formatSpan(frame.atMs)} · {frame.reason}
            </small>
          </button>
        ))}
        {shown < frames.length && (
          <button className="ghost frame-more" onClick={() => setShown(shown + FRAME_BATCH)}>
            Load {Math.min(FRAME_BATCH, frames.length - shown)} more
          </button>
        )}
      </div>

      {zoomed && images[zoomed.file] && (
        <div className="lightbox" role="dialog" onClick={() => setZoomed(null)}>
          <img src={images[zoomed.file]} alt={`Screen at ${formatSpan(zoomed.atMs)}`} />
          <p>
            {formatSpan(zoomed.atMs)} · {zoomed.reason} · {zoomed.width}×{zoomed.height} · click or
            press Escape to close
          </p>
        </div>
      )}
    </details>
  );
}
