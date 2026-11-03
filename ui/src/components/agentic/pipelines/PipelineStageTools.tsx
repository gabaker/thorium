import { FC } from 'react';
import styled from 'styled-components';

// project imports
import { isStageConverted, StageBacking, stageBacking } from './stages';
import { Badge, BadgeTone, EntityLink, Mono, Muted, Table } from '@components/agentic/shared';
import { useAgenticEntities } from '@utilities/queries/agentic';
import { Pipeline } from '@models/pipelines';
import { Tool, ToolInstance } from '@models/tools';

/**
 * The table embedded in a key/value panel: the first column starts flush with the panel's other values
 * and the header row with the panel's label. The columns have fixed widths so stacked tables line up.
 */
const StageToolsTable = styled(Table)`
  table-layout: fixed;

  th:first-child,
  td:first-child {
    padding-left: 0;
    width: 30%;
  }

  th:nth-child(2),
  td:nth-child(2),
  th:nth-child(3),
  td:nth-child(3) {
    width: 35%;
  }

  thead th {
    padding-top: 0;
  }
`;

/** Props for {@link PipelineStageTools}. */
interface PipelineStageToolsProps {
  /** The pipeline whose stage tools are shown */
  pipeline: Pipeline;
}

/** Show the tool backing each stage of a pipeline, and mark stages that still name an image no tool backs. */
export const PipelineStageTools: FC<PipelineStageToolsProps> = ({ pipeline }) => {
  const stages = pipeline.order.flat();
  const stageTools = pipeline.stage_tools ?? {};
  // each id is read once through the shared entity cache, all at once, so a new pipeline object reads nothing again
  const entities = useAgenticEntities<Tool | ToolInstance>(Object.values(stageTools));
  const resolved: Record<string, StageBacking> = {};
  for (const [stage, id] of Object.entries(stageTools)) {
    const entity = entities[id];
    const backing = entity ? stageBacking(entity) : null;
    if (backing) resolved[stage] = backing;
  }
  return (
    <StageToolsTable data-testid="stage-tools">
      <thead>
        <tr>
          <th>Stage</th>
          <th>Backed by</th>
          <th>Version</th>
        </tr>
      </thead>
      <tbody>
        {stages.map((stage) => {
          const entry = resolved[stage];
          return (
            <tr key={stage} data-testid={`stage-tool-row-${stage}`}>
              <td>{stage}</td>
              <td>
                {isStageConverted(pipeline, stage) ? (
                  entry ? (
                    <EntityLink to={`/tools/${entry.toolId}`}>{entry.name}</EntityLink>
                  ) : (
                    <Muted>
                      loading <Mono>{pipeline.stage_tools?.[stage]}</Mono>
                    </Muted>
                  )
                ) : (
                  <Badge $tone={BadgeTone.Legacy}>needs a tool: no tool backs this stage</Badge>
                )}
              </td>
              <td>
                {entry &&
                  (entry.pinned ? (
                    <>
                      pinned to <Mono>{entry.pinned}</Mono>
                    </>
                  ) : (
                    'follows latest'
                  ))}
              </td>
            </tr>
          );
        })}
      </tbody>
    </StageToolsTable>
  );
};
