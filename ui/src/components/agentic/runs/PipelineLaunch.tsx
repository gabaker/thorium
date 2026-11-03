import { FC } from 'react';

// project imports
import { LaunchPickerSection } from './LaunchPickerSection';
import { pipelineBlocker } from './pipelineRequest';
import { PickerOption } from '@components/agentic/pickers';
import { Severity } from '@components/shared/alerts/AlertBanner';
import { PipelineListQueryState } from '@utilities/queries/pipelines';
import { Pipeline } from '@models/pipelines';

/**
 * Describe pipelines as picker choices keyed by `<group>/<name>`, noting the group and a pipeline that can't run.
 *
 * @param pipelines - The pipelines.
 * @returns One choice per pipeline.
 */
export function pipelineOptions(pipelines: Pipeline[]): PickerOption[] {
  return pipelines.map((pipeline) => ({
    value: `${pipeline.group}/${pipeline.name}`,
    text: pipeline.name,
    note: pipelineBlocker(pipeline) ? `in ${pipeline.group}; cannot be run` : `in ${pipeline.group}`,
  }));
}

/** Props for {@link PipelineLaunch}. */
interface PipelineLaunchProps {
  /** The pipelines of the user's groups */
  pipelines: PipelineListQueryState;
  /** The chosen pipelines as `<group>/<name>` */
  selected: string[];
  /** Called with the chosen pipelines */
  onSelect: (keys: string[]) => void;
  /** The picker's search text, kept by the page so a tab switch doesn't clear it */
  search: string;
  /** Called with the search text as typed */
  onSearch: (search: string) => void;
}

/** The Pipelines tab: a searchable list of the static pipelines to add to the runs to launch. */
export const PipelineLaunch: FC<PipelineLaunchProps> = ({ pipelines, selected, onSelect, search, onSearch }) => (
  <LaunchPickerSection
    title="Choose pipelines"
    error={pipelines.errors.length > 0 ? `Some pipelines could not be listed: ${pipelines.errors.join('; ')}` : ''}
    errorTestId="pipeline-list-error"
    errorSeverity={Severity.Warning}
    loading={pipelines.loading}
    label="Pipelines"
    hint="A pipeline runs the tool behind each stage in order, in the pipeline's group."
    testId="run-pipeline"
    options={pipelineOptions(pipelines.pipelines)}
    selected={selected}
    onSelect={onSelect}
    emptyText="No pipelines in your groups."
    search={search}
    onSearch={onSearch}
  />
);
