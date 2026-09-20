export interface Sample {
  ts: number;
  cpu_percent: number;
  load1: number;
  mem_used: number;
  mem_total: number;
  disk_used: number;
  disk_total: number;
  process_cpu_percent: number;
  process_rss: number;
  data_files: number;
  data_bytes: number;
}

export interface Session {
  id: string;
  kind: string;
  status: 'running' | 'completed' | 'interrupted' | 'failed';
  started_at: number;
  ended_at: number | null;
  note: string | null;
  /** Null on rows from before sessions recorded their configuration. */
  isolation: string | null;
  workdir: string | null;
  provider: string | null;
  model: string | null;
  reasoning: string | null;
  /** The harness's own id and file for this session, once it has reported them. */
  harness_session_id: string | null;
  harness_session_file: string | null;
}

export interface Overview {
  version: string;
  hostname: string;
  os: string;
  uptime_seconds: number;
  sample_interval_seconds: number;
  latest: Sample | null;
  sessions: { running: number; completed: number; interrupted: number; failed: number };
}

async function getJson<T>(url: string, signal: AbortSignal): Promise<T> {
  const response = await fetch(url, { signal });
  if (!response.ok) throw new Error(`${url} responded ${response.status}`);
  return (await response.json()) as T;
}

/** POSTs JSON and returns the parsed body, or throws the server's error message. */
async function postJson<T>(url: string, body: unknown): Promise<T | null> {
  const response = await fetch(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  const text = await response.text();
  const parsed = text ? (JSON.parse(text) as T & { error?: string }) : null;
  if (!response.ok) throw new Error(parsed?.error ?? `${url} responded ${response.status}`);
  return parsed;
}

export async function createSession(request: unknown): Promise<string> {
  const created = await postJson<{ id: string }>('/api/sessions', request);
  return created!.id;
}
export const sendPrompt = (id: string, message: string) => postJson(`/api/sessions/${id}/prompt`, { message });
export const abortSession = (id: string) => postJson(`/api/sessions/${id}/abort`, {});
/** Starts the harness again for an ended session, continuing its conversation. */
export const resumeSession = (id: string) => postJson(`/api/sessions/${id}/resume`, {});
export const stopSession = (id: string) => postJson(`/api/sessions/${id}/stop`, {});

export interface HarnessModel {
  provider: string;
  id: string;
  name: string;
  context_window: number | null;
  reasoning: boolean;
}

async function getJsonOrError<T>(url: string, signal: AbortSignal): Promise<T> {
  const response = await fetch(url, { signal });
  const body = (await response.json()) as T & { error?: string };
  if (!response.ok) throw new Error(body.error ?? `${url} responded ${response.status}`);
  return body;
}

/** Models the harness itself reports as usable on this host. */
export const fetchHarnessModels = (harness: string, signal: AbortSignal) =>
  getJsonOrError<HarnessModel[]>(`/api/harnesses/${harness}/models`, signal);
/** Reasoning levels the harness reports for one model. */
export const fetchReasoningLevels = (harness: string, provider: string, model: string, signal: AbortSignal) =>
  getJsonOrError<string[]>(
    `/api/harnesses/${harness}/reasoning?provider=${encodeURIComponent(provider)}&model=${encodeURIComponent(model)}`,
    signal,
  );

export const fetchOverview = (signal: AbortSignal) => getJson<Overview>('/api/overview', signal);
export const fetchSamples = (windowSeconds: number, signal: AbortSignal) =>
  getJson<Sample[]>(`/api/samples?window=${windowSeconds}`, signal);
export const fetchSessions = (signal: AbortSignal) => getJson<Session[]>('/api/sessions', signal);

/**
 * Subscribes to the live sample stream. `onOpen` fires on every (re)connect, so callers can
 * backfill whatever the stream missed. Returns the unsubscribe function.
 */
export function subscribeSamples(handlers: {
  onOpen: () => void;
  onSample: (sample: Sample) => void;
  onError: () => void;
}): () => void {
  const source = new EventSource('/api/events');
  source.onopen = handlers.onOpen;
  source.onerror = handlers.onError;
  source.addEventListener('sample', (event: MessageEvent<string>) => {
    handlers.onSample(JSON.parse(event.data) as Sample);
  });
  return () => source.close();
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KiB', 'MiB', 'GiB', 'TiB'];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

export function formatDuration(seconds: number): string {
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${minutes}m`;
  return `${minutes}m ${seconds % 60}s`;
}

export function formatTime(unixSeconds: number): string {
  return new Date(unixSeconds * 1000).toLocaleString();
}
