import { formatBytes } from './api';
import { Sparkline } from './Sparkline';
import { HISTORY_LABEL } from './useDashboard';

export function percent(used: number, total: number): number {
  return total > 0 ? (used / total) * 100 : 0;
}

export function Tile({ title, value = '—', detail, series, max }: {
  title: string; value?: string | number; detail?: string; series?: number[]; max?: number;
}) {
  return (
    <section className="tile">
      <h2>{title}</h2>
      <p className="value">{value}</p>
      {detail && <p className="detail">{detail}</p>}
      {series && <Sparkline values={series} max={max} label={`${title} ${HISTORY_LABEL}`} />}
    </section>
  );
}

export function UsageTile({ title, used, total, series }: {
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
