import { useMemo } from 'react';
import { formatBytes, formatDuration } from '../api';
import { Tile, UsageTile, percent } from '../Tile';
import type { Dashboard } from '../useDashboard';

export function HostPage({ overview, samples }: Pick<Dashboard, 'overview' | 'samples'>) {
  const latest = samples.at(-1) ?? overview?.latest ?? undefined;
  const series = useMemo(() => ({
    cpu: samples.map((s) => s.cpu_percent),
    mem: samples.map((s) => percent(s.mem_used, s.mem_total)),
    disk: samples.map((s) => percent(s.disk_used, s.disk_total)),
    rss: samples.map((s) => s.process_rss),
    data: samples.map((s) => s.data_bytes),
  }), [samples]);

  return (
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
    </div>
  );
}
