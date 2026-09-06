/**
 * 行内迷你趋势。表格里一行一个，用手写 SVG 而不是 ECharts 实例：
 * 十几个 canvas 实例的开销和内存都不值得，而这里也不需要交互。
 *
 * **null 是「没算出来」，断成缺口并画竖虚线，绝不连成 0。**
 */

import { PALETTE } from "@/app/theme/tokens";

export function Sparkline({
  values,
  title,
}: {
  values: readonly (number | null)[];
  title?: string | undefined;
}) {
  const p = PALETTE;
  const W = 82;
  const H = 22;
  const nums = values.filter((v): v is number => v !== null);
  const max = Math.max(1, ...nums);
  const x = (i: number) => (values.length < 2 ? W / 2 : (i * W) / (values.length - 1));
  const y = (v: number) => H - 2 - (v / max) * (H - 5);

  let d = "";
  let pen = false;
  const gaps: number[] = [];
  values.forEach((v, i) => {
    if (v === null) {
      pen = false;
      gaps.push(i);
      return;
    }
    d += `${pen ? "L" : "M"}${x(i).toFixed(1)},${y(v).toFixed(1)} `;
    pen = true;
  });

  const lastIndex = values.length - 1;
  const lastValue = values[lastIndex];

  return (
    <svg className="c2e-spark" viewBox={`0 0 ${W} ${H}`} aria-hidden="true">
      {title ? <title>{title}</title> : null}
      {gaps.map((i) => (
        <line
          key={i}
          x1={x(i)}
          y1={2}
          x2={x(i)}
          y2={H - 2}
          stroke={p.axis}
          strokeWidth={1}
          strokeDasharray="2 2"
        />
      ))}
      <path
        d={d}
        fill="none"
        stroke={p.accent}
        strokeWidth={1.8}
        strokeLinejoin="round"
        strokeLinecap="round"
      />
      {typeof lastValue === "number" ? (
        <circle cx={x(lastIndex)} cy={y(lastValue)} r={2.4} fill={p.accent} />
      ) : null}
    </svg>
  );
}
