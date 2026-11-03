import { FC, useEffect, useState } from 'react';

// project imports
import { agentName, workflowInstanceLabel } from './display';
import { CardTable, CardWideTable } from './runs.styled';
import { versionsIntro } from './stages';
import { Badge, BadgeTone, EntityLink, InlineList, KeyValues, Mono, Muted, NoWrap, Section, TableScroll } from '@components/agentic/shared';
import { getAgenticEntity } from '@thorpi/agentic';
import { Reaction } from '@models/reactions';
import { ModelEndpoint } from '@models/modelconfigs';
import { ReactionModelBinding, RunProvenance, ToolProvenance } from '@models/runs';
import { runKind, runProvenance } from '@utilities/runs';

/** Props for the panels that read one run. */
interface RunPanelProps {
  /** The run */
  run: Reaction;
}

/** Props for {@link EndpointCell}. */
interface EndpointCellProps {
  /** The endpoint the model was called on, as the run recorded it */
  endpoint: ReactionModelBinding['endpoint'];
}

/** The name of a model endpoint: a group endpoint is read by id, and one from the runner's own settings has no entity to read. */
const EndpointCell: FC<EndpointCellProps> = ({ endpoint }) => {
  const [name, setName] = useState('');
  const group = endpoint['Group'];
  const user = endpoint['User'];
  useEffect(() => {
    if (!group) return;
    let cancelled = false;
    // the name is a nicety, so a failed read leaves the id shown
    void getAgenticEntity<ModelEndpoint>(group, () => undefined).then((found) => {
      if (!cancelled && found) setName(found.name);
    });
    return () => {
      cancelled = true;
    };
  }, [group]);
  if (user) return <Muted>the runner&apos;s own endpoint</Muted>;
  if (!group) return <Mono>{JSON.stringify(endpoint)}</Mono>;
  return name === '' ? <Mono>{group}</Mono> : <span title={group}>{name}</span>;
};

/** Props for {@link ToolInstanceCell}. */
interface ToolInstanceCellProps {
  /** The tool instance's id */
  id: string;
  /** The tools the run recorded, which name the instance */
  tools: ToolProvenance[];
}

/** A tool instance by name, linked to its tool's page; the id is shown only when the run recorded no name for it. */
const ToolInstanceCell: FC<ToolInstanceCellProps> = ({ id, tools }) => {
  const tool = tools.find((entry) => entry.tool_instance === id);
  if (!tool) return <Mono>{id}</Mono>;
  return (
    <span title={id}>
      <EntityLink to={`/tools/${tool.tool}`}>{tool.name}</EntityLink>
    </span>
  );
};

/** What the run ran: workflow instance, entry agent, harness, result key, and resolved models. */
export const RunSourcePanel: FC<RunPanelProps> = ({ run }) => {
  const source = run.source;
  const provenance = runProvenance(run);
  return (
    <Section data-testid="run-source">
      <h2>Source</h2>
      {source && 'WorkflowRun' in source && (
        <>
          <KeyValues>
            <dt>Workflow instance</dt>
            <dd>
              <span title={source.WorkflowRun.workflow_instance}>
                <EntityLink to={`/workflows/instances/${source.WorkflowRun.workflow_instance}`}>
                  {workflowInstanceLabel(source.WorkflowRun.result_key, source.WorkflowRun.workflow_instance)}
                </EntityLink>
              </span>
            </dd>
            <dt>Entry agent</dt>
            <dd>
              <EntityLink to={`/agents/${source.WorkflowRun.agent}`}>{agentName(provenance, source.WorkflowRun.agent)}</EntityLink>
            </dd>
            <dt>Harness</dt>
            <dd>
              <ToolInstanceCell id={source.WorkflowRun.harness} tools={source.WorkflowRun.provenance.tools} />
            </dd>
            <dt>Result key</dt>
            <dd data-testid="run-result-key">
              <Mono>{source.WorkflowRun.result_key}</Mono>
            </dd>
            <dt>Digest</dt>
            <dd>
              <Mono>{source.WorkflowRun.digest}</Mono>
            </dd>
          </KeyValues>
          <h3>Resolved models</h3>
          {source.WorkflowRun.models.length === 0 ? (
            <Muted>No models were resolved.</Muted>
          ) : (
            <CardTable data-testid="run-models">
              <thead>
                <tr>
                  <th>Agent</th>
                  <th>Model</th>
                  <th>Level</th>
                  <th>API</th>
                  <th>Endpoint</th>
                </tr>
              </thead>
              <tbody>
                {source.WorkflowRun.models.map((model) => (
                  <tr key={model.agent}>
                    <td>
                      <EntityLink to={`/agents/${model.agent}`}>{agentName(provenance, model.agent)}</EntityLink>
                    </td>
                    <td>{model.model_id}</td>
                    <td>{model.level}</td>
                    <td>{model.api}</td>
                    <td>
                      <EndpointCell endpoint={model.endpoint} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </CardTable>
          )}
        </>
      )}
      {source && 'Tool' in source && (
        <KeyValues>
          <dt>Tool instance</dt>
          <dd>
            <ToolInstanceCell id={source.Tool.tool_instance} tools={source.Tool.provenance.tools} />
          </dd>
        </KeyValues>
      )}
      {source && 'Pipeline' in source && (
        <KeyValues>
          <dt>Pipeline</dt>
          <dd>{source.Pipeline.name} </dd>
        </KeyValues>
      )}
      {!source && <Muted>This reaction did not record a source (created before runs were tracked).</Muted>}
      <p style={{ marginTop: '0.75rem' }}>
        <Muted>{runKind(run)} run. </Muted>
        <EntityLink to={`/reactions/${run.group}/${run.id}`}>Open the single reaction status page</EntityLink>
      </p>
    </Section>
  );
};

/** Props for {@link ProvenanceTables}. */
interface ProvenanceTablesProps {
  /** The provenance to show */
  provenance: RunProvenance;
  /** The harness tool instance of a workflow run, marked in the tool table */
  harness?: string;
}

/** The agent and tool tables of a provenance. */
export const ProvenanceTables: FC<ProvenanceTablesProps> = ({ provenance, harness }) => (
  <>
    {provenance.agents.length > 0 && (
      <>
        <h3>Agents</h3>
        {/* the hashes need room, so a narrow window scrolls the table instead of splitting each hash over many lines */}
        <TableScroll>
          <CardWideTable data-testid="provenance-agents">
            <thead>
              <tr>
                <th>Agent</th>
                <th>Config hash</th>
                <th>Toolset digest</th>
              </tr>
            </thead>
            <tbody>
              {provenance.agents.map((agent) => (
                <tr key={agent.agent}>
                  {/* long hashes wrap anywhere, so the short name cells stay on one line instead of splitting at hyphens */}
                  <td>
                    <NoWrap>{agent.name}</NoWrap>
                  </td>
                  <td>
                    <Mono>{agent.config_hash}</Mono>
                  </td>
                  <td>
                    <Mono>{agent.toolset_digest}</Mono>
                  </td>
                </tr>
              ))}
            </tbody>
          </CardWideTable>
        </TableScroll>
      </>
    )}
    <h3>Tools</h3>
    {/* a hash and an image reference are read whole, so a narrow window scrolls the table instead of splitting them */}
    <TableScroll>
      <CardWideTable data-testid="provenance-tools">
        <thead>
          <tr>
            <th>Tool instance</th>
            <th>Kind</th>
            <th>Version</th>
            <th>Spec hash</th>
            <th>Image</th>
          </tr>
        </thead>
        <tbody>
          {provenance.tools.map((tool) => (
            <tr key={tool.tool_instance}>
              <td>
                <InlineList>
                  <NoWrap>
                    <EntityLink to={`/tools/${tool.tool}`}>{tool.name}</EntityLink>
                  </NoWrap>
                  {tool.tool_instance === harness && <Badge $tone={BadgeTone.Info}>harness</Badge>}
                </InlineList>
              </td>
              <td>
                <NoWrap>{tool.kind}</NoWrap>
              </td>
              <td>
                <NoWrap>{tool.version}</NoWrap>
              </td>
              <td>
                <Mono>{tool.spec_hash}</Mono>
              </td>
              <td>
                <Mono>{tool.image ?? ''}</Mono>
              </td>
            </tr>
          ))}
        </tbody>
      </CardWideTable>
    </TableScroll>
  </>
);

/** The Versions used section: the exact agent, harness, and tool versions the run was created with. */
export const RunProvenancePanel: FC<RunPanelProps> = ({ run }) => {
  const provenance = runProvenance(run);
  const harness = run.source && 'WorkflowRun' in run.source ? run.source.WorkflowRun.harness : undefined;
  // the intro names only the kinds of version the tables list, and is null when nothing was recorded
  const intro = versionsIntro(provenance, harness);
  return (
    <Section data-testid="run-provenance">
      <h2>Versions used</h2>
      {intro === null ? (
        <Muted>No versions were recorded for this run.</Muted>
      ) : (
        <>
          <Muted as="div" data-testid="run-versions-intro">
            {intro}
          </Muted>
          <ProvenanceTables provenance={provenance!} harness={harness} />
        </>
      )}
    </Section>
  );
};
