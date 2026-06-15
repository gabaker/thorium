import React, { useLayoutEffect, useRef, useState } from 'react';
import styled from 'styled-components';

// project imports
import Markdown from '@components/shared/syntax/Markdown';

// spec: ./SPEC.md

const Clip = styled.div<{ $collapsed: boolean; $maxPx: number; $fade: boolean }>`
  position: relative;
  overflow: hidden;
  max-height: ${({ $collapsed, $maxPx }) => ($collapsed ? `${$maxPx}px` : 'none')};
  /* fade the clipped bottom edge to signal there's more, only while collapsed AND overflowing */
  ${({ $fade }) =>
    $fade &&
    `
    -webkit-mask-image: linear-gradient(to bottom, black 55%, transparent 100%);
    mask-image: linear-gradient(to bottom, black 55%, transparent 100%);
  `}

  /* tighten markdown block spacing for the summary preview context */
  p,
  ul,
  ol,
  pre,
  blockquote {
    margin: 0 0 0.4em;
  }
  p:last-child,
  ul:last-child,
  ol:last-child,
  pre:last-child {
    margin-bottom: 0;
  }
  ul,
  ol {
    padding-left: 1.2em;
  }
`;

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

interface CollapsibleMarkdownProps {
  /** The raw text; rendered as markdown (plain text formats fine, markdown syntax is honored). */
  children: string;
  /** Collapsed height cap in px; longer content is clipped behind an expand toggle. */
  collapsedMaxPx: number;
}

/**
 * Render (possibly long) text as markdown, collapsed to `collapsedMaxPx` by default with a Show
 * more/less toggle when the content overflows. Used for view-only description previews so a long
 * description never dominates the summary until the user opts to expand it.
 */
const CollapsibleMarkdown: React.FC<CollapsibleMarkdownProps> = ({ children, collapsedMaxPx }) => {
  const [collapsed, setCollapsed] = useState(true);
  const [overflowing, setOverflowing] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  // measure while collapsed (max-height + overflow:hidden still report the full scrollHeight); this
  // runs on mount/content change but not on toggle, so the button persists once expanded
  useLayoutEffect(() => {
    const el = ref.current;
    if (el) setOverflowing(el.scrollHeight > collapsedMaxPx + 1);
  }, [children, collapsedMaxPx]);

  return (
    <>
      <Clip ref={ref} $collapsed={collapsed} $maxPx={collapsedMaxPx} $fade={collapsed && overflowing}>
        <Markdown>{children}</Markdown>
      </Clip>
      {overflowing && (
        <ToggleButton type="button" onClick={() => setCollapsed((c) => !c)}>
          {collapsed ? 'Show more' : 'Show less'}
        </ToggleButton>
      )}
    </>
  );
};

export default CollapsibleMarkdown;
