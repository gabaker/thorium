import React, { useEffect, useMemo, useRef, useState } from 'react';
import { AxisBottom, AxisLeft } from '@visx/axis';
import { Group } from '@visx/group';
import { scaleLinear, scaleTime } from '@visx/scale';
import { LinePath } from '@visx/shape';

// project imports
import {
  ChartContainer,
  ChartLegend,
  ChartLegendEntry,
  ChartLegendSwatch,
  ChartTooltip,
  ChartTooltipCount,
  EmptyChart,
} from './charts.styled';

/// Axis/label color, resolved from the active theme via a CSS variable.
const AXIS_COLOR = 'var(--thorium-secondary-text)';

/// The default chart height in pixels when the caller does not override it.
const DEFAULT_HEIGHT = 220;

/// Inner margins; the left margin leaves room for value tick labels.
const MARGIN = { top: 8, right: 8, left: 48, bottom: 28 } as const;

/// The stroke width of each series line (thin marks per the viz spec).
const LINE_WIDTH = 2;

/// The radius of the hovered-point marker (an 8px diameter hover target).
const MARKER_RADIUS = 4;

/// A single point in a series; a `null` value is a deliberate gap that breaks the line.
export interface VisxLinePoint {
  /// The point's time in epoch milliseconds.
  ts: number;
  /// The point's value, or `null` to render an honest gap instead of interpolating.
  value: number | null;
}

/// A single series in a {@link VisxLineChart}.
export interface VisxLineSeries {
  /// Stable identity used as the React key.
  id: string;
  /// Human-readable label shown in the legend and tooltip.
  label: string;
  /// CSS color for the line (e.g. a `--thorium-series-*` var); identity follows the series, never its rank.
  color: string;
  /// The points to render, oldest first.
  points: VisxLinePoint[];
}

/// Props for {@link VisxLineChart}.
export interface VisxLineChartProps {
  /// The series to render, in fixed display order.
  series: VisxLineSeries[];
  /// Accessible name for the chart group.
  ariaLabel: string;
  /// Chart height in pixels; defaults to {@link DEFAULT_HEIGHT}.
  height?: number;
  /// Formats a value for the tooltip; defaults to `toLocaleString`.
  valueFormatter?: (value: number) => string;
  /// Placeholder text when every series is empty; defaults to "No data".
  emptyText?: string;
}

/**
 * Find the index of the timestamp nearest to a target among sorted timestamps.
 *
 * @param timestamps - The candidate timestamps, sorted ascending.
 * @param target - The target timestamp to search near.
 * @returns The index of the nearest timestamp, or -1 when there are none.
 */
export function nearestTsIndex(timestamps: number[], target: number): number {
  if (timestamps.length === 0) {
    return -1;
  }
  // walk once keeping the smallest absolute distance; ties keep the earlier point
  let best = 0;
  let bestDistance = Math.abs(timestamps[0] - target);
  for (let index = 1; index < timestamps.length; index++) {
    const distance = Math.abs(timestamps[index] - target);
    if (distance < bestDistance) {
      best = index;
      bestDistance = distance;
    }
  }
  return best;
}

/**
 * Format a time tick label, choosing day or time granularity from the span.
 *
 * @param ts - The tick's time in epoch milliseconds.
 * @param spanMs - The full x-domain span in milliseconds.
 * @returns A short tick label.
 */
export function formatTimeTick(ts: number, spanMs: number): string {
  const date = new Date(ts);
  // spans past two days read better as dates; shorter spans as clock times
  if (spanMs > 2 * 24 * 60 * 60 * 1000) {
    return date.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
  }
  return date.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
}

/**
 * A reusable, responsive multi-series time line chart built on visx.
 *
 * Uses a time x-scale and a linear y-scale, drawing each series with `@visx/shape`'s
 * `LinePath` whose `defined` predicate turns `null` values into honest gaps instead of
 * interpolating across them. The chart measures its container width with a `ResizeObserver`
 * (the house VisxBarChart pattern), renders a legend row (swatch + text-token label) so
 * identity is never color-alone, and ships a crosshair hover: moving the pointer snaps to
 * the nearest timestamp, marks each series' point there, and shows a tooltip listing every
 * series' value. Axes and all text use theme CSS variables; series colors ride each
 * series' own `color`.
 *
 * @param series - The series to render in fixed display order.
 * @param ariaLabel - Accessible name for the chart group.
 * @param height - Chart height in pixels (default {@link DEFAULT_HEIGHT}).
 * @param valueFormatter - Optional tooltip value formatter.
 * @param emptyText - Placeholder when every series is empty.
 * @returns The responsive line chart.
 */
const VisxLineChart: React.FC<VisxLineChartProps> = ({
  series,
  ariaLabel,
  height = DEFAULT_HEIGHT,
  valueFormatter = (value: number) => value.toLocaleString(),
  emptyText = 'No data',
}) => {
  const containerRef = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);
  // the hovered timestamp index into the union of all series timestamps
  const [hoverTs, setHoverTs] = useState<number | null>(null);

  // observe the container so the SVG re-renders at the available width
  useEffect(() => {
    const node = containerRef.current;
    if (!node) {
      return;
    }
    const observer = new ResizeObserver((entries) => {
      const measured = entries[0]?.contentRect.width ?? node.clientWidth;
      setWidth(measured);
    });
    observer.observe(node);
    setWidth(node.clientWidth);
    return () => observer.disconnect();
  }, []);

  const innerWidth = Math.max(0, width - MARGIN.left - MARGIN.right);
  const innerHeight = Math.max(0, height - MARGIN.top - MARGIN.bottom);

  // the sorted union of every series' timestamps drives the x-domain and the crosshair snap
  const timestamps = useMemo(() => {
    const union = new Set<number>();
    for (const line of series) {
      for (const point of line.points) {
        union.add(point.ts);
      }
    }
    return Array.from(union).sort((a, b) => a - b);
  }, [series]);

  const hasData = timestamps.length > 0;
  const spanMs = hasData ? Math.max(1, timestamps[timestamps.length - 1] - timestamps[0]) : 1;

  const xScale = useMemo(
    () =>
      scaleTime<number>({
        domain: hasData ? [new Date(timestamps[0]), new Date(timestamps[timestamps.length - 1])] : [new Date(0), new Date(1)],
        range: [0, innerWidth],
      }),
    [timestamps, innerWidth, hasData],
  );

  const yScale = useMemo(() => {
    // only fall back to 1 when every value is zero (so the domain is never
    // [0,0]); flooring unconditionally would squash sub-1 rates like 0.2/s
    // into the bottom of the plot
    const values = series.flatMap((line) => line.points.map((point) => point.value ?? 0));
    const max = Math.max(0, ...values);
    return scaleLinear<number>({
      domain: [0, max > 0 ? max : 1],
      range: [innerHeight, 0],
      nice: true,
    });
  }, [series, innerHeight]);

  // snap pointer motion to the nearest sampled timestamp for the crosshair;
  // measure against the overlay rect itself since offsetX for SVG children is
  // not consistently relative to the same box across browsers
  const onMove = (event: React.MouseEvent<SVGRectElement>): void => {
    const bounds = event.currentTarget.getBoundingClientRect();
    const localX = event.clientX - bounds.left;
    const target = xScale.invert(localX).getTime();
    const index = nearestTsIndex(timestamps, target);
    setHoverTs(index >= 0 ? timestamps[index] : null);
  };

  // the values each series holds at the hovered timestamp (gaps and misses
  // excluded); a refresh can re-stride the data so a remembered timestamp that
  // no longer exists clears the crosshair instead of drawing it off the data
  const hovered = useMemo(() => {
    if (hoverTs === null || !timestamps.includes(hoverTs)) {
      return null;
    }
    const rows = series.flatMap((line) => {
      const point = line.points.find((candidate) => candidate.ts === hoverTs);
      return point && point.value !== null ? [{ id: line.id, label: line.label, color: line.color, value: point.value }] : [];
    });
    return { ts: hoverTs, rows };
  }, [hoverTs, series, timestamps]);

  return (
    <ChartContainer ref={containerRef}>
      {!hasData ? (
        <EmptyChart>{emptyText}</EmptyChart>
      ) : width > 0 ? (
        <>
          <svg width={width} height={height} role="group" aria-label={ariaLabel}>
            <Group left={MARGIN.left} top={MARGIN.top}>
              {series.map((line) => (
                <LinePath<VisxLinePoint>
                  key={line.id}
                  data={line.points}
                  x={(point) => xScale(new Date(point.ts)) ?? 0}
                  y={(point) => yScale(point.value ?? 0) ?? 0}
                  defined={(point) => point.value !== null}
                  stroke={line.color}
                  strokeWidth={LINE_WIDTH}
                  strokeLinecap="round"
                />
              ))}
              {hovered && (
                <g>
                  <line
                    x1={xScale(new Date(hovered.ts))}
                    x2={xScale(new Date(hovered.ts))}
                    y1={0}
                    y2={innerHeight}
                    stroke={AXIS_COLOR}
                    strokeWidth={1}
                    strokeDasharray="3,3"
                    pointerEvents="none"
                  />
                  {hovered.rows.map((row) => (
                    <circle
                      key={row.id}
                      cx={xScale(new Date(hovered.ts))}
                      cy={yScale(row.value)}
                      r={MARKER_RADIUS}
                      fill={row.color}
                      stroke="var(--thorium-panel-bg)"
                      strokeWidth={2}
                      pointerEvents="none"
                    />
                  ))}
                </g>
              )}
              <AxisLeft
                scale={yScale}
                numTicks={4}
                stroke={AXIS_COLOR}
                tickStroke={AXIS_COLOR}
                tickLabelProps={() => ({ fill: AXIS_COLOR, fontSize: 10, textAnchor: 'end', dx: -2, dy: 3 })}
              />
              <AxisBottom
                top={innerHeight}
                scale={xScale}
                numTicks={Math.max(2, Math.min(6, Math.floor(innerWidth / 90)))}
                stroke={AXIS_COLOR}
                tickStroke={AXIS_COLOR}
                tickFormat={(value) => formatTimeTick((value as Date).getTime(), spanMs)}
                tickLabelProps={() => ({ fill: AXIS_COLOR, fontSize: 10, textAnchor: 'middle', dy: 4 })}
              />
              {/* a full-plot overlay keeps the crosshair hover alive between the thin lines */}
              <rect
                x={0}
                y={0}
                width={innerWidth}
                height={innerHeight}
                fill="transparent"
                onMouseMove={onMove}
                onMouseLeave={() => setHoverTs(null)}
              />
            </Group>
          </svg>
          {hovered && hovered.rows.length > 0 && (
            <ChartTooltip
              style={{
                left: MARGIN.left + (xScale(new Date(hovered.ts)) ?? 0),
                top: MARGIN.top + Math.min(...hovered.rows.map((row) => yScale(row.value) ?? 0)),
              }}
            >
              {formatTimeTick(hovered.ts, spanMs)}
              {hovered.rows.map((row) => (
                <ChartTooltipCount key={row.id}>
                  {row.label}: {valueFormatter(row.value)}
                </ChartTooltipCount>
              ))}
            </ChartTooltip>
          )}
          {/* identity is never color-alone: every series carries a swatch + text label */}
          <ChartLegend>
            {series.map((line) => (
              <ChartLegendEntry key={line.id}>
                <ChartLegendSwatch $color={line.color} aria-hidden />
                {line.label}
              </ChartLegendEntry>
            ))}
          </ChartLegend>
        </>
      ) : (
        // width not yet measured: reserve height so the container never collapses
        <EmptyChart aria-hidden />
      )}
    </ChartContainer>
  );
};

export default VisxLineChart;
