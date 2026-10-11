import { useState } from 'react';
import { deleteSession, formatDuration, formatTime, resumeSession } from '../api';
import { Link, navigate } from '../router';
import { canResume, confirmDelete } from '../sessionOptions';
import type { Dashboard } from '../useDashboard';

export function SessionsPage({ sessions, samples, refresh }: Pick<Dashboard, 'sessions' | 'samples' | 'refresh'>) {
  // The newest sample stands in for "now" so running durations tick with the stream.
  const now = samples.at(-1)?.ts;
  const [busy, setBusy] = useState<{ id: string; action: 'resume' | 'delete' } | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function act(id: string, action: 'resume' | 'delete') {
    setBusy({ id, action });
    setError(null);
    try {
      if (action === 'resume') {
        await resumeSession(id);
        navigate(`/sessions/${id}`);
        return;
      }
      await deleteSession(id);
      refresh();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
    setBusy(null);
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
                    <td className="row-actions">
                      {canResume(s) && (
                        <button type="button" className="ghost" disabled={busy !== null} onClick={() => void act(s.id, 'resume')}>
                          {busy?.id === s.id && busy.action === 'resume' ? 'Resuming…' : 'Resume'}
                        </button>
                      )}
                      {s.status !== 'running' && (
                        <button type="button" className="ghost danger" disabled={busy !== null}
                          onClick={() => { if (confirmDelete(s)) void act(s.id, 'delete'); }}>
                          {busy?.id === s.id && busy.action === 'delete' ? 'Deleting…' : 'Delete'}
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
