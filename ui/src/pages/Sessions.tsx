import { formatDuration, formatTime } from '../api';
import type { Dashboard } from '../useDashboard';

export function SessionsPage({ sessions, samples }: Pick<Dashboard, 'sessions' | 'samples'>) {
  // The newest sample stands in for "now" so running durations tick with the stream.
  const now = samples.at(-1)?.ts;
  return (
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
                const end = s.ended_at ?? now ?? s.started_at;
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
  );
}
