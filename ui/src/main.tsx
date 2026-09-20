import { StrictMode, useEffect, useState } from 'react';
import { createRoot } from 'react-dom/client';
import {
  fetchOverview, fetchSamples, fetchSessions, formatBytes, formatDuration, formatTime, subscribeSamples,
  type Overview, type Sample, type Session,
} from './api';
import { Sparkline } from './Sparkline';
import './style.css';

const HISTORY_SECONDS = 60 * 60;

type Stream = 'connecting' | 'live' | 'reconnecting';
type Connection = Stream | 'unavailable';

/** Appends samples newer than the tail of `prev`, then drops everything outside the history window. */
function mergeSamples(prev: Sample[], incoming: Sample[]): Sample[] {
  const lastTs = prev.length > 0 ? prev[prev.length - 1].ts : -Infinity;
  const merged = [...prev, ...incoming.filter((s) => s.ts > lastTs)];
  if (merged.length === 0) return merged;
  const cutoff = merged[merged.length - 1].ts - HISTORY_SECONDS;
  return merged.filter((s) => s.ts >= cutoff);
}

function useDashboard() {
  const [overview, setOverview] = useState<Overview | null>(null);
  const [samples, setSamples] = useState<Sample[]>([]);
  const [sessions, setSessions] = useState<Session[]>([]);
  const [stream, setStream] = useState<Stream>('connecting');
  const [loadFailed, setLoadFailed] = useState(false);

  // Sessions and host facts change rarely; refresh them on a slow cadence.
  useEffect(() => {
    const controller = new AbortController();
    async function load() {
      try {
        const [o, sess] = await Promise.all([
          fetchOverview(controller.signal),
          fetchSessions(controller.signal),
        ]);
        setOverview(o);
        setSessions(sess);
        setLoadFailed(false);
      } catch {
        if (!controller.signal.aborted) setLoadFailed(true);
      }
    }
    void load();
    const timer = setInterval(load, 60_000);
    return () => {
      controller.abort();
      clearInterval(timer);
    };
  }, []);

  // Samples arrive over the stream; history is backfilled on every (re)connect.
  useEffect(() => {
    const controller = new AbortController();
    const unsubscribe = subscribeSamples({
      onOpen: () => {
        setStream('live');
        fetchSamples(HISTORY_SECONDS, controller.signal)
          .then((history) => setSamples((prev) => mergeSamples(history, prev)))
          .catch(() => {
            if (!controller.signal.aborted) setLoadFailed(true);
          });
      },
      onSample: (sample) => setSamples((prev) => mergeSamples(prev, [sample])),
      onError: () => setStream('reconnecting'),
    });
    return () => {
      controller.abort();
      unsubscribe();
    };
  }, []);

  const connection: Connection = loadFailed ? 'unavailable' : stream;
  return { overview, samples, sessions, connection };
}

function Tile({ title, value, detail, series, max }: {
  title: string; value: string; detail?: string; series: number[]; max?: number;
}) {
  return (
    <section className="tile">
      <h2>{title}</h2>
      <p className="value">{value}</p>
      {detail && <p className="detail">{detail}</p>}
      <Sparkline values={series} max={max} label={title} />
    </section>
  );
}

function percent(used: number, total: number): number {
  return total > 0 ? (used / total) * 100 : 0;
}

function App() {
  const { overview, samples, sessions, connection } = useDashboard();
  const latest = samples.at(-1) ?? overview?.latest ?? undefined;

  return (
    <>
      <header>
        <div>
          <h1>craftlions Factory</h1>
          <p className="subtitle">
            {overview ? `${overview.hostname} · ${overview.os} · v${overview.version}` : 'Loading…'}
          </p>
        </div>
        <p role="status" className={`connection ${connection}`}>
          {connection === 'live' && 'Live'}
          {connection === 'connecting' && 'Connecting…'}
          {connection === 'reconnecting' && 'Reconnecting…'}
          {connection === 'unavailable' && 'Unavailable'}
        </p>
      </header>

      <main>
        <div className="tiles">
          <Tile
            title="CPU"
            value={latest ? `${latest.cpu_percent.toFixed(0)} %` : '—'}
            detail={latest ? `load ${latest.load1.toFixed(2)}` : undefined}
            series={samples.map((s) => s.cpu_percent)}
            max={100}
          />
          <Tile
            title="Memory"
            value={latest ? `${percent(latest.mem_used, latest.mem_total).toFixed(0)} %` : '—'}
            detail={latest ? `${formatBytes(latest.mem_used)} of ${formatBytes(latest.mem_total)}` : undefined}
            series={samples.map((s) => percent(s.mem_used, s.mem_total))}
            max={100}
          />
          <Tile
            title="Disk"
            value={latest ? `${percent(latest.disk_used, latest.disk_total).toFixed(0)} %` : '—'}
            detail={latest ? `${formatBytes(latest.disk_used)} of ${formatBytes(latest.disk_total)}` : undefined}
            series={samples.map((s) => percent(s.disk_used, s.disk_total))}
            max={100}
          />
          <Tile
            title="Service"
            value={latest ? formatBytes(latest.process_rss) : '—'}
            detail={latest ? `${latest.process_cpu_percent.toFixed(1)} % CPU · up ${overview ? formatDuration(overview.uptime_seconds) : ''}` : undefined}
            series={samples.map((s) => s.process_rss)}
          />
          <Tile
            title="Data"
            value={latest ? formatBytes(latest.data_bytes) : '—'}
            detail={latest ? `${latest.data_files} file${latest.data_files === 1 ? '' : 's'}` : undefined}
            series={samples.map((s) => s.data_bytes)}
          />
          <section className="tile">
            <h2>Sessions</h2>
            <p className="value">{overview ? overview.sessions.running : '—'}</p>
            <p className="detail">
              {overview
                ? `running · ${overview.sessions.completed} completed · ${overview.sessions.interrupted} interrupted`
                : undefined}
            </p>
          </section>
        </div>

        <section className="panel">
          <h2>Recent sessions</h2>
          {sessions.length === 0 ? (
            <p className="detail">No sessions recorded.</p>
          ) : (
            <div className="table-wrap">
              <table>
                <thead>
                  <tr><th>Kind</th><th>Status</th><th>Started</th><th>Duration</th><th>Note</th></tr>
                </thead>
                <tbody>
                  {sessions.map((s) => {
                    const end = s.ended_at ?? (latest?.ts ?? s.started_at);
                    return (
                      <tr key={s.id}>
                        <td>{s.kind}</td>
                        <td><span className={`badge ${s.status}`}>{s.status}</span></td>
                        <td>{formatTime(s.started_at)}</td>
                        <td>{formatDuration(Math.max(0, end - s.started_at))}</td>
                        <td>{s.note ?? ''}</td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </section>

        <section className="panel">
          <h2>Sources</h2>
          <ul className="sources">
            {(overview?.sources ?? []).map((src) => (
              <li key={src.name}>
                <span className={`badge ${src.status}`}>{src.status}</span>
                <strong>{src.name}</strong>
                <span className="detail">{src.detail}</span>
              </li>
            ))}
          </ul>
        </section>
      </main>
    </>
  );
}

createRoot(document.getElementById('root')!).render(
  <StrictMode><App /></StrictMode>,
);
