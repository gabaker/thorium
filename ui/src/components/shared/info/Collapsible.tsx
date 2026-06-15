import React, { useLayoutEffect, useRef, useState } from 'react';
import styled from 'styled-components';

// spec: ./SPEC.md

// project imports
import { spacers } from '@styles';

/**
 * The clipped content wrapper. While collapsed it caps its height at `$maxPx`. When `$scroll` is set the
 * collapsed cap becomes a scroll area (so the content can be browsed without expanding). The clipped
 * bottom edge fades (`mask-image`) while collapsed AND overflowing — the mask is anchored to the box's
 * bottom, so it stays a **static** bottom fade even as the scroll area scrolls.
 */
const Clip = styled.div<{ $collapsed: boolean; $maxPx: number; $fade: boolean; $scroll: boolean }>`
  position: relative;
  overflow: ${({ $collapsed, $scroll }) => ($collapsed && $scroll ? 'hidden auto' : 'hidden')};
  max-height: ${({ $collapsed, $maxPx }) => ($collapsed ? `${$maxPx}px` : 'none')};
  ${({ $fade }) =>
    $fade &&
    `
    -webkit-mask-image: linear-gradient(to bottom, black 55%, transparent 100%);
    mask-image: linear-gradient(to bottom, black 55%, transparent 100%);
  `}
`;

// the show-more/less (or caller-labeled) toggle button, styled as an inline link
const ToggleButton = styled.button`
  margin-top: 2px;
  padding: 0;
  background: transparent;
  border: none;
  color: var(--thorium-link-text);
  font-size: 0.78rem;
  font-weight: 600;
  cursor: pointer;

  &:hover {
    color: var(--thorium-highlight-text);
  }
`;

// centers the toggle horizontally when it is placed at the top of the collapsible
const TopToggleRow = styled.div`
  display: flex;
  justify-content: center;
  margin-bottom: ${spacers.two};
`;

// centers the toggle horizontally when it is placed below the content (adaptive/collapsed case)
const BottomToggleRow = styled.div`
  display: flex;
  justify-content: center;
  margin-top: ${spacers.two};
`;

/** Where the collapse toggle sits relative to the clipped content. */
export enum TogglePosition {
  /** Below the content (the default markdown/show-more pattern). */
  Bottom = 'bottom',
  /** Above the content, horizontally centered. */
  Top = 'top',
  /**
   * Below while collapsed (so it reads as "expand", under the fade), above once expanded (so the
   * "collapse" control sits at the top of the now-tall content).
   */
  Adaptive = 'adaptive',
}

/** Props for {@link Collapsible}. */
export interface CollapsibleProps {
  /** The content to clip; may be any node (markdown, a tags tile, arbitrary JSX). */
  children: React.ReactNode;
  /** Collapsed height cap in px; taller content is clipped behind the expand toggle. */
  maxPx: number;
  /**
   * Render the toggle label for the given collapsed state. Defaults to `Show more`/`Show less`. Callers
   * override to match their context (e.g. `⌄ filters` / `⌃ filters`).
   */
  renderToggleLabel?: (collapsed: boolean) => React.ReactNode;
  /** Whether to start collapsed. Defaults to `true`. */
  defaultCollapsed?: boolean;
  /**
   * Where the toggle sits relative to the content. `Bottom` (default) keeps the show-more/less pattern;
   * `Top` renders a horizontally-centered toggle above the content; `Adaptive` puts it at the bottom while
   * collapsed (under the fade, reading as "expand") and at the top once expanded (the "collapse" control).
   */
  togglePosition?: TogglePosition;
  /**
   * When true, the collapsed cap becomes a vertical scroll area so the content can be browsed without
   * expanding; the static bottom fade stays intact. Defaults to false (clip only, still faded).
   */
  scrollWhenCollapsed?: boolean;
  /** Optional class name applied to the clip wrapper (lets callers scope content styles). */
  className?: string;
}

/** The default toggle label when a caller supplies none. */
function defaultToggleLabel(collapsed: boolean): React.ReactNode {
  return collapsed ? 'Show more' : 'Show less';
}

/**
 * Clip arbitrary content to `maxPx` when collapsed, fading the clipped edge and offering a toggle only
 * when the content actually overflows.
 *
 * Generalizes the collapse/fade/measure pattern: it measures the inner content's `scrollHeight` against
 * `maxPx` via a `ResizeObserver` (which fires on mount AND whenever the content resizes — async loads,
 * grid reflow) so the toggle reliably appears once the content overflows, and short content shows no
 * chrome. The measure never keys off the toggle, so the toggle persists once expanded. When collapsed and
 * overflowing the bottom edge fades (static, anchored to the box); `scrollWhenCollapsed` additionally makes
 * the collapsed cap a scroll area (fade kept). Callers control the toggle label via `renderToggleLabel`.
 *
 * @param children - The content to clip.
 * @param maxPx - Collapsed height cap in px.
 * @param renderToggleLabel - Optional toggle-label renderer (defaults to `Show more`/`Show less`).
 * @param defaultCollapsed - Whether to start collapsed (defaults to `true`).
 * @param togglePosition - Where the toggle sits relative to the content (defaults to `Bottom`).
 * @param scrollWhenCollapsed - When true, the collapsed cap scrolls while keeping the static bottom fade.
 * @param className - Optional class name applied to the clip wrapper.
 * @returns The collapsible content plus its toggle (when overflowing).
 */
const Collapsible: React.FC<CollapsibleProps> = ({
  children,
  maxPx,
  renderToggleLabel = defaultToggleLabel,
  defaultCollapsed = true,
  togglePosition = TogglePosition.Bottom,
  scrollWhenCollapsed = false,
  className,
}) => {
  const [collapsed, setCollapsed] = useState(defaultCollapsed);
  const [overflowing, setOverflowing] = useState(false);
  // the inner, uncapped content wrapper: its full height drives the overflow measure (the Clip itself is
  // capped, so we measure the content, not the clip)
  const contentRef = useRef<HTMLDivElement>(null);

  // re-measure whenever the content's size changes (not just on mount) so async-loaded content — e.g. tags
  // that populate after the graph loads, or a grid that reflows on resize — reliably reveals the toggle
  useLayoutEffect(() => {
    const el = contentRef.current;
    if (!el) return;
    const measure = () => setOverflowing(el.scrollHeight > maxPx + 1);
    measure();
    if (typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => observer.disconnect();
  }, [maxPx]);

  const toggle = overflowing && (
    <ToggleButton type="button" onClick={() => setCollapsed((c) => !c)}>
      {renderToggleLabel(collapsed)}
    </ToggleButton>
  );

  // resolve Adaptive to a concrete side per collapsed state: bottom while collapsed, top once expanded
  const effectivePosition =
    togglePosition === TogglePosition.Adaptive ? (collapsed ? TogglePosition.Bottom : TogglePosition.Top) : togglePosition;
  // center the bottom toggle for Adaptive (e.g. the dashboard filters); plain Bottom (CollapsibleMarkdown's
  // show-more) keeps its inline, left-aligned placement
  const bottomToggle =
    effectivePosition === TogglePosition.Bottom && toggle ? (
      togglePosition === TogglePosition.Adaptive ? (
        <BottomToggleRow>{toggle}</BottomToggleRow>
      ) : (
        toggle
      )
    ) : null;

  return (
    <>
      {effectivePosition === TogglePosition.Top && toggle && <TopToggleRow>{toggle}</TopToggleRow>}
      <Clip className={className} $collapsed={collapsed} $maxPx={maxPx} $fade={collapsed && overflowing} $scroll={scrollWhenCollapsed}>
        <div ref={contentRef}>{children}</div>
      </Clip>
      {bottomToggle}
    </>
  );
};

export default Collapsible;
