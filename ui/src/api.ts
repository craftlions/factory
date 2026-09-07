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
  id: number;
  kind: string;
  status: 'running' | 'completed' | 'interrupted';
  started_at: number;
  ended_at: number | null;
  note: string | null;
}

export interface Source {
  name: string;
  status: 'active' | 'planned';
  detail: string;
}

export interface Overview {
  version: string;
  hostname: string;
  os: string;
  uptime_seconds: number;
  sample_interval_seconds: number;
  latest: Sample | null;
  sessions: { running: number; completed: number; interrupted: number };
  sources: Source[];
}

async function getJson<T>(url: string, signal: AbortSignal): Promise<T> {
  const response = await fetch(url, { signal });
  if (!response.ok) throw new Error(`${url} responded ${response.status}`);
  return (await response.json()) as T;
}

export const fetchOverview = (signal: AbortSignal) => getJson<Overview>('/api/overview', signal);
export const fetchSamples = (windowSeconds: number, signal: AbortSignal) =>
  getJson<Sample[]>(`/api/samples?window=${windowSeconds}`, signal);
export const fetchSessions = (signal: AbortSignal) => getJson<Session[]>('/api/sessions', signal);

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
