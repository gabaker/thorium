// spec: ./SPEC.md
import React, { useMemo } from 'react';
import styled from 'styled-components';

// project imports
import { decodeText } from './detect';
import { FileRendererProps } from './types';
import Markdown from '@components/shared/syntax/Markdown';

const MarkdownScroll = styled.div`
  padding: 8px 12px;
  min-width: 0;
  color: var(--thorium-text);
`;

/** Render bytes as formatted markdown via the shared {@link Markdown} component. */
const MarkdownRenderer: React.FC<FileRendererProps> = ({ input }) => {
  const text = useMemo(() => input.text ?? decodeText(input.bytes), [input.text, input.bytes]);
  return (
    <MarkdownScroll>
      <Markdown>{text}</Markdown>
    </MarkdownScroll>
  );
};

export default MarkdownRenderer;
