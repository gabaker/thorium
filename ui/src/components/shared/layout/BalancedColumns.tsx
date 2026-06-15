import React, { useCallback, useLayoutEffect, useMemo, useRef, useState } from 'react';
import styled from 'styled-components';

// project imports
import { spacers } from '@styles';

// spec: ./BalancedColumns.spec.md

/** Which of the two columns a flowable item is assigned to (two-column back-compat API). */
export enum ColumnSide {
  /** The left column (hosting the static `left` slot). */
  Left = 'left',
  /** The right column (hosting the static `right` slot). */
  Right = 'right',
}

/** The 0-based column assignment for every anchor and item, index-aligned with the inputs. */
export interface ColumnAssignment {
  /** The column index each anchor renders in, index-aligned with the anchor list. */
  anchorCols: number[];
  /** The column index each flowable item renders in, index-aligned with the item list. */
  itemCols: number[];
}

/**
 * How many columns of at least `columnWidth` px fit in `containerWidth` px given `gap` px between
 * columns: the largest n with n*columnWidth + (n-1)*gap <= containerWidth, i.e.
 * floor((containerWidth + gap) / (columnWidth + gap)), clamped to a minimum of 1.
 *
 * @param containerWidth - The measured container width in px.
 * @param columnWidth - The minimum column width in px.
 * @param gap - The gap between adjacent columns in px.
 * @returns The number of columns that fit, always at least 1.
 */
export function computeColumnCount(containerWidth: number, columnWidth: number, gap: number): number {
  // a non-positive divisor would yield Infinity/NaN columns, so degrade to a single column
  if (columnWidth + gap <= 0) {
    return 1;
  }
  return Math.max(1, Math.floor((containerWidth + gap) / (columnWidth + gap)));
}

/**
 * Find the index of the shortest running column, with ties going to the lowest index so tiles
 * fill reading order first.
 *
 * @param heights - The running height of each column.
 * @returns The index of the shortest column.
 */
function shortestColumn(heights: number[]): number {
  let shortest = 0;
  for (let col = 1; col < heights.length; col++) {
    // strict comparison keeps the earliest column on ties
    if (heights[col] < heights[shortest]) {
      shortest = col;
    }
  }
  return shortest;
}

/**
 * Greedy N-column assignment. Anchors seed columns 0..N-1 in order (anchors beyond the column
 * count balance into the shortest column like items); items then balance into the shortest
 * running column. Ties pick the lowest index. Because all columns are equal width, a tile's
 * height is column-independent and the greedy result is stable across re-measures.
 *
 * @param columnCount - The number of columns to assign into (clamped to at least 1).
 * @param anchorHeights - The measured heights of the anchor tiles, in priority order.
 * @param itemHeights - The measured heights of the flowable items, in placement order.
 * @returns The 0-based column index for every anchor and item, index-aligned with the inputs.
 */
export function assignColumnsN(columnCount: number, anchorHeights: number[], itemHeights: number[]): ColumnAssignment {
  // clamp so a degenerate count still produces a valid single-column stack
  const count = Math.max(1, columnCount);
  const heights: number[] = new Array<number>(count).fill(0);
  // anchors pin their own column while columns remain; overflow anchors balance like items —
  // by the time an overflow index is reached every seed is already placed, so seeding order holds
  const anchorCols = anchorHeights.map((height, i) => {
    const col = i < count ? i : shortestColumn(heights);
    heights[col] += height;
    return col;
  });
  // each item flows, in order, into whichever column is currently shortest
  const itemCols = itemHeights.map((height) => {
    const col = shortestColumn(heights);
    heights[col] += height;
    return col;
  });
  return { anchorCols, itemCols };
}

/**
 * Greedily assign each flowable item, in order, to whichever of two columns is currently shorter
 * (ties go left). Back-compat wrapper over {@link assignColumnsN} for the two-column API.
 *
 * @param baseLeftHeight - The measured height of the static left slot.
 * @param baseRightHeight - The measured height of the static right slot.
 * @param itemHeights - The measured heights of the flowable items, in placement order.
 * @returns The column each item should render in, index-aligned with `itemHeights`.
 */
export function assignColumns(baseLeftHeight: number, baseRightHeight: number, itemHeights: number[]): ColumnSide[] {
  // two columns seeded by the left/right slots; column 0 is left and column 1 is right
  const { itemCols } = assignColumnsN(2, [baseLeftHeight, baseRightHeight], itemHeights);
  return itemCols.map((col) => (col === 0 ? ColumnSide.Left : ColumnSide.Right));
}

/**
 * The px improvement in max-column imbalance a new assignment must beat before it replaces the
 * previous one. Sub-pixel measurement jitter (a 1px reflow) must not re-parent heavy tiles, which
 * would tear down their DOM subtree (WebGL canvas, editor, scroll position); requiring a meaningful
 * gain gives the layout hysteresis so it only retiles when the balance genuinely improves.
 */
export const ASSIGNMENT_HYSTERESIS_PX = 24;

/**
 * The tallest column total under an assignment: the metric the greedy balancer minimizes and the
 * basis for the hysteresis comparison in {@link shouldReplaceAssignment}.
 *
 * @param assignment - The column assignment to score.
 * @param columnCount - The number of columns to sum into.
 * @param anchorHeights - The measured anchor heights, index-aligned with `assignment.anchorCols`.
 * @param itemHeights - The measured item heights, index-aligned with `assignment.itemCols`.
 * @returns The height of the tallest column under this assignment.
 */
export function maxColumnHeight(assignment: ColumnAssignment, columnCount: number, anchorHeights: number[], itemHeights: number[]): number {
  const heights = new Array<number>(Math.max(1, columnCount)).fill(0);
  assignment.anchorCols.forEach((col, i) => {
    if (col < heights.length) heights[col] += anchorHeights[i] ?? 0;
  });
  assignment.itemCols.forEach((col, i) => {
    if (col < heights.length) heights[col] += itemHeights[i] ?? 0;
  });
  return heights.reduce((tallest, h) => (h > tallest ? h : tallest), 0);
}

/**
 * Decide whether a freshly computed assignment should replace the previous one.
 *
 * Replaces when there is no previous assignment, when the tile set changed shape (different anchor
 * or item counts, so the old assignment no longer applies), or when the new assignment improves the
 * max-column imbalance by more than {@link ASSIGNMENT_HYSTERESIS_PX}. An identical assignment is
 * never a replacement (the caller keeps the previous object identity to avoid a re-render loop).
 *
 * @param prev - The previous assignment, or `null` on first measure.
 * @param next - The newly computed assignment.
 * @param columnCount - The active column count.
 * @param anchorHeights - The measured anchor heights.
 * @param itemHeights - The measured item heights.
 * @returns `true` when `next` should be committed in place of `prev`.
 */
export function shouldReplaceAssignment(
  prev: ColumnAssignment | null,
  next: ColumnAssignment,
  columnCount: number,
  anchorHeights: number[],
  itemHeights: number[],
): boolean {
  // no baseline to keep, or the tile set changed shape — the previous assignment can't apply
  if (prev === null || prev.anchorCols.length !== next.anchorCols.length || prev.itemCols.length !== next.itemCols.length) {
    return true;
  }
  // identical placement: keep the previous object so React sees no change
  const identical =
    prev.anchorCols.every((col, i) => col === next.anchorCols[i]) && prev.itemCols.every((col, i) => col === next.itemCols[i]);
  if (identical) return false;
  // different placement: only adopt it if it meaningfully improves the tallest-column balance
  const prevMax = maxColumnHeight(prev, columnCount, anchorHeights, itemHeights);
  const nextMax = maxColumnHeight(next, columnCount, anchorHeights, itemHeights);
  return prevMax - nextMax > ASSIGNMENT_HYSTERESIS_PX;
}

/** Parse a CSS gap string (e.g. `16px`, `1rem`) to px; falls back to 0 for unparseable values. */
function parseGapPx(gap: string): number {
  const value = parseFloat(gap);
  if (Number.isNaN(value)) return 0;
  // rem/em fall back to the root font size; anything else is treated as px
  if (/rem$|em$/.test(gap.trim())) {
    const rootFont = typeof document !== 'undefined' ? parseFloat(getComputedStyle(document.documentElement).fontSize) : 16;
    return value * (Number.isNaN(rootFont) ? 16 : rootFont);
  }
  return value;
}

/// The N-column grid: equal `minmax(0, 1fr)` tracks so every column can shrink without
/// overflowing, and `align-items: start` so each column flows to its own height.
const ColumnsGrid = styled.div<{ $columns: number; $gap: string }>`
  display: grid;
  grid-template-columns: repeat(${({ $columns }) => $columns}, minmax(0, 1fr));
  align-items: start;
  gap: ${({ $gap }) => $gap};
`;

/// One column: a flex stack whose row gap matches the grid gap so flowed items read as grid rows.
const Column = styled.div<{ $gap: string }>`
  display: flex;
  flex-direction: column;
  gap: ${({ $gap }) => $gap};
  min-width: 0;
`;

/// A measured wrapper around an anchor/item; full width with `min-width: 0` so the wrapped tile's
/// own sizing (e.g. an aspect-ratio square deriving height from the column width) is unchanged.
const MeasuredSlot = styled.div`
  width: 100%;
  min-width: 0;
`;

/** Props for {@link BalancedColumns}. */
export interface BalancedColumnsProps {
  /** Prioritized initial tiles: anchors[i] seeds column i (left-to-right) for the first N columns. */
  anchors?: React.ReactNode[];
  /** Back-compat convenience for the 2-column case; equivalent to anchors=[left, right]. */
  left?: React.ReactNode;
  /** Back-compat convenience for the 2-column case; equivalent to anchors=[left, right]. */
  right?: React.ReactNode;
  /** Ordered flowable tiles placed into whichever column is currently shortest. */
  items: React.ReactNode[];
  /** Fixed column count. Mutually exclusive with columnWidth (columns wins if both are set). */
  columns?: number;
  /**
   * Minimum column width in px; the actual column count is derived from the measured container
   * width (columns stretch to fill, so the real width is >= columnWidth). Mutually exclusive
   * with columns.
   */
  columnWidth?: number;
  /** The column and row gap (defaults to the standard tile spacer). */
  gap?: string;
  /** Optional class name for the grid root (styled-components extension). */
  className?: string;
}

/**
 * A "balanced columns" (greedy masonry) layout: an ordered list of anchor tiles seeds the top of
 * each column in priority order, and an ordered list of flowable tiles is greedily placed into
 * whichever column is currently shortest, so no column strands dead space beneath a short
 * neighbor. The column count is fixed (`columns`), derived from a minimum column width
 * (`columnWidth`), or defaults to the number of anchors (so `left`/`right` yield two columns).
 *
 * Heights are measured from the DOM (`offsetHeight`) and re-measured via a single persistent
 * `ResizeObserver`, so the placement tracks content growth, collapses, and viewport resizes; the
 * root is observed too so width changes recompute the derived column count. Observer callbacks are
 * coalesced with `requestAnimationFrame` so a burst of resizes triggers one measure. Re-tiling is
 * damped by {@link ASSIGNMENT_HYSTERESIS_PX} so sub-pixel jitter never re-parents a heavy tile.
 *
 * @param anchors - Prioritized tiles seeding columns 0..N-1; extras balance like items.
 * @param left - Back-compat static left slot (equivalent to anchors[0]).
 * @param right - Back-compat static right slot (equivalent to anchors[1]).
 * @param items - Ordered flowable tiles placed into the shortest column.
 * @param columns - Fixed column count (takes precedence over columnWidth).
 * @param columnWidth - Minimum column width in px used to derive the column count.
 * @param gap - The column and row gap (defaults to `spacers.four`).
 * @param className - Optional class name for the grid root.
 * @returns The balanced N-column grid.
 */
export const BalancedColumns: React.FC<BalancedColumnsProps> = ({
  anchors,
  left,
  right,
  items,
  columns,
  columnWidth,
  gap = spacers.four,
  className,
}) => {
  // resolve the anchor list: explicit anchors win, else the left/right convenience slots
  const resolvedAnchors: React.ReactNode[] =
    anchors ?? (left !== undefined || right !== undefined ? [left, right].filter((slot) => slot !== undefined) : []);
  const rootRef = useRef<HTMLDivElement | null>(null);
  // index-aligned refs to the currently-rendered anchor/item wrappers (nulls while unmounted)
  const anchorRefs = useRef<(HTMLDivElement | null)[]>([]);
  const itemRefs = useRef<(HTMLDivElement | null)[]>([]);
  // one persistent ResizeObserver reused for the root and every tile so an assignment change does
  // not tear down and rebuild the observer; MeasuredSlot ref callbacks observe/unobserve nodes
  const observerRef = useRef<ResizeObserver | null>(null);
  // pending rAF handle used to coalesce a burst of observer callbacks into a single measure
  const rafRef = useRef<number | null>(null);
  // parse the gap prop once per value instead of reading getComputedStyle on every measure
  const gapPx = useMemo(() => parseGapPx(gap), [gap]);
  const gapPxRef = useRef(gapPx);
  gapPxRef.current = gapPx;
  // width-derived column count; only meaningful in columnWidth mode, refreshed on every measure
  const [measuredColumns, setMeasuredColumns] = useState(1);
  // measured greedy assignment; null until the first layout-effect measure commits real heights, so
  // the very first commit renders an empty grid root and tiles appear on the second commit already
  // placed into the correct columns (no all-in-one-column flash + re-parent on first paint)
  const [assignment, setAssignment] = useState<ColumnAssignment | null>(null);
  // resolve the column count: fixed count, then width-derived, then one column per anchor
  const columnCount =
    columns !== undefined ? Math.max(1, columns) : columnWidth !== undefined ? measuredColumns : Math.max(1, resolvedAnchors.length);
  const anchorCount = resolvedAnchors.length;
  // in columnWidth mode the real column count isn't known until the root is measured; render an
  // empty grid root on the first commit so the layout-effect measure reads the true clientWidth, then
  // render the tiles on the second commit already placed into the correct number of columns (avoids
  // stacking every tile into one column and re-parenting them all once the width is known)
  const awaitingWidth = columns === undefined && columnWidth !== undefined && assignment === null;

  // measure the container (in columnWidth mode) and every anchor/item, recompute the column count
  // and greedy assignment, and commit each only when it actually changed (with hysteresis on the
  // assignment) so the re-observe effect below never loops and heavy tiles don't jitter-remount
  const measure = useCallback(() => {
    // derive the column count from live measurements so a resize retiles in the same pass
    let count: number;
    if (columns !== undefined) {
      count = Math.max(1, columns);
    } else if (columnWidth !== undefined) {
      const root = rootRef.current;
      count = computeColumnCount(root?.clientWidth ?? 0, columnWidth, gapPxRef.current);
      setMeasuredColumns((prev) => (prev === count ? prev : count));
    } else {
      count = Math.max(1, anchorCount);
    }
    const anchorHeights = anchorRefs.current.map((node) => node?.offsetHeight ?? 0);
    const itemHeights = itemRefs.current.map((node) => node?.offsetHeight ?? 0);
    const next = assignColumnsN(count, anchorHeights, itemHeights);
    setAssignment((prev) => (shouldReplaceAssignment(prev, next, count, anchorHeights, itemHeights) ? next : prev));
  }, [columns, columnWidth, anchorCount]);
  // hold the latest measure in a ref so the observer (created once, below) always calls the current
  // closure without needing to be rebuilt when measure's identity changes — rebuilding would drop the
  // per-tile observations set up by the ref callbacks
  const measureRef = useRef(measure);
  measureRef.current = measure;

  // create the single persistent observer once; it coalesces a burst of callbacks into one measure on
  // the next animation frame. Torn down (with any pending rAF) only on unmount, so the per-tile
  // observations from the ref callbacks survive every assignment/column-count change
  useLayoutEffect(() => {
    const observer = new ResizeObserver(() => {
      if (rafRef.current !== null) return;
      rafRef.current = requestAnimationFrame(() => {
        rafRef.current = null;
        measureRef.current();
      });
    });
    observerRef.current = observer;
    // ref callbacks that fired during the initial commit ran before this effect (when the observer
    // didn't exist yet), so observe whatever is already mounted now to catch that first paint
    if (rootRef.current) observer.observe(rootRef.current);
    for (const node of anchorRefs.current) if (node) observer.observe(node);
    for (const node of itemRefs.current) if (node) observer.observe(node);
    return () => {
      observer.disconnect();
      observerRef.current = null;
      if (rafRef.current !== null) {
        cancelAnimationFrame(rafRef.current);
        rafRef.current = null;
      }
    };
  }, []);

  // run a synchronous measure before paint whenever the tile lists or column count change, and keep
  // the root observed; individual tiles are observed via their MeasuredSlot ref callbacks
  useLayoutEffect(() => {
    anchorRefs.current.length = anchorCount;
    itemRefs.current.length = items.length;
    const observer = observerRef.current;
    if (observer && rootRef.current) {
      observer.observe(rootRef.current);
    }
    measure();
  }, [columnCount, anchorCount, items.length, measure]);

  // unmeasured anchors default to their own index column and unmeasured items to the last column
  // (matching the pre-measure static layout: left slot in column 0, right slot and items in
  // column 1); measured indices are clamped in case the count shrank before the next measure
  const anchorCols = resolvedAnchors.map((_, i) => Math.min(assignment?.anchorCols[i] ?? i, columnCount - 1));
  const itemCols = items.map((_, i) => Math.min(assignment?.itemCols[i] ?? columnCount - 1, columnCount - 1));

  // bucket the anchor/item indices per column in a single O(n) pass so rendering each column maps
  // over its own bucket instead of scanning every tile once per column (O(columns * n))
  const anchorBuckets: number[][] = Array.from({ length: columnCount }, () => []);
  anchorCols.forEach((col, i) => {
    if (col >= 0 && col < columnCount) anchorBuckets[col].push(i);
  });
  const itemBuckets: number[][] = Array.from({ length: columnCount }, () => []);
  itemCols.forEach((col, i) => {
    if (col >= 0 && col < columnCount) itemBuckets[col].push(i);
  });

  // observe a tile node when it mounts and unobserve when it unmounts, storing it index-aligned so
  // the measure pass reads live heights; the persistent observer is reused across assignment changes
  const setAnchorRef = useCallback((i: number, node: HTMLDivElement | null) => {
    const prev = anchorRefs.current[i];
    if (prev && prev !== node) observerRef.current?.unobserve(prev);
    anchorRefs.current[i] = node;
    if (node) observerRef.current?.observe(node);
  }, []);
  const setItemRef = useCallback((i: number, node: HTMLDivElement | null) => {
    const prev = itemRefs.current[i];
    if (prev && prev !== node) observerRef.current?.unobserve(prev);
    itemRefs.current[i] = node;
    if (node) observerRef.current?.observe(node);
  }, []);

  // wrappers are keyed by the tile's own key (or list index) so stable tiles keep their identity;
  // moving between columns necessarily remounts the DOM node, which the ref callbacks re-track
  const renderAnchor = (i: number) => {
    const anchor = resolvedAnchors[i];
    if (!React.isValidElement(anchor) || anchor.key == null) {
      // index-fallback keys can remount tiles on reorder; warn in dev so call sites add stable keys
      if (import.meta.env.DEV) {
        console.warn('BalancedColumns: anchor at index %d has no key; falling back to its index (may remount on reorder).', i);
      }
    }
    return (
      <MeasuredSlot
        key={`anchor-${React.isValidElement(anchor) && anchor.key != null ? anchor.key : i}`}
        ref={(node) => setAnchorRef(i, node)}
      >
        {anchor}
      </MeasuredSlot>
    );
  };
  const renderItem = (i: number) => {
    const item = items[i];
    if (!React.isValidElement(item) || item.key == null) {
      if (import.meta.env.DEV) {
        console.warn('BalancedColumns: item at index %d has no key; falling back to its index (may remount on reorder).', i);
      }
    }
    return (
      <MeasuredSlot key={React.isValidElement(item) && item.key != null ? item.key : i} ref={(node) => setItemRef(i, node)}>
        {item}
      </MeasuredSlot>
    );
  };

  return (
    <ColumnsGrid ref={rootRef} className={className} $columns={columnCount} $gap={gap}>
      {/* while awaiting the first width measure (columnWidth mode) the grid root renders empty so the
          measure reads the true clientWidth; tiles appear on the next commit already placed */}
      {!awaitingWidth &&
        Array.from({ length: columnCount }, (_, col) => (
          <Column key={col} $gap={gap}>
            {anchorBuckets[col]?.map((i) => renderAnchor(i))}
            {itemBuckets[col]?.map((i) => renderItem(i))}
          </Column>
        ))}
    </ColumnsGrid>
  );
};
