// project imports
import { HuntLane } from '@models/entities/hunts';

/**
 * Clamp a fraction into [0, 1].
 *
 * @param fraction - The raw fraction to clamp.
 * @returns The clamped fraction.
 */
const clamp01 = (fraction: number): number => Math.min(1, Math.max(0, fraction));

/**
 * The time width of a lane in milliseconds (lanes walk newest `start` to oldest `end`).
 *
 * @param lane - The lane to measure.
 * @returns The width in milliseconds; 0 or negative means a degenerate lane.
 */
const laneWidthMs = (lane: HuntLane): number => Date.parse(lane.start) - Date.parse(lane.end);

/**
 * How far one lane has walked its time range, as a fraction in [0, 1].
 *
 * Hunt progress is deliberately time-based: `current` is the oldest upload timestamp the
 * lane has checkpointed, so the fraction is its position inside `[end, start]`. There is
 * no item denominator — counters are raw tallies, never percentages. An exhausted lane is
 * always 1 regardless of `current`, and a degenerate (zero or negative width) lane counts
 * as done since it cannot hold any walkable range.
 *
 * @param lane - The lane to compute progress for.
 * @returns The lane's progress fraction in [0, 1].
 */
export function laneFraction(lane: HuntLane): number {
  // an exhausted lane is done no matter where its current timestamp sits
  if (lane.exhausted) {
    return 1;
  }
  // guard unparseable timestamps (blank entities) before any math
  const width = laneWidthMs(lane);
  if (Number.isNaN(width)) {
    return 0;
  }
  // a zero or negative width lane has no walkable range left
  if (width <= 0) {
    return 1;
  }
  // position of current inside [end, start], clamped against clock skew on either side
  const walked = Date.parse(lane.start) - Date.parse(lane.current);
  if (Number.isNaN(walked)) {
    return 0;
  }
  return clamp01(walked / width);
}

/**
 * Overall hunt progress as the width-weighted mean of its lanes' fractions.
 *
 * Lane widths differ (the remainder of an uneven split lands on the newest lanes), so a
 * plain mean would overweight narrow lanes. Degenerate lanes carry zero weight; when every
 * lane is degenerate the hunt is done only if every lane is exhausted.
 *
 * @param lanes - The hunt's lanes.
 * @returns The overall progress fraction in [0, 1]; an empty lane list is 0.
 */
export function overallProgress(lanes: HuntLane[]): number {
  // no lanes means nothing to measure (a blank or failed-at-create hunt)
  if (lanes.length === 0) {
    return 0;
  }
  // accumulate width-weighted fractions, skipping degenerate widths
  let totalWidth = 0;
  let walkedWidth = 0;
  for (const lane of lanes) {
    const width = laneWidthMs(lane);
    if (Number.isNaN(width) || width <= 0) {
      continue;
    }
    totalWidth += width;
    walkedWidth += width * laneFraction(lane);
  }
  // every lane was degenerate: done only when every lane is exhausted
  if (totalWidth <= 0) {
    return lanes.every((lane) => lane.exhausted) ? 1 : 0;
  }
  return clamp01(walkedWidth / totalWidth);
}
