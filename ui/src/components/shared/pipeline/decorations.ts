import type { ReactNode } from 'react';

/** How a read-only pipeline diagram draws one stage node beyond its name, such as a run's status for that stage. */
export interface StepDecoration {
  /** Drawn before the stage name, such as a status icon */
  icon?: ReactNode;
  /** The colour of the node's left accent, as a CSS colour (a `--thorium-*` variable) */
  accent?: string;
  /** The node's accessible name and tooltip, such as `clamav: Failed`; the stage name when left out */
  label?: string;
  /** Whether this node is the selected one */
  selected?: boolean;
  /** Whether this node is emphasised, such as a stage that is running now */
  active?: boolean;
  /** The test id of the node */
  testId?: string;
}

/** The width an icon decoration adds to a node: a 12px icon and the 6px gap after it. */
export const DECORATION_ICON_WIDTH = 18;

/**
 * Get the extra width the nodes of a diagram need for their decorations.
 *
 * @param decorations - The decorations by stage name, or `undefined` for a plain diagram.
 * @returns The pixels to add to every node so an icon never clips the name.
 */
export function decorationWidth(decorations?: Record<string, StepDecoration>): number {
  if (!decorations) return 0;
  return Object.values(decorations).some((decoration) => decoration.icon !== undefined) ? DECORATION_ICON_WIDTH : 0;
}
