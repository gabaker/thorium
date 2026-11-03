import { FC } from 'react';
import styled from 'styled-components';

/** Monospace inline text for ids, hashes, and names. */
export const Mono = styled.code`
  font-family: monospace;
  color: inherit;
  font-size: 0.85em;
  overflow-wrap: anywhere;
`;

/** Props for {@link CodeSpans}. */
interface CodeSpansProps {
  /** Text from the API that may mark names with markdown backticks */
  text: string;
}

/** Render API text with its `backticked` names as inline code instead of literal backticks. */
export const CodeSpans: FC<CodeSpansProps> = ({ text }) => (
  <>
    {/* splitting on a capture group puts each backticked name at an odd index */}
    {text.split(/`([^`]*)`/).map((part, index) => (index % 2 === 1 ? <Mono key={index}>{part}</Mono> : part))}
  </>
);
