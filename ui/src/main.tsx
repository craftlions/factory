import { StrictMode, useEffect, useMemo, useState } from 'react';
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

const CONNECTION_LABEL: Record<Connection, string> = {
  connecting: 'Connecting…',
  live: 'Live',
  reconnecting: 'Reconnecting…',
  unavailable: 'Unavailable',
};

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

function Tile({ title, value = '—', detail, series, max }: {
  title: string; value?: string | number; detail?: string; series?: number[]; max?: number;
}) {
  return (
    <section className="tile">
      <h2>{title}</h2>
      <p className="value">{value}</p>
      {detail && <p className="detail">{detail}</p>}
      {series && <Sparkline values={series} max={max} label={title} />}
    </section>
  );
}

function UsageTile({ title, used, total, series }: {
  title: string; used?: number; total?: number; series: number[];
}) {
  const known = used !== undefined && total !== undefined;
  return (
    <Tile
      title={title}
      value={known ? `${percent(used, total).toFixed(0)} %` : undefined}
      detail={known ? `${formatBytes(used)} of ${formatBytes(total)}` : undefined}
      series={series}
      max={100}
    />
  );
}

function percent(used: number, total: number): number {
  return total > 0 ? (used / total) * 100 : 0;
}

function App() {
  const { overview, samples, sessions, connection } = useDashboard();
  const latest = samples.at(-1) ?? overview?.latest ?? undefined;
  const series = useMemo(() => ({
    cpu: samples.map((s) => s.cpu_percent),
    mem: samples.map((s) => percent(s.mem_used, s.mem_total)),
    disk: samples.map((s) => percent(s.disk_used, s.disk_total)),
    rss: samples.map((s) => s.process_rss),
    data: samples.map((s) => s.data_bytes),
  }), [samples]);

  return (
    <>
      <header>
        <div>
          <h1>craftlions Factory</h1>
          <p className="subtitle">
            {overview ? `${overview.hostname} · ${overview.os} · v${overview.version}` : 'Loading…'}
          </p>
        </div>
        <p role="status" className={`connection ${connection}`}>{CONNECTION_LABEL[connection]}</p>
      </header>

      <main>
        <div className="tiles">
          <Tile
            title="CPU"
            value={latest && `${latest.cpu_percent.toFixed(0)} %`}
            detail={latest && `load ${latest.load1.toFixed(2)}`}
            series={series.cpu}
            max={100}
          />
          <UsageTile title="Memory" used={latest?.mem_used} total={latest?.mem_total} series={series.mem} />
          <UsageTile title="Disk" used={latest?.disk_used} total={latest?.disk_total} series={series.disk} />
          <Tile
            title="Service"
            value={latest && formatBytes(latest.process_rss)}
            detail={latest && overview
              ? `${latest.process_cpu_percent.toFixed(1)} % CPU · up ${formatDuration(overview.uptime_seconds)}`
              : undefined}
            series={series.rss}
          />
          <Tile
            title="Data"
            value={latest && formatBytes(latest.data_bytes)}
            detail={latest && `${latest.data_files} file${latest.data_files === 1 ? '' : 's'}`}
            series={series.data}
          />
          <Tile
            title="Sessions"
            value={overview?.sessions.running}
            detail={overview
              ? `running · ${overview.sessions.completed} completed · ${overview.sessions.interrupted} interrupted`
              : undefined}
          />
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
                    const end = s.ended_at ?? latest?.ts ?? s.started_at;
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
