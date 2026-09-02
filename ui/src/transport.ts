/**
 * Where the UI's calls go.
 *
 * Inside the desktop app they are Tauri commands and events. In a browser,
 * served by the TeachOnce server, the same command names go to the server's
 * RPC route with the shared API key, and events arrive over server-sent
 * events. Nothing above this file knows which it is talking to.
 */
import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen as tauriListen } from "@tauri-apps/api/event";

export const isTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

const KEY_STORAGE = "teachonce.apiKey";

/** The key this browser presents to the server, if it has one. */
export function serverKey(): string {
  try {
    return localStorage.getItem(KEY_STORAGE) ?? "";
  } catch {
    return "";
  }
}

export function setServerKey(key: string) {
  try {
    if (key) localStorage.setItem(KEY_STORAGE, key);
    else localStorage.removeItem(KEY_STORAGE);
  } catch {
    // Storage may be unavailable; the key then lasts for this page only.
  }
  closeEvents();
}

/** Run a command: a Tauri invoke, or the server's RPC route. */
export async function call<T>(command: string, args: Record<string, unknown> = {}): Promise<T> {
  if (isTauri) return tauriInvoke<T>(command, args);
  const response = await fetch(`/api/rpc/${command}`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Authorization: `Bearer ${serverKey()}` },
    body: JSON.stringify(args),
  });
  if (response.status === 401) {
    throw new Error("The server rejected the API key. Enter it again under Settings → Server.");
  }
  if (!response.ok) {
    throw new Error((await response.text()) || `The server answered ${response.status}.`);
  }
  return (await response.json()) as T;
}

type Handler = (event: { payload: unknown }) => void;
const handlers = new Map<string, Set<Handler>>();
let source: EventSource | null = null;

function openEvents() {
  if (source || isTauri) return;
  source = new EventSource(`/api/events?key=${encodeURIComponent(serverKey())}`);
  source.onmessage = (message) => {
    try {
      const { event, payload } = JSON.parse(message.data) as { event: string; payload: unknown };
      handlers.get(event)?.forEach((handler) => handler({ payload }));
    } catch {
      // A malformed line is dropped; the next one is independent.
    }
  };
}

function closeEvents() {
  source?.close();
  source = null;
}

/** Subscribe to an event; resolves to the function that unsubscribes. */
export async function listen<T>(
  event: string,
  handler: (event: { payload: T }) => void,
): Promise<() => void> {
  if (isTauri) return tauriListen<T>(event, handler);
  const set = handlers.get(event) ?? new Set<Handler>();
  const wrapped: Handler = (e) => handler(e as { payload: T });
  set.add(wrapped);
  handlers.set(event, set);
  openEvents();
  return () => {
    set.delete(wrapped);
  };
}

/** Upload a file to the server. Browser-only; the desktop app has its own path. */
export async function upload(path: string, file: File): Promise<Response> {
  const form = new FormData();
  form.append("file", file);
  return fetch(path, { method: "POST", headers: { Authorization: `Bearer ${serverKey()}` }, body: form });
}
