import { FC } from 'react';

// project imports
import { Muted, Pre } from '@components/agentic/shared';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { useStageLogs } from '@utilities/queries/runs';

/** Props for {@link StageLogView}. */
interface StageLogViewProps {
  /** The run's group */
  group: string;
  /** The run's reaction id */
  reaction: string;
  /** The stage's name */
  stage: string;
  /** Whether the run is still working, or the stage's jobs can still run after it failed, so the logs keep reloading */
  running: boolean;
  /** When set, the time in milliseconds after which the logs stop reloading */
  liveUntil?: number;
}

/** The log lines of one stage, reloaded while the run works. */
export const StageLogView: FC<StageLogViewProps> = ({ group, reaction, stage, running, liveUntil }) => {
  const { data: lines, error } = useStageLogs(group, reaction, stage, running, liveUntil);
  // each state is its own block so it never runs on from the stage's Hide logs button; a failed read
  // keeps its error shown until the next successful read replaces it with the lines
  if (error !== '') {
    return (
      <AlertBanner severity={Severity.Error}>
        <span data-testid={`stage-logs-error-${stage}`}>{error}</span>
      </AlertBanner>
    );
  }
  if (lines === null) return <Muted as="div">Loading logs...</Muted>;
  if (lines.length === 0) {
    return (
      <Muted as="div" data-testid="stage-logs-empty">
        {running ? 'No log lines yet.' : 'This stage wrote no log lines.'}
      </Muted>
    );
  }
  return <Pre data-testid={`stage-logs-${stage}`}>{lines.join('\n')}</Pre>;
};
