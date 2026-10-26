import { FC, useCallback, useEffect, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';

// project imports
import {
  Actions,
  BannerSlot,
  CodeSpans,
  EntityLink,
  InlineList,
  KeyValues,
  Muted,
  NoWrap,
  PageHead,
  PageTitle,
  Pre,
  Section,
} from '@components/agentic/shared';
import { ConfirmButton } from '@components/agentic/ConfirmButton';
import { UsedBy } from '@components/agentic/UsedBy';
import { useAgenticList, useEntityAccess } from '@components/agentic/hooks';
import { EntityNotFound } from '@components/agentic/EntityNotFound';
import { linkFor, modelsText, versionNote } from '@components/agentic/agents/names';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import Page from '@components/pages/Page';
import { getAgenticEntity } from '@thorpi/agentic';
import { deleteEntity } from '@thorpi/entities';
import { workflowsUsingAgent } from '@utilities/agentic/usedBy';
import { Agent } from '@models/agents';
import { AgenticKinds, AnyAgenticEntity } from '@models/agentic';
import { Workflow } from '@models/workflows';

/**
 * Show an agent's configuration and the entities it is linked to.
 *
 * @param props - The props for the page; its state belongs to one id.
 * @param props.id - The id of the agent to show.
 */
const AgentPage: FC<{ id: string }> = ({ id }) => {
  const navigate = useNavigate();
  const [agent, setAgent] = useState<Agent | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const { deleteReason } = useEntityAccess(agent?.groups ?? [], agent?.submitter ?? '');
  // the workflows that include this agent are found by looking through every workflow the user can see
  const workflows = useAgenticList<Workflow>(AgenticKinds.WorkflowCatalog);
  // the kind an opened id turned out to be when it is not an agent
  const [other, setOther] = useState<{ id: string; kind: string } | null>(null);
  // whether the agent read failed for a reason other than the agent being missing or hidden (such as a server error); a retry may fix it
  const [readFailed, setReadFailed] = useState(false);
  const load = useCallback(async () => {
    setLoading(true);
    setOther(null);
    setReadFailed(false);
    const found = await getAgenticEntity<Agent>(id, setError, (status) => setReadFailed(![401, 403, 404].includes(status)));
    // an id of another kind (such as a tool) has no agent metadata to show
    if (found && found.kind !== AgenticKinds.Agent) setOther({ id, kind: found.kind });
    setAgent(found && found.kind === AgenticKinds.Agent ? found : null);
    setLoading(false);
  }, [id]);
  useEffect(() => {
    void load();
  }, [load]);
  if (!agent) {
    return (
      <EntityNotFound
        what="Agent"
        loading={loading}
        listPath="/agents"
        listName="agents"
        error={error}
        failed={readFailed}
        onRetry={() => {
          // show the spinner while the agent is read again
          setError('');
          setLoading(true);
          void load();
        }}
        other={other ?? undefined}
      />
    );
  }
  const meta = agent.metadata.Agent;
  const config = meta.config;
  // every edge is populated as a full entity, so show names with links by kind
  const linked = (items: AnyAgenticEntity[] | undefined, withVersion = false) =>
    (items ?? []).length === 0 ? (
      <Muted>none</Muted>
    ) : (
      <InlineList>
        {(items ?? []).map((item) => (
          <NoWrap key={item.id}>
            <EntityLink to={linkFor(item)}>{item.name}</EntityLink>
            {withVersion && versionNote(item) && <Muted> ({versionNote(item)})</Muted>}
          </NoWrap>
        ))}
      </InlineList>
    );
  return (
    <Page title={`${agent.name} · Thorium`}>
      <PageHead>
        <PageTitle data-testid="agent-title">{agent.name}</PageTitle>
        <Actions>
          <ConfirmButton
            testId="delete-agent"
            disabled={deleteReason !== ''}
            title={deleteReason}
            onConfirm={() =>
              void deleteEntity(id, setError).then((done) => {
                if (done) void navigate('/agents');
              })
            }
          >
            Delete
          </ConfirmButton>
        </Actions>
      </PageHead>
      <BannerSlot>
        {error && (
          <AlertBanner severity={Severity.Error}>
            <CodeSpans text={error} />
          </AlertBanner>
        )}
      </BannerSlot>
      <Section>
        <h2>Agent</h2>
        <KeyValues data-testid="agent-info">
          <dt>Groups</dt>
          <dd>{agent.groups.join(', ')}</dd>
          <dt>Description</dt>
          <dd>{agent.description ?? <Muted>none</Muted>}</dd>
          <dt>Models</dt>
          <dd>{modelsText(config.models)}</dd>
          <dt>Max turns</dt>
          <dd>{config.max_turns ?? <Muted>harness default</Muted>}</dd>
          <dt>Max tools</dt>
          <dd>{config.max_tools ?? <Muted>default</Muted>}</dd>
          <dt>Function name</dt>
          <dd>{config.function_name ?? <Muted>none (not a sub-agent)</Muted>}</dd>
          <dt>Model description</dt>
          <dd>{config.model_description ?? <Muted>none (not a sub-agent)</Muted>}</dd>
          <dt>Harness</dt>
          <dd data-testid="agent-harness">{linked(meta.harness ? [meta.harness] : [], true)}</dd>
          <dt>Tools (latest)</dt>
          <dd>{linked(meta.latest_tools, true)}</dd>
          <dt>Tools (pinned)</dt>
          <dd>{linked(meta.pinned_tools, true)}</dd>
          <dt>Toolsets</dt>
          <dd>{linked(meta.tool_sets, true)}</dd>
          <dt>Sub-agents</dt>
          <dd>{linked(meta.sub_agents)}</dd>
          <dt>Used by workflows</dt>
          <dd data-testid="agent-used-by">
            <UsedBy
              users={workflowsUsingAgent(workflows.items, agent.id)}
              linkTo={(workflowId) => `/workflows/${workflowId}`}
              loading={workflows.loading}
              error={workflows.error}
            />
          </dd>
        </KeyValues>
      </Section>
      <Section>
        <h2>System prompt</h2>
        {config.system_prompt ? (
          <Pre data-testid="agent-system-prompt">{config.system_prompt}</Pre>
        ) : (
          <Muted data-testid="agent-system-prompt">none</Muted>
        )}
        <h2>Task prompt</h2>
        {config.prompt ? <Pre data-testid="agent-task-prompt">{config.prompt}</Pre> : <Muted data-testid="agent-task-prompt">none</Muted>}
      </Section>
    </Page>
  );
};

/**
 * View an agent by the id in the URL.
 *
 * The page is keyed by the id so moving between ids (such as with back and forward) starts from a clean page:
 * the previous id's content, forms, in-flight reads, and any error they reported are dropped.
 */
const AgentDetails: FC = () => {
  const { id = '' } = useParams();
  // keying the page by id remounts it, so every id starts loading from a clean state
  return <AgentPage key={id} id={id} />;
};

export default AgentDetails;
