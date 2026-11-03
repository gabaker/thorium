import { FC, useEffect, useRef } from 'react';
import styled from 'styled-components';

// project imports
import { LaunchError } from './LaunchNotices';
import { LAUNCH_KIND_NOUNS, LaunchOutcome } from './launchForms';
import { Actions, Badge, BadgeTone, EntityLink, Muted } from '@components/agentic/shared';
import { Button, ButtonVariant } from '@components/shared/buttons';

/** The muted line beside the launch button, at the size of a field hint. */
const SubmitNote = styled(Muted)`
  font-size: 0.85rem;
`;

/** Props for {@link LaunchSubmit}. */
interface LaunchSubmitProps {
  /** The button label, naming the number of runs */
  label: string;
  /** Whether the runs are being started */
  busy: boolean;
  /** Why the runs can't start yet, shown beside the disabled button; `null` when they can */
  blocker: string | null;
  /** Starts the runs */
  onSubmit: () => void;
}

/** The launch button, with the reason it is disabled. */
export const LaunchSubmit: FC<LaunchSubmitProps> = ({ label, busy, blocker, onSubmit }) => (
  <Actions>
    <Button variant={ButtonVariant.Ok} data-testid="run-submit" disabled={busy || blocker !== null} onClick={onSubmit}>
      {busy ? 'Starting...' : label}
    </Button>
    {!busy && blocker !== null && <SubmitNote data-testid="run-submit-hint">{blocker}</SubmitNote>}
  </Actions>
);

/** The list of a launch's outcomes, set off from the launch button above it. */
const ResultList = styled.ul`
  list-style: none;
  margin: 0.75rem 0 0 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 0.5rem;
`;

/** One run's outcome: its kind, its name, and a link to the run or why it didn't start. */
const ResultRow = styled.li`
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.5rem;
  > div {
    flex: 1 1 100%;
  }
`;

/** Props for {@link LaunchResults}. */
interface LaunchResultsProps {
  /** What happened to each run of the last launch */
  outcomes: LaunchOutcome[];
}

/** What the last launch did: a link to each run it started and the reason for each one that failed. */
export const LaunchResults: FC<LaunchResultsProps> = ({ outcomes }) => {
  const list = useRef<HTMLUListElement>(null);
  // the results open below the button, so a long form scrolls them into view where the user is looking
  useEffect(() => {
    if (outcomes.length > 0) list.current?.scrollIntoView({ block: 'nearest' });
  }, [outcomes]);
  if (outcomes.length === 0) return null;
  const started = outcomes.filter((outcome) => outcome.error === '').length;
  return (
    <ResultList ref={list} data-testid="launch-results" aria-label="Launch results">
      <li>
        <Muted data-testid="launch-results-summary">
          Started {started} of {outcomes.length} run{outcomes.length === 1 ? '' : 's'}.
          {started < outcomes.length ? ' The runs that failed stay in the list above.' : ''}
        </Muted>
      </li>
      {outcomes.map((outcome) => (
        <ResultRow key={outcome.key} data-testid="launch-result" data-key={outcome.key}>
          <Badge tone={outcome.error === '' ? BadgeTone.Ok : BadgeTone.Error}>{outcome.error === '' ? 'started' : 'failed'}</Badge>
          <span>
            {LAUNCH_KIND_NOUNS[outcome.kind]} <strong>{outcome.name}</strong>
          </span>
          {outcome.error === '' ? (
            <span data-testid="launch-result-link">
              <EntityLink to={outcome.path}>View run</EntityLink>
            </span>
          ) : (
            <div>
              <LaunchError error={outcome.error} />
            </div>
          )}
        </ResultRow>
      ))}
    </ResultList>
  );
};
