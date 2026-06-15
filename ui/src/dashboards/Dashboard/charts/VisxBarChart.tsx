import React, { useEffect, useMemo, useRef, useState } from 'react';
import { AxisBottom, AxisLeft } from '@visx/axis';
import { Group } from '@visx/group';
import { scaleBand, scaleLinear } from '@visx/scale';
import { Bar } from '@visx/shape';

// spec: ./SPEC.md

// project imports
import { ChartContainer, EmptyChart } from './charts.styled';

/// The default fill color used when a bar does not specify its own `color`.
const DEFAULT_BAR_COLOR = 'var(--thorium-link-text, #5b9bd5)';

/// Axis/label color, resolved from the active theme via a CSS variable.
const AXIS_COLOR = 'var(--thorium-secondary-text)';

/// The default chart height in pixels when the caller does not override it.
const DEFAULT_HEIGHT = 220;

/// Inner margins reserving room for the left count ticks and the bottom category labels.
const MARGIN = { top: 8, right: 8, bottom: 44, left: 40 } as const;

/// Max characters shown on a bottom-axis tick label before truncation (full text stays in the a11y label).
const MAX_TICK_CHARS = 10;

/// The minimum horizontal space (px) a bottom-axis tick label needs to stay legible; when a band is
/// narrower than this, ticks are thinned to keep every-Nth label so high-cardinality keys don't smear.
const MIN_TICK_SPACING_PX = 44;

/// A single bar in a {@link VisxBarChart}.
export interface VisxBar {
  /// Stable identity used as the React key and passed to `onBarClick`.
  id: string;
  /// Human-readable label shown under the bar and in its accessible name.
  label: string;
  /// The numeric value; sizes the bar height and is shown in the accessible name.
  value: number;
  /// Optional CSS color (e.g. a `--thorium-*` var) for the bar fill; defaults to a theme link color.
  color?: string;
}

/// Props for {@link VisxBarChart}.
export interface VisxBarChartProps {
  /// The bars to render, in display order (left to right).
  bars: VisxBar[];
  /// When provided, each bar becomes interactive and this is called with the activated bar's `id`.
  onBarClick?: (id: string) => void;
  /// Accessible name for the chart group.
  ariaLabel: string;
  /// Chart height in pixels; defaults to {@link DEFAULT_HEIGHT}.
  height?: number;
}

/**
 * Truncate a tick label to {@link MAX_TICK_CHARS}, appending an ellipsis when shortened.
 *
 * @param label - The full label.
 * @returns The label, truncated with an ellipsis when longer than the cap.
 */
function truncateTick(label: string): string {
  return label.length > MAX_TICK_CHARS ? `${label.slice(0, MAX_TICK_CHARS - 1)}…` : label;
}

/**
 * A reusable, responsive vertical bar chart built on visx.
 *
 * Uses a band x-scale (one slot per bar) and a linear y-scale (count), drawing each bar with
 * `@visx/shape`'s `Bar` inside a `@visx/group` `Group`, with `@visx/axis` `AxisLeft`/`AxisBottom` for the
 * count and category axes. The chart measures its container width with a `ResizeObserver` and re-renders
 * responsively; height is fixed via the `height` prop. When `onBarClick` is supplied each bar is a
 * keyboard- and pointer-activatable `role="button"` rect (Enter/Space activate) with an
 * `"{label}: {value}"` accessible name; otherwise bars are inert. Bar fill uses each bar's `color` or a
 * theme fallback, and axes use theme CSS variables so every theme renders legibly. An empty `bars` array
 * renders a muted "No data" placeholder.
 *
 * @param bars - The bars to render, in display order.
 * @param onBarClick - Optional handler called with a bar's `id` on activation; makes bars interactive.
 * @param ariaLabel - Accessible name for the chart group.
 * @param height - Chart height in pixels (default {@link DEFAULT_HEIGHT}).
 * @returns The responsive bar chart.
 */
const VisxBarChart: React.FC<VisxBarChartProps> = ({ bars, onBarClick, ariaLabel, height = DEFAULT_HEIGHT }) => {
  const containerRef = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);

  // observe the container so the SVG re-renders at the available width across layout/breakpoint changes
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

  const xScale = useMemo(
    () =>
      scaleBand<string>({
        domain: bars.map((bar) => bar.id),
        range: [0, innerWidth],
        padding: 0.2,
      }),
    [bars, innerWidth],
  );

  const yScale = useMemo(
    () =>
      scaleLinear<number>({
        // floor the domain at 1 so an all-zero (or empty) data set never divides by zero
        domain: [0, Math.max(1, ...bars.map((bar) => bar.value))],
        range: [innerHeight, 0],
        nice: true,
      }),
    [bars, innerHeight],
  );

  // map bar id -> label so axis tick formatting (which receives the band value = id) can show the label
  const labelById = useMemo(() => new Map(bars.map((bar) => [bar.id, bar.label])), [bars]);

  // thin the bottom-axis ticks when bands get too narrow for a legible label: keep every Nth bar id so a
  // high-cardinality key (dozens of tag values) shows a readable subset instead of an overlapping smear.
  // Each bar still carries its own <title>/aria-label, so the dropped labels remain discoverable on hover.
  const tickValues = useMemo(() => {
    const bandwidth = xScale.bandwidth();
    if (bandwidth <= 0 || bandwidth >= MIN_TICK_SPACING_PX) {
      return undefined;
    }
    const step = Math.ceil(MIN_TICK_SPACING_PX / bandwidth);
    return bars.filter((_, index) => index % step === 0).map((bar) => bar.id);
  }, [bars, xScale]);

  const interactive = typeof onBarClick === 'function';

  if (bars.length === 0) {
    return <EmptyChart>No data</EmptyChart>;
  }

  return (
    <ChartContainer ref={containerRef}>
      {width > 0 ? (
        <svg width={width} height={height} role="group" aria-label={ariaLabel}>
          <Group left={MARGIN.left} top={MARGIN.top}>
            {bars.map((bar) => {
              const barX = xScale(bar.id) ?? 0;
              const barWidth = xScale.bandwidth();
              const barY = yScale(bar.value);
              const barHeight = Math.max(0, innerHeight - barY);
              const accessibleName = `${bar.label}: ${bar.value}`;
              return (
                <Bar
                  key={bar.id}
                  x={barX}
                  y={barY}
                  width={barWidth}
                  height={barHeight}
                  fill={bar.color ?? DEFAULT_BAR_COLOR}
                  role={interactive ? 'button' : 'img'}
                  tabIndex={interactive ? 0 : undefined}
                  aria-label={accessibleName}
                  cursor={interactive ? 'pointer' : undefined}
                  onClick={interactive ? () => onBarClick?.(bar.id) : undefined}
                  onKeyDown={
                    interactive
                      ? (event) => {
                          // activate on Enter/Space to match native button keyboard semantics
                          if (event.key === 'Enter' || event.key === ' ') {
                            event.preventDefault();
                            onBarClick?.(bar.id);
                          }
                        }
                      : undefined
                  }
                >
                  <title>{accessibleName}</title>
                </Bar>
              );
            })}
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
              tickValues={tickValues}
              stroke={AXIS_COLOR}
              tickStroke={AXIS_COLOR}
              tickFormat={(value) => truncateTick(labelById.get(String(value)) ?? String(value))}
              tickLabelProps={() => ({ fill: AXIS_COLOR, fontSize: 10, textAnchor: 'middle', dy: 4 })}
            />
          </Group>
        </svg>
      ) : null}
    </ChartContainer>
  );
};

export default VisxBarChart;
