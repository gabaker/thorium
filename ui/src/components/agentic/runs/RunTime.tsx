import { FC } from 'react';
import styled from 'styled-components';

// project imports
import { describeTime } from '@utilities/time';

/** A time whose overdue state is drawn in the warning colour beside the text. */
const TimeText = styled.time<{ $overdue: boolean }>`
  ${({ $overdue }) => ($overdue ? 'color: var(--thorium-warning-bg); font-weight: 600;' : '')}
`;

/** Props for {@link RunTime}. */
interface RunTimeProps {
  /** The timestamp text from the API */
  value: string;
  /** Whether a past time is overdue, such as the SLA of a run that has not finished */
  overdueWhenPast?: boolean;
  /** The test id */
  testId?: string;
}

/** A run timestamp as a relative time with the local date and time, and the full ISO time as its tooltip. */
export const RunTime: FC<RunTimeProps> = ({ value, overdueWhenPast = false, testId }) => {
  const time = describeTime(value);
  if (!time.valid) return <span data-testid={testId}>{value}</span>;
  const overdue = overdueWhenPast && time.past;
  return (
    <TimeText dateTime={time.iso} title={time.iso} $overdue={overdue} data-testid={testId} data-overdue={overdue || undefined}>
      {time.relative} ({time.absolute}){overdue ? ', overdue' : ''}
    </TimeText>
  );
};
