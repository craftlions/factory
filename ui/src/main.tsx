import { StrictMode, useEffect, useState } from 'react';
import { createRoot } from 'react-dom/client';
import {
  fetchOverview, fetchSamples, fetchSessions, formatBytes, formatDuration, formatTime,
  type Overview, type Sample, type Session,
} from './api';
import { Sparkline } from './Sparkline';
import './style.css';

const HISTORY_SECONDS = 60 * 60;
const MAX_POINTS = Math.ceil(HISTORY_SECONDS / 5) + 1;

type Connection = 'connecting' | 'live' | 'reconnecting' | 'unavailable';

function useDashboard() {
  const [overview, setOverview] = useState<Overview | null>(null);
  const [samples, setSamples] = useState<Sample[]>([]);
  const [sessions, setSessions] = useState<Session[]>([]);
  const [connection, setConnection] = useState<Connection>('connecting');

  useEffect(() => {
    const controller = new AbortController();
    async function load() {
      try {
        const [o, s, sess] = await Promise.all([
          fetchOverview(controller.signal),
          fetchSamples(HISTORY_SECONDS, controller.signal),
          fetchSessions(controller.signal),
        ]);
        setOverview(o);
        setSamples(s);
        setSessions(sess);
      } catch {
        if (!controller.signal.aborted) setConnection('unavailable');
      }
    }
    void load();
    // Sessions and host facts change rarely; refresh them on a slow cadence.
    const timer = setInterval(load, 60_000);
    return () => {
      controller.abort();
      clearInterval(timer);
    };
  }, []);

  useEffect(() => {
    const source = new EventSource('/api/events');
    source.onopen = () => setConnection('live');
    source.onerror = () => setConnection((c) => (c === 'unavailable' ? c : 'reconnecting'));
    source.addEventListener('sample', (event: MessageEvent<string>) => {
      const sample = JSON.parse(event.data) as Sample;
      setSamples((prev) => [...prev, sample].slice(-MAX_POINTS));
      setOverview((prev) => (prev ? { ...prev, latest: sample } : prev));
    });
    return () => source.close();
  }, []);

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
  const latest = overview?.latest ?? null;

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
