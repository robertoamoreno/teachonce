/**
 * Typed bindings for the Rust command surface.
 *
 * Everything the UI can do goes through here, so the set of operations the
 * frontend has is exactly this file — there is no ambient `invoke` scattered
 * through components.
 */
import { call as invoke, listen } from "./transport";

export type MicrophoneState =
  | { state: "off" }
  | { state: "on"; detail: { device: string } }
  | { state: "error"; detail: { message: string } };

export interface RecorderStatus {
  recording: boolean;
  sessionId: string | null;
  startedAt: number | null;
  eventCount: number;
  microphone: MicrophoneState;
  lastSessionId: string | null;
}

export interface SessionSummary {
  id: string;
  startedAt: number;
  stoppedAt: number | null;
  platform: string;
  appVersion: string;
  narrated: boolean;
  title?: string;
  submitted?: { server: string; at: number } | null;
  eventCount: number;
  frameCount: number;
  hasTranscript: boolean;
  hasAnalysis: boolean;
  hasSkill: boolean;
}

export type Confidence = "high" | "medium" | "low";

export interface AnalysisStep {
  id: string;
  title: string;
  detail: string;
  startMs?: number | null;
  endMs?: number | null;
  apps: string[];
  evidence: string[];
  confidence: Confidence;
}

export type QuestionKind =
  | "exception"
  | "decision"
  | "variable"
  | "precondition"
  | "outcome"
  | "gotcha";

/** A question the recording could not answer, and the user's reply once given. */
export interface DebriefQuestion {
  id: string;
  question: string;
  why: string;
  kind: QuestionKind;
  stepId?: string | null;
  answer?: string | null;
  skipped: boolean;
}

/** One reply, by question id. A skip with an answer counts as an answer. */
export interface DebriefReply {
  id: string;
  answer: string | null;
  skipped: boolean;
}

export interface Analysis {
  sessionId: string;
  title: string;
  intent: string;
  intentConfidence: Confidence;
  intentRationale: string;
  steps: AnalysisStep[];
  revision: number;
  model: string;
  debrief: DebriefQuestion[];
}

export interface FixedValue {
  id: string;
  name: string;
  value: string;
}

export interface PlanStep {
  title: string;
  text: string;
  kind: "calculation" | "action";
  tool: string;
}

export interface SkillPlan {
  name: string;
  title: string;
  description: string;
  summary: string;
  generalization: string;
  values: FixedValue[];
  steps: PlanStep[];
  allowedTools: string[];
}

export interface BuiltSkill {
  sessionId: string;
  name: string;
  description: string;
  allowedTools: string[];
  body: string;
  values: FixedValue[];
  model: string;
}

export interface FrameRecord {
  file: string;
  atMs: number;
  reason: "changed" | "heartbeat" | "initial";
  width: number;
  height: number;
}

export interface NarrationSegment {
  atMs: number;
  endMs: number;
  text: string;
}

export interface SessionDetail {
  summary: SessionSummary;
  description: string;
  timeline: unknown;
  narration: { model: string; language: string; segments: NarrationSegment[] } | null;
  analysis: Analysis | null;
  skill: BuiltSkill | null;
  frames: FrameRecord[];
  needsTranscription: boolean;
  transcribeVia: TranscriptionBackend;
  transcribeHost: string;
  /** The server this app would submit to, when one is configured. Desktop only. */
  serverUrl?: string | null;
  /** Where the server-side pipeline is for this recording. Server only. */
  job?: JobStatus | null;
}

export type TranscriptionBackend = "local" | "hosted";

export interface JobStatus {
  id: string;
  phase: string;
  message: string;
  updatedAt: number;
}

export interface ServerLink {
  baseUrl: string;
  apiKey: string;
}

export interface ServerInfo {
  version: string;
  dataDir: string;
  apiKey: string;
  sessions: number;
}

export interface DisplayInfo {
  id: number;
  name: string;
  width: number;
  height: number;
  isPrimary: boolean;
}

export interface Settings {
  capture: {
    appActivity: boolean;
    windowTitles: boolean;
    browserUrls: boolean;
    clipboard: boolean;
    screenFrames: boolean;
    /** Display name as macOS shows it; empty means the primary display. */
    display: string;
  };
  llm: {
    baseUrl: string;
    model: string;
    apiKey: string;
    vision: boolean;
    temperature: number;
    maxTokens: number;
    requestTimeoutSecs: number;
    reasoningEffort: string;
  };
  narration: {
    model: "tiny" | "base" | "small" | "medium" | "large-v3-turbo";
    language: string;
    backend: TranscriptionBackend;
    hosted: {
      baseUrl: string;
      apiKey: string;
      model: string;
      requestTimeoutSecs: number;
    };
  };
  server: ServerLink;
}

export interface ConnectionTest {
  reachable: boolean;
  message: string;
  models: string[];
}

export interface PermissionReport {
  screenRecording: "granted" | "denied" | "not-required";
  accessibility: "granted" | "denied" | "not-required";
  warnings: string[];
}

export interface AgentProgress {
  sessionId: string;
  phase: string;
  message: string;
}

export interface AppInfo {
  name: string;
  version: string;
  identifier: string;
  dataDir: string;
  skillsDir: string;
  author: string;
  repository: string;
  license: string;
}

export interface DownloadProgress {
  downloadedBytes: number;
  totalBytes: number;
  fraction: number;
}

export const api = {
  status: () => invoke<RecorderStatus>("recorder_status"),
  start: (narrate: boolean, device?: string) =>
    invoke<string>("start_recording", { narrate, device: device ?? null }),
  stop: () => invoke<string>("stop_recording"),
  discard: () => invoke<string>("discard_recording"),
  setMicrophone: (on: boolean, device?: string) =>
    invoke<MicrophoneState>("set_microphone", { on, device: device ?? null }),
  listMicrophones: () =>
    invoke<{ id: string; label: string; isDefault: boolean }[]>("list_microphones"),
  listDisplays: () => invoke<DisplayInfo[]>("list_displays"),

  permissions: () => invoke<PermissionReport>("permission_report"),
  requestScreenRecording: () => invoke<boolean>("request_screen_recording"),

  listSessions: () => invoke<SessionSummary[]>("list_sessions"),
  loadSession: (id: string) => invoke<SessionDetail>("load_session", { id }),
  deleteSession: (id: string) => invoke<void>("delete_session", { id }),
  readFrame: (id: string, file: string) => invoke<string>("read_frame", { id, file }),

  getSettings: () => invoke<Settings>("get_settings"),
  saveSettings: (settings: Settings) => invoke<Settings>("save_settings", { settings }),
  testConnection: (settings?: Settings) =>
    invoke<ConnectionTest>("test_connection", { settings: settings ?? null }),

  analyze: (id: string) => invoke<Analysis>("analyze_session", { id }),
  reviseAnalysis: (id: string, feedback: string) =>
    invoke<Analysis>("revise_analysis", { id, feedback }),
  editAnalysis: (id: string, patch: { title?: string; intent?: string; steps?: AnalysisStep[] }) =>
    invoke<Analysis>("edit_analysis", { id, ...patch }),
  debriefQuestions: (id: string) => invoke<Analysis>("debrief_questions", { id }),
  answerDebrief: (id: string, answers: DebriefReply[]) =>
    invoke<Analysis>("answer_debrief", { id, answers }),

  planSkill: (id: string, feedback?: string) =>
    invoke<SkillPlan>("plan_skill", { id, feedback: feedback ?? null }),
  buildSkill: (id: string, values: FixedValue[], exportDir?: string) =>
    invoke<{ skill: BuiltSkill; path: string }>("build_skill", {
      id,
      values,
      exportDir: exportDir ?? null,
    }),

  appInfo: () => invoke<AppInfo>("app_info"),

  // Desktop → server
  submitSession: (id: string) => invoke<void>("submit_session", { id }),
  testServer: (link: ServerLink) => invoke<string>("test_server", { link }),

  // Server only
  serverInfo: () => invoke<ServerInfo>("server_info"),
  rotateApiKey: () => invoke<{ apiKey: string }>("rotate_api_key"),
  listJobs: () => invoke<JobStatus[]>("list_jobs"),
  processSession: (id: string) => invoke<{ queued: boolean }>("process_session", { id }),

  whisperStatus: () =>
    invoke<{ model: string; cached: boolean; approxMb: number }>("whisper_status"),
  downloadWhisper: () => invoke<string>("download_whisper_model"),
  transcribe: (id: string) =>
    invoke<{ model: string; language: string; segments: NarrationSegment[] }>(
      "transcribe_session",
      { id },
    ),
};

export const events = {
  onStatus: (handler: (status: RecorderStatus) => void) =>
    listen<RecorderStatus>("recorder://status", (e) => handler(e.payload)),
  onSaved: (handler: (id: string) => void) =>
    listen<string>("recorder://saved", (e) => handler(e.payload)),
  onAgentProgress: (handler: (progress: AgentProgress) => void) =>
    listen<AgentProgress>("agent://progress", (e) => handler(e.payload)),
  onDownload: (handler: (progress: DownloadProgress) => void) =>
    listen<DownloadProgress>("whisper://download", (e) => handler(e.payload)),
  onJob: (handler: (job: JobStatus) => void) =>
    listen<JobStatus>("job://status", (e) => handler(e.payload)),
};

/** Format a millisecond span the way the timeline does. */
export function formatSpan(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  const seconds = ms / 1000;
  if (seconds < 60) return `${seconds.toFixed(1)}s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes}m${String(Math.round(seconds - minutes * 60)).padStart(2, "0")}s`;
}
