import { Tile } from '../Tile';
import type { Dashboard } from '../useDashboard';

export function OverviewPage({ overview }: Pick<Dashboard, 'overview'>) {
  const counts = overview?.sessions;
  return (
    <div className="tiles">
      <Tile title="Running sessions" value={counts?.running} />
      <Tile title="Completed sessions" value={counts?.completed} />
      <Tile title="Interrupted sessions" value={counts?.interrupted} />
    </div>
  );
}
