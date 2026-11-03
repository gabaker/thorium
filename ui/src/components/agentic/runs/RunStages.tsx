import { FC, useCallback, useMemo, useReducer } from 'react';
import styled from 'styled-components';

// project imports
import { CardWideTable, InlineRow, SeparatedList } from './runs.styled';
import { RunTime } from './RunTime';
import {
  buildStageViews,
  INITIAL_STAGE_UI,
  logsOpenByDefault,
  logsShown,
  RunStagesView,
  StageView,
  stageUiReducer,
  workerPrefix,
} from './stages';
import { statusIndicator } from './status';
import { StageLogView } from './StageLogView';
import { StatusIcon, StatusPill } from './StatusIndicator';
import { Badge, EntityLink, InlineList, Mono, Muted, Section, TableScroll } from '@components/agentic/shared';
import { Button, ButtonSize, ButtonVariant } from '@components/shared/buttons';
import PipelineOrderFlow from '@components/shared/pipeline/PipelineOrderFlow';
import { StepDecoration } from '@components/shared/pipeline/decorations';
import { isRunFinished, runProvenance } from '@utilities/runs';
import { Reaction, StatusUpdate } from '@models/reactions';
import { ToolProvenance } from '@models/runs';

/** One stage's block; the selected stage is marked with a left bar so the diagram's selection is visible here too. */
const StageBlock = styled.div<{ $selected: boolean }>`
  scroll-margin-top: 4rem;
  padding-left: 0.75rem;
  box-shadow: inset 3px 0 0 ${({ $selected }) => ($selected ? 'var(--thorium-highlight-panel-border)' : 'transparent')};
`;

/** The space between a stage's header and its jobs or logs. */
const StageBody = styled.div`
  margin-top: 0.5rem;
  display: flex;
  flex-direction: column;
  gap: 0.5rem;
`;

/**
 * Get the element id of a stage's block, so a diagram node can scroll to it.
 *
 * @param stage - The stage's name.
 * @returns An id that is safe in a URL fragment.
 */
export function stageBlockId(stage: string): string {
  return `run-stage-${workerPrefix(stage) || 'stage'}`;
}

/**
 * Decorate each stage node with its status: the status icon, the status colour, the status in its
 * accessible name, the selection, and the running stage.
 *
 * @param stages - The stages of each step.
 * @param selected - The selected stage, or `null`.
 * @returns The decorations by stage name.
 */
export function stageDecorations(stages: StageView[][], selected: string | null): Record<string, StepDecoration> {
  const decorations: Record<string, StepDecoration> = {};
  for (const stage of stages.flat()) {
    const style = statusIndicator(stage.state);
    decorations[stage.name] = {
      icon: <StatusIcon status={stage.state} />,
      accent: style.color,
      label: `${stage.name}: ${style.label}`,
      selected: stage.name === selected,
      active: stage.active,
      testId: `stage-node-${stage.name}`,
    };
  }
  return decorations;
}

/** Props for {@link StageJobs}. */
interface StageJobsProps {
  /** The stage */
  stage: StageView;
  /** Whether the run still works */
  running: boolean;
}

/** The jobs of one stage with their status, worker, and times. */
const StageJobs: FC<StageJobsProps> = ({ stage, running }) => {
  if (stage.jobs.length === 0) {
    return <Muted as="div">{running ? 'No jobs yet.' : 'No jobs were recorded for this stage.'}</Muted>;
  }
  return (
    <TableScroll>
      <CardWideTable data-testid={`stage-jobs-${stage.name}`}>
        <thead>
          <tr>
            <th>Job</th>
            <th>Status</th>
            <th>Worker</th>
            <th>Created</th>
            <th>Started</th>
            <th>Finished</th>
          </tr>
        </thead>
        <tbody>
          {stage.jobs.map((job) => (
            <tr key={job.id} data-testid="stage-job-row">
              <td>
                <InlineList>
                  <Mono>{job.id}</Mono>
                  {job.generator && <Badge>generator</Badge>}
                </InlineList>
                {job.resets > 0 && <Muted as="div">reset {job.resets === 1 ? 'once' : `${job.resets} times`}</Muted>}
              </td>
              <td>
                <StatusPill status={job.state} testId={`job-status-${job.id}`} />
              </td>
              <td>{job.worker ? <Mono>{job.worker}</Mono> : <Muted>none</Muted>}</td>
              <td>{job.created ? <RunTime value={job.created} /> : <Muted>none</Muted>}</td>
              <td>{job.started ? <RunTime value={job.started} /> : <Muted>none</Muted>}</td>
              <td>{job.finished ? <RunTime value={job.finished} /> : <Muted>none</Muted>}</td>
            </tr>
          ))}
        </tbody>
      </CardWideTable>
    </TableScroll>
  );
};

/** Props for {@link StageDetail}. */
interface StageDetailProps {
  /** The run */
  run: Reaction;
  /** The stage */
  stage: StageView;
  /** Whether the stage's step runs other stages in parallel */
  parallel: boolean;
  /** The tool the stage runs, when the run recorded it */
  tool?: ToolProvenance;
  /** Whether the stage is selected */
  selected: boolean;
  /** Whether the stage's logs show */
  shown: boolean;
  /** Show or hide the stage's logs */
  onToggle: () => void;
}

/** One stage: its status, tool, step, jobs, a link to its log page, and its logs. */
const StageDetail: FC<StageDetailProps> = ({ run, stage, parallel, tool, selected, shown, onToggle }) => {
  const running = !isRunFinished(run.status);
  return (
    <StageBlock id={stageBlockId(stage.name)} $selected={selected} data-testid="stage-detail" data-stage={stage.name}>
      <InlineRow>
        <StatusPill status={stage.state} testId={`stage-status-${stage.name}`} />
        <Mono>{stage.name}</Mono>
        {tool && (
          <Muted>
            runs{' '}
            <span title={tool.tool_instance}>
              <EntityLink to={`/tools/${tool.tool}`}>
                {tool.name} {tool.version}
              </EntityLink>
            </span>
          </Muted>
        )}
        <Muted>
          step {stage.step + 1}
          {parallel ? ' (parallel)' : ''}
        </Muted>
        <Button size={ButtonSize.XSmall} variant={ButtonVariant.Ghost} data-testid={`toggle-logs-${stage.name}`} onClick={onToggle}>
          {shown ? 'Hide logs' : 'Show logs'}
        </Button>
        <EntityLink to={`/reactions/logs/${run.group}/${run.id}/${encodeURIComponent(stage.name)}`}>Log page</EntityLink>
      </InlineRow>
      <StageBody>
        <StageJobs stage={stage} running={running} />
        {shown && <StageLogView group={run.group} reaction={run.id} stage={stage.name} running={running} />}
      </StageBody>
    </StageBlock>
  );
};

/** Props for {@link RunStagesSection}. */
interface RunStagesSectionProps {
  /** The run */
  run: Reaction;
  /** The stage names of each step */
  order: string[][];
  /** The run's status updates, or `null` before they are read */
  updates: StatusUpdate[] | null;
  /** How the stages show: a diagram over the stage list, or a run of one stage alone */
  view: RunStagesView.Diagram | RunStagesView.SingleStage;
}

/**
 * A run's stages: a diagram with a status node per stage (one column per step, the running step
 * highlighted), and under it each stage's jobs and logs. Selecting a node selects its stage, opens its
 * logs, and scrolls to it. A run of one stage shows the stage alone, with its logs open.
 */
export const RunStagesSection: FC<RunStagesSectionProps> = ({ run, order, updates, view }) => {
  const [ui, dispatch] = useReducer(stageUiReducer, INITIAL_STAGE_UI);
  const stages = useMemo(() => buildStageViews(run, order, updates), [run, order, updates]);
  const decorations = useMemo(() => stageDecorations(stages, ui.selected), [stages, ui.selected]);
  const tools = runProvenance(run)?.tools ?? [];
  const select = useCallback((stage: string) => {
    dispatch({ type: 'select', stage });
    // wait for the logs to open before scrolling, so the block lands where it ends up
    requestAnimationFrame(() => document.getElementById(stageBlockId(stage))?.scrollIntoView({ behavior: 'smooth', block: 'nearest' }));
  }, []);
  const single = view === RunStagesView.SingleStage;
  return (
    <Section data-testid={single ? 'run-stage-logs' : 'run-stages'}>
      <h2>{single ? 'Logs' : 'Stages'}</h2>
      {order.length === 0 && <Muted as="div">This run has no recorded stages.</Muted>}
      {!single && order.length > 0 && (
        <div data-testid="run-stage-diagram" style={{ marginBottom: '0.75rem' }}>
          <PipelineOrderFlow order={order} decorations={decorations} onStepSelect={select} />
        </div>
      )}
      <SeparatedList>
        {stages.flat().map((stage) => {
          const shown = logsShown(ui, stage.name, logsOpenByDefault(view, run, stage));
          return (
            <StageDetail
              key={stage.name}
              run={run}
              stage={stage}
              parallel={(order[stage.step]?.length ?? 1) > 1}
              tool={tools.find((tool) => tool.tool_instance === stage.toolInstance)}
              selected={ui.selected === stage.name}
              shown={shown}
              onToggle={() => dispatch({ type: 'toggle', stage: stage.name, shown })}
            />
          );
        })}
      </SeparatedList>
    </Section>
  );
};
