import { memo } from 'react';

interface Props {
  values: number[];
  max?: number;
  /** Full accessible description, e.g. "CPU over the last hour". */
  label: string;
}

const WIDTH = 160;
const HEIGHT = 40;

// Memoised: pass a referentially stable `values` array to skip rebuilding the polyline.
export const Sparkline = memo(function Sparkline({ values, max, label }: Props) {
  const hasHistory = values.length >= 2;
  let points = '';
  if (hasHistory) {
    const top = max ?? values.reduce((a, b) => Math.max(a, b), 1);
    const step = WIDTH / (values.length - 1);
    points = values
      .map((v, i) => `${(i * step).toFixed(1)},${(HEIGHT - (Math.min(v, top) / top) * HEIGHT).toFixed(1)}`)
      .join(' ');
  }
  return (
    <svg
      className="sparkline"
      viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
      role="img"
      aria-label={hasHistory ? label : `${label}: not enough history yet`}
      preserveAspectRatio="none"
    >
      {hasHistory && (
        <polyline points={points} fill="none" stroke="currentColor" strokeWidth="1.5" vectorEffect="non-scaling-stroke" />
      )}
    </svg>
  );
});
