import { FC } from 'react';
import styled, { css, keyframes } from 'styled-components';

// project imports
import { statusIndicator } from './status';
import { Badge } from '@components/agentic/shared';

/** One turn of a spinning status icon. */
const spin = keyframes`
  from {
    transform: rotate(0deg);
  }
  to {
    transform: rotate(360deg);
  }
`;

/** The icon of a status, coloured by its tone, spinning for work in progress. */
const IconBox = styled.span<{ $color: string; $spin: boolean; $size: number; $gap: boolean }>`
  display: inline-flex;
  flex: none;
  align-items: center;
  vertical-align: -0.1em;
  margin-right: ${({ $gap }) => ($gap ? '0.25rem' : '0')};
  color: ${({ $color }) => $color};
  font-size: ${({ $size }) => $size}px;

  svg {
    ${({ $spin }) =>
      $spin &&
      css`
        animation: ${spin} 1.2s linear infinite;
      `}
  }

  @media (prefers-reduced-motion: reduce) {
    svg {
      animation: none;
    }
  }
`;

/** Props for {@link StatusIcon}. */
interface StatusIconProps {
  /** The status, in any of the API's spellings */
  status: string;
  /** The icon size in pixels */
  size?: number;
  /** Whether a gap follows the icon, for an icon before text */
  gap?: boolean;
  /** Whether the icon names its status to screen readers, for an icon with no status text beside it */
  labelled?: boolean;
}

/** A status icon; unless labelled, it is hidden from screen readers, so whatever holds it names the status in text. */
export const StatusIcon: FC<StatusIconProps> = ({ status, size = 12, gap = false, labelled = false }) => {
  const style = statusIndicator(status);
  const Icon = style.icon;
  return (
    <IconBox
      $color={style.color}
      $spin={style.spin}
      $size={size}
      $gap={gap}
      role={labelled ? 'img' : undefined}
      aria-label={labelled ? style.label : undefined}
      aria-hidden={labelled ? undefined : 'true'}
      title={labelled ? style.label : undefined}
      data-status={style.label}
    >
      <Icon />
    </IconBox>
  );
};

/** Props for {@link StatusPill}. */
interface StatusPillProps {
  /** The status, in any of the API's spellings */
  status: string;
  /** The pill's test id */
  testId?: string;
}

/** A status as a pill: icon, colour, and text label, so the status never rests on colour alone. */
export const StatusPill: FC<StatusPillProps> = ({ status, testId }) => {
  const style = statusIndicator(status);
  return (
    <Badge tone={style.tone} title={style.description} data-testid={testId} data-status={style.label}>
      <StatusIcon status={status} size={11} gap />
      {style.label}
    </Badge>
  );
};
