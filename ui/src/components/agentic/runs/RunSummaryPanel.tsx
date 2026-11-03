import { FC, ReactNode } from 'react';

// project imports
import { RunTags } from './RunTags';
import { RunTime } from './RunTime';
import { InputRow, inputRows, runProgressText } from './stages';
import { StatusIcon, StatusPill } from './StatusIndicator';
import { EntityLink, KeyValues, Mono, Muted, Section } from '@components/agentic/shared';
import { isRunFinished, runKind, userRunTags } from '@utilities/runs';
import { Reaction } from '@models/reactions';

/** Props for {@link RunSummaryPanel}. */
interface RunSummaryPanelProps {
  /** The run */
  run: Reaction;
  /** The file names the input samples were submitted under, keyed by SHA256 */
  names?: Record<string, string>;
  /** The number of steps in the run's stage order; the plan's step count when left out */
  steps?: number;
  /** Rows only one page shows, as `dt`/`dd` pairs after the shared rows */
  children?: ReactNode;
}

/** The status, kind, creator, SLA, relations, progress, tags, and inputs of a run. */
export const RunSummaryPanel: FC<RunSummaryPanelProps> = ({ run, names = {}, steps, children }) => {
  const stepCount = steps ?? run.plan?.stages.length ?? 0;
  const inputs = inputRows(run);
  return (
    <Section data-testid="run-summary">
      <h2>Summary</h2>
      <KeyValues>
        <dt>Status</dt>
        <dd>
          <StatusPill status={run.status} testId="run-status" />
        </dd>
        <dt>Kind</dt>
        <dd data-testid="run-kind">{runKind(run)}</dd>
        <dt>Run</dt>
        <dd>{run.pipeline}</dd>
        <dt>Reaction</dt>
        <dd>
          <Mono>{run.id}</Mono>
        </dd>
        <dt>Group</dt>
        <dd>{run.group}</dd>
        <dt>Creator</dt>
        <dd data-testid="run-creator">{run.creator}</dd>
        <dt>SLA expires</dt>
        <dd>
          {/* an SLA that passed while the run still works is overdue */}
          <RunTime value={run.sla} overdueWhenPast={!isRunFinished(run.status)} testId="run-sla" />
        </dd>
        <dt>Progress</dt>
        <dd data-testid="run-progress">
          <StatusIcon status={run.status} gap labelled />
          <Muted data-testid="run-stage-text">{runProgressText(run, stepCount)}</Muted>
        </dd>
        {run.parent && (
          <>
            <dt>Parent</dt>
            <dd>
              <EntityLink to={`/runs/${run.group}/${run.parent}`}>{run.parent}</EntityLink>
            </dd>
          </>
        )}
        {run.sub_reactions > 0 && (
          <>
            <dt>Sub reactions</dt>
            <dd>
              {run.completed_sub_reactions} of {run.sub_reactions} completed
            </dd>
          </>
        )}
        {userRunTags(run).length > 0 && (
          <>
            <dt>Tags</dt>
            <dd>
              <RunTags run={run} />
            </dd>
          </>
        )}
        {inputs.includes(InputRow.Samples) && (
          <>
            <dt>Input files</dt>
            <dd data-testid="run-input-samples">
              {run.samples.map((sha) => (
                <div key={sha}>
                  <EntityLink to={`/file/${sha}`}>
                    <Mono>{sha}</Mono>
                  </EntityLink>
                  {names[sha] && <Muted data-testid="run-input-name"> ({names[sha]})</Muted>}
                </div>
              ))}
            </dd>
          </>
        )}
        {inputs.includes(InputRow.Repos) && (
          <>
            <dt>Input repos</dt>
            <dd data-testid="run-input-repos">
              {run.repos.map((repo) => (
                <div key={`${repo.url}${repo.commitish ?? ''}`}>
                  <Mono>{repo.url}</Mono>
                  {repo.commitish ? ` @ ${repo.commitish}` : ''}
                </div>
              ))}
            </dd>
          </>
        )}
        {inputs.includes(InputRow.None) && (
          <>
            <dt>Inputs</dt>
            <dd data-testid="run-no-inputs">
              <Muted>No inputs</Muted>
            </dd>
          </>
        )}
        {children}
      </KeyValues>
    </Section>
  );
};
