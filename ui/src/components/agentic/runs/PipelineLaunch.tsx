import { FC } from 'react';

// project imports
import { LaunchSection, NoticeBanner } from './LaunchNotices';
import { pipelineBlocker } from './pipelineRequest';
import { PickerOption, SearchPicker } from '@components/agentic/pickers';
import { Severity } from '@components/shared/alerts/AlertBanner';
import { LoadingSpinner } from '@components/shared/fallback/LoadingSpinner';
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
}

/** The Pipelines tab: a searchable list of the static pipelines to add to the runs to launch. */
export const PipelineLaunch: FC<PipelineLaunchProps> = ({ pipelines, selected, onSelect }) => (
  <LaunchSection>
    <h2>Choose pipelines</h2>
    {pipelines.errors.length > 0 && (
      <NoticeBanner severity={Severity.Warning}>
        <span data-testid="pipeline-list-error">Some pipelines could not be listed: {pipelines.errors.join('; ')}</span>
      </NoticeBanner>
    )}
    <LoadingSpinner loading={pipelines.loading} />
    {!pipelines.loading && (
      <SearchPicker
        label="Pipelines"
        hint="A pipeline runs the tool behind each stage in order, in the pipeline's group."
        testId="run-pipeline"
        options={pipelineOptions(pipelines.pipelines)}
        selected={selected}
        onChange={onSelect}
        emptyText="No pipelines in your groups."
      />
    )}
  </LaunchSection>
);
