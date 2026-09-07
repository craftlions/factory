interface Props {
  values: number[];
  max?: number;
  label: string;
}

const WIDTH = 160;
const HEIGHT = 40;

export function Sparkline({ values, max, label }: Props) {
  if (values.length < 2) {
    return <svg className="sparkline" viewBox={`0 0 ${WIDTH} ${HEIGHT}`} role="img" aria-label={`${label}: not enough history yet`} />;
  }
  const top = max ?? Math.max(...values, 1);
  const step = WIDTH / (values.length - 1);
  const points = values
    .map((v, i) => `${(i * step).toFixed(1)},${(HEIGHT - (Math.min(v, top) / top) * HEIGHT).toFixed(1)}`)
    .join(' ');
  return (
    <svg className="sparkline" viewBox={`0 0 ${WIDTH} ${HEIGHT}`} role="img" aria-label={`${label} over the last hour`} preserveAspectRatio="none">
      <polyline points={points} fill="none" stroke="currentColor" strokeWidth="1.5" vectorEffect="non-scaling-stroke" />
    </svg>
  );
}
