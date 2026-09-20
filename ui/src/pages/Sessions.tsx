import { useState } from 'react';
import { formatDuration, formatTime, resumeSession } from '../api';
import { Link, navigate } from '../router';
import type { Dashboard } from '../useDashboard';

export function SessionsPage({ sessions, samples }: Pick<Dashboard, 'sessions' | 'samples'>) {
  // The newest sample stands in for "now" so running durations tick with the stream.
  const now = samples.at(-1)?.ts;
  const [restarting, setRestarting] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function restart(id: string) {
    setRestarting(id);
    setError(null);
    try {
      await resumeSession(id);
      navigate(`/sessions/${id}`);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setRestarting(null);
    }
  }

  return (
    <section className="panel">
      <div className="panel-head">
        <h2>Recent sessions</h2>
        <Link to="/sessions/new" className="plus" label="New session">+</Link>
      </div>
      {sessions.length === 0 ? (
        <p className="empty">No sessions yet. <Link to="/sessions/new">Create the first one</Link>.</p>
      ) : (
        <div className="table-wrap">
          <table>
            <thead>
              <tr><th>Session</th><th>Status</th><th>Model</th><th>Started</th><th>Duration</th><th>Note</th><th /></tr>
            </thead>
            <tbody>
              {sessions.map((s) => {
                const end = s.ended_at ?? now ?? s.started_at;
                return (
                  <tr key={s.id}>
                    <td><Link to={`/sessions/${s.id}`}><code>{s.id}</code> {s.kind}</Link></td>
                    <td><span className={`badge ${s.status}`}>{s.status}</span></td>
                    <td>{s.model ?? ''}</td>
                    <td>{formatTime(s.started_at)}</td>
                    <td>{formatDuration(Math.max(0, end - s.started_at))}</td>
                    <td className="note">{s.note ?? ''}</td>
                    <td>
                      {s.status === 'interrupted' && s.isolation !== null && s.isolation !== 'none' && (
                        <button type="button" className="ghost" disabled={restarting !== null} onClick={() => void restart(s.id)}>
                          {restarting === s.id ? 'Restarting…' : 'Restart'}
                        </button>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      {error && <p className="msg-error" role="alert">{error}</p>}
    </section>
  );
}
