import React, { forwardRef } from 'react';
import styled, { css } from 'styled-components';

/** The colour family of a {@link Badge}. */
export enum BadgeTone {
  Neutral = 'neutral',
  Ok = 'ok',
  Warn = 'warn',
  Error = 'error',
  Info = 'info',
  Active = 'active',
  Legacy = 'legacy',
  Accent = 'accent',
}

/** The size of a {@link Badge}. */
export enum BadgeSize {
  Small = 'sm',
  Medium = 'md',
}

/** The theme colour of each tone; the blue tones fall back to the link colour in a theme that doesn't set them. */
const BADGE_COLORS: Record<BadgeTone, string> = {
  [BadgeTone.Neutral]: 'var(--thorium-secondary-text)',
  [BadgeTone.Ok]: 'var(--thorium-ok-bg)',
  [BadgeTone.Warn]: 'var(--thorium-warning-bg)',
  [BadgeTone.Error]: 'var(--thorium-danger-bg)',
  [BadgeTone.Info]: 'var(--thorium-badge-info, var(--thorium-link-text))',
  [BadgeTone.Active]: 'var(--thorium-badge-active, var(--thorium-link-text))',
  [BadgeTone.Legacy]: 'var(--thorium-warning-secondary-bg)',
  [BadgeTone.Accent]: 'var(--thorium-badge-accent, var(--thorium-link-text))',
};

/** The radius of a badge: nearly square sides, a little tighter on a small badge. */
const BADGE_RADIUS = 'var(--thorium-badge-radius, 4px)';

/** The radius of a small badge. */
const BADGE_RADIUS_SMALL = 'var(--thorium-badge-radius-sm, 3px)';

/** The styled badge box; {@link Badge} maps its public props onto these transient ones. */
const BadgeBox = styled.span<{ $color: string; $size: BadgeSize; $wrap: boolean; $count: boolean }>`
  display: inline-flex;
  align-items: center;
  justify-content: center;
  vertical-align: middle;
  gap: 0.25rem;
  margin: 0;
  border-radius: ${({ $size }) => ($size === BadgeSize.Small ? BADGE_RADIUS_SMALL : BADGE_RADIUS)};
  padding: ${({ $size }) => ($size === BadgeSize.Small ? '0 0.35rem' : '0.1rem 0.5rem')};
  font-size: ${({ $size }) => ($size === BadgeSize.Small ? '0.7rem' : '0.75rem')};
  font-weight: 600;
  /* a line no taller than the cell text, so a badge in a table row doesn't push the row taller or off its baseline */
  line-height: 1.4;
  color: var(--thorium-text);
  border: 1px solid ${({ $color }) => $color};
  background: color-mix(in srgb, ${({ $color }) => $color} 22%, transparent);
  ${({ $wrap }) =>
    $wrap
      ? css`
          /* flow layout, so the text (and any break hints inside it) wraps as one run instead of as flex items */
          display: inline-block;
          white-space: normal;
          overflow-wrap: anywhere;
        `
      : css`
          white-space: nowrap;
        `}
  ${({ $count }) =>
    $count &&
    css`
      min-width: 1.5rem;
      font-variant-numeric: tabular-nums;
    `}
`;

/** Props for {@link Badge}. */
export interface BadgeProps extends React.HTMLAttributes<HTMLSpanElement> {
  /** The colour family, Neutral unless given */
  tone?: BadgeTone;
  /** An alias of `tone`, kept for the pages that pass it the styled-components way */
  $tone?: BadgeTone;
  /** The size, Medium unless given */
  size?: BadgeSize;
  /** Whether the text wraps inside its cell, so a long value narrows a table instead of widening it */
  wrap?: boolean;
  /** Whether the badge holds a count: a minimum width and digits of equal width */
  count?: boolean;
  /** A CSS colour that replaces the tone's, for a badge whose colour is set per value (such as a role) */
  color?: string;
}

/**
 * A small label with nearly square sides, used for kinds, statuses, counts, and legacy markers. It has no outer margin:
 * a run of badges is spaced by the gap of its container (`InlineList`), and it centres on the line it sits in.
 */
export const Badge = forwardRef<HTMLSpanElement, BadgeProps>(
  ({ tone, $tone, size = BadgeSize.Medium, wrap = false, count = false, color, ...rest }, ref) => (
    <BadgeBox
      ref={ref}
      $color={color ?? BADGE_COLORS[tone ?? $tone ?? BadgeTone.Neutral]}
      $size={size}
      $wrap={wrap}
      $count={count}
      {...rest}
    />
  ),
);

Badge.displayName = 'Badge';

/**
 * Get the theme colour of a tone, for a status icon or accent drawn beside the badges rather than as one.
 *
 * @param tone - The tone.
 * @returns The colour, as a `--thorium-*` variable.
 */
export function badgeColor(tone: BadgeTone): string {
  return BADGE_COLORS[tone];
}

/**
 * A badge whose text wraps inside its cell.
 *
 * @param props - The badge props; `wrap` is always on.
 * @returns The badge.
 */
export const WrapBadge = forwardRef<HTMLSpanElement, BadgeProps>((props, ref) => <Badge ref={ref} {...props} wrap />);

WrapBadge.displayName = 'WrapBadge';

/** The statuses whose badge is green: done and usable. */
const OK_STATUSES = ['completed', 'ready'];

/** The statuses whose badge is red: failed, unusable, or banned. */
const ERROR_STATUSES = ['failed', 'invalid', 'banned'];

/** The statuses whose badge shows work in progress. */
const ACTIVE_STATUSES = ['started', 'running'];

/** The statuses whose badge warns that something is missing or on its way out. */
const WARN_STATUSES = ['no live version', 'deprecated', 'no ready instance', 'stopped'];

/**
 * Pick the badge tone of a status, so the same status has the same colour on every page.
 *
 * Completed and Ready are Ok; Failed, Invalid, and banned are Error; Started and Running are Active; no live version,
 * deprecated, no ready instance, and a stage stopped when its run ended are Warn; Created, Sleeping, and anything else are Neutral.
 *
 * @param status - The status as the API or the page names it; case doesn't matter.
 * @returns The tone to draw the status badge in.
 */
export function statusTone(status: string): BadgeTone {
  const key = status.trim().toLowerCase();
  if (OK_STATUSES.includes(key)) return BadgeTone.Ok;
  if (ERROR_STATUSES.includes(key)) return BadgeTone.Error;
  if (ACTIVE_STATUSES.includes(key)) return BadgeTone.Active;
  if (WARN_STATUSES.includes(key)) return BadgeTone.Warn;
  return BadgeTone.Neutral;
}
