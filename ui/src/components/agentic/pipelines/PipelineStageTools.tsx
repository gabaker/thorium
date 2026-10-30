import { FC, useEffect, useState } from 'react';
import styled from 'styled-components';

// project imports
import { isStageConverted } from './stages';
import { Badge, BadgeTone, EntityLink, Mono, Muted, Table } from '@components/agentic/shared';
import { getAgenticEntity } from '@thorpi/agentic';
import { toolVersionLabel } from '@utilities/agentic/display';
import { AgenticKinds } from '@models/agentic';
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

/** What a stage's tool id resolved to. */
interface Resolved {
  /** The tool's name */
  name: string;
  /** The tool's id, for the link */
  toolId: string;
  /** The pinned version, or `null` when the stage follows latest */
  pinned: string | null;
}

/** Props for {@link PipelineStageTools}. */
interface PipelineStageToolsProps {
  /** The pipeline whose stage tools are shown */
  pipeline: Pipeline;
}

/** Show the tool backing each stage of a pipeline, and mark stages that still name an image no tool backs. */
export const PipelineStageTools: FC<PipelineStageToolsProps> = ({ pipeline }) => {
  const [resolved, setResolved] = useState<Record<string, Resolved>>({});
  const stages = pipeline.order.flat();
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      const found: Record<string, Resolved> = {};
      for (const [stage, id] of Object.entries(pipeline.stage_tools ?? {})) {
        const entity = await getAgenticEntity<Tool | ToolInstance>(id, () => undefined);
        if (!entity) continue;
        if (entity.kind === AgenticKinds.Tool) {
          found[stage] = { name: entity.name, toolId: entity.id, pinned: null };
        } else {
          const instance = entity;
          const tool = instance.metadata.ToolInstance.tool;
          found[stage] = {
            name: entity.name.split('@')[0],
            toolId: tool,
            pinned: toolVersionLabel(instance.metadata.ToolInstance.version),
          };
        }
      }
      if (!cancelled) setResolved(found);
    })();
    return () => {
      cancelled = true;
    };
  }, [pipeline]);
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
