import { FC } from 'react';

// project imports
import { LaunchSection, NoticeBanner } from './LaunchNotices';
import { entityOptions, SearchPicker } from '@components/agentic/pickers';
import { EntityLink, Muted } from '@components/agentic/shared';
import { Severity } from '@components/shared/alerts/AlertBanner';
import { LoadingSpinner } from '@components/shared/fallback/LoadingSpinner';
import { AgenticListQueryState } from '@utilities/queries/agentic';
import { Workflow } from '@models/workflows';

/** Props for {@link WorkflowLaunch}. */
interface WorkflowLaunchProps {
  /** The workflows the user can see */
  workflows: AgenticListQueryState<Workflow>;
  /** The ids of the chosen workflows */
  selected: string[];
  /** Called with the ids of the chosen workflows */
  onSelect: (ids: string[]) => void;
}

/** The Workflows tab: a searchable list of the preconfigured workflows to add to the runs to launch. */
export const WorkflowLaunch: FC<WorkflowLaunchProps> = ({ workflows, selected, onSelect }) => (
  <LaunchSection>
    <h2>Choose workflows</h2>
    {/* the picker decides what can be run, so a failed or cut-off workflow listing is shown with it */}
    {workflows.error !== '' && (
      <NoticeBanner severity={Severity.Error}>
        <span data-testid="workflows-error">{workflows.error}</span>
      </NoticeBanner>
    )}
    <LoadingSpinner loading={workflows.loading} />
    {!workflows.loading && workflows.items.length === 0 && (
      <p data-testid="no-workflows">
        <Muted>There are no workflows in your groups yet. </Muted>
        <EntityLink to="/workflows">Browse workflows</EntityLink>
        <Muted> or </Muted>
        <EntityLink to="/launch?tab=tool">run a single tool</EntityLink>
        <Muted> instead.</Muted>
      </p>
    )}
    {!workflows.loading && workflows.items.length > 0 && (
      <SearchPicker
        label="Workflows"
        hint="Each chosen workflow runs its latest Ready instance."
        testId="run-workflow"
        options={entityOptions(workflows.items)}
        selected={selected}
        onChange={onSelect}
      />
    )}
  </LaunchSection>
);
