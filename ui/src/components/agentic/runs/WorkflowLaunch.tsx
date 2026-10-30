import { FC } from 'react';

// project imports
import { LaunchPickerSection } from './LaunchPickerSection';
import { entityOptions } from '@components/agentic/pickers';
import { EntityLink, Muted } from '@components/agentic/shared';
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
  /** The picker's search text, kept by the page so a tab switch doesn't clear it */
  search: string;
  /** Called with the search text as typed */
  onSearch: (search: string) => void;
}

/** The Workflows tab: a searchable list of the preconfigured workflows to add to the runs to launch. */
export const WorkflowLaunch: FC<WorkflowLaunchProps> = ({ workflows, selected, onSelect, search, onSearch }) => (
  <LaunchPickerSection
    title="Choose workflows"
    error={workflows.error}
    errorTestId="workflows-error"
    loading={workflows.loading}
    empty={
      workflows.items.length === 0 ? (
        <p data-testid="no-workflows">
          <Muted>There are no workflows in your groups yet. </Muted>
          <EntityLink to="/workflows">Browse workflows</EntityLink>
          <Muted> or </Muted>
          <EntityLink to="/launch?tab=tool">run a single tool</EntityLink>
          <Muted> instead.</Muted>
        </p>
      ) : undefined
    }
    label="Workflows"
    hint="Each chosen workflow runs its latest Ready instance."
    testId="run-workflow"
    options={entityOptions(workflows.items)}
    selected={selected}
    onSelect={onSelect}
    search={search}
    onSearch={onSearch}
  />
);
