import { FC } from 'react';

// project imports
import { CardTable } from './runs.styled';
import { StatusPill } from './StatusIndicator';
import { EntityLink, Muted, Section } from '@components/agentic/shared';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { useSubRuns } from '@utilities/queries/runs';
import { Reaction } from '@models/reactions';

/** Props for {@link RunSubRunsPanel}. */
interface RunSubRunsPanelProps {
  /** The run */
  run: Reaction;
}

/** The sub reactions a run started, such as a workflow run's tool calls, each linked to its own page and logs. */
export const RunSubRunsPanel: FC<RunSubRunsPanelProps> = ({ run }) => {
  const { data: subs, error } = useSubRuns(run);
  return (
    <Section data-testid="run-sub-runs">
      <h2>Sub reactions</h2>
      {error !== '' && (
        <AlertBanner severity={Severity.Error}>
          <span data-testid="run-sub-runs-error">{error}</span>
        </AlertBanner>
      )}
      {subs === null && error === '' && <Muted>Loading sub reactions...</Muted>}
      {subs?.length === 0 && error === '' && <Muted>No sub reactions were found.</Muted>}
      {subs && subs.length > 0 && (
        <CardTable>
          <thead>
            <tr>
              <th>Run</th>
              <th>Status</th>
            </tr>
          </thead>
          <tbody>
            {subs.map((sub) => (
              <tr key={sub.id} data-testid="sub-run-row">
                <td>
                  <EntityLink to={`/runs/${sub.group}/${sub.id}`}>{sub.pipeline}</EntityLink>
                </td>
                <td>
                  <StatusPill status={sub.status} />
                </td>
              </tr>
            ))}
          </tbody>
        </CardTable>
      )}
    </Section>
  );
};
