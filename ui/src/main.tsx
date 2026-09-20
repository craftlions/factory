import { StrictMode, useEffect, type ReactNode } from 'react';
import { createRoot } from 'react-dom/client';
import { HostPage } from './pages/Host';
import { NewSessionPage } from './pages/NewSession';
import { OverviewPage } from './pages/Overview';
import { SessionPage } from './pages/Session';
import { SessionsPage } from './pages/Sessions';
import { Link, usePath } from './router';
import { useDashboard, type Connection, type Dashboard } from './useDashboard';
import './style.css';

const CONNECTION_LABEL: Record<Connection, string> = {
  connecting: 'Connecting…',
  live: 'Live',
  reconnecting: 'Reconnecting…',
  unavailable: 'Unavailable',
};

const PAGES: { path: string; title: string; hidden?: boolean; render: (dashboard: Dashboard) => ReactNode }[] = [
  { path: '/', title: 'Overview', render: (d) => <OverviewPage overview={d.overview} /> },
  { path: '/sessions', title: 'Sessions', render: (d) => <SessionsPage sessions={d.sessions} samples={d.samples} /> },
  { path: '/sessions/new', title: 'New session', hidden: true, render: () => <NewSessionPage /> },
  { path: '/host', title: 'Host', render: (d) => <HostPage overview={d.overview} samples={d.samples} /> },
];

function App() {
  const dashboard = useDashboard();
  const { overview, connection } = dashboard;
  const path = usePath();
  const page = PAGES.find((p) => p.path === path);
  const sessionId = /^\/sessions\/([a-z0-9]{1,12})$/.exec(path)?.[1];
  const title = page?.title ?? (sessionId ? `Session ${sessionId}` : 'Not found');

  // Pages are entered after actions that change the session list, such as creating one.
  const { refresh } = dashboard;
  useEffect(refresh, [path, refresh]);

  useEffect(() => {
    document.title = `${title} · craftlions Factory`;
  }, [title]);

  return (
    <div className="shell">
      <aside className="sidebar">
        <p className="brand">craftlions Factory</p>
        <nav aria-label="Main">
          {PAGES.filter((p) => !p.hidden).map((p) => (
            <Link key={p.path} to={p.path} current={p.path === path || path.startsWith(`${p.path}/`)}>{p.title}</Link>
          ))}
        </nav>
        <p className="detail host">
          {overview ? <>{overview.hostname}<br />{overview.os}<br />v{overview.version}</> : 'Loading…'}
        </p>
      </aside>

      <div className="content">
        <header>
          <h1>{title}</h1>
          <p role="status" className={`connection ${connection}`}>{CONNECTION_LABEL[connection]}</p>
        </header>
        <main>
          {page ? page.render(dashboard) : sessionId ? <SessionPage id={sessionId} /> : (
            <section className="panel">
              <p className="detail">There is no page at {path}. <Link to="/">Back to the overview</Link>.</p>
            </section>
          )}
        </main>
      </div>
    </div>
  );
}

createRoot(document.getElementById('root')!).render(
  <StrictMode><App /></StrictMode>,
);
