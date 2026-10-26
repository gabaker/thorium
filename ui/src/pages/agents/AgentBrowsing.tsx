import { FC, Fragment } from 'react';
import { useNavigate } from 'react-router-dom';

// project imports
import {
  Actions,
  BannerSlot,
  CodeSpans,
  Badge,
  EntityLink,
  FlexName,
  InlineList,
  LinkRow,
  NoWrap,
  Muted,
  PageHead,
  PageTitle,
  Section,
  ShortId,
  Table,
  TableScroll,
} from '@components/agentic/shared';
import { useAgenticList } from '@components/agentic/hooks';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Button } from '@components/shared/buttons';
import { ButtonVariant } from '@components/shared/buttons/types';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import Page from '@components/pages/Page';
import { duplicateNames } from '@utilities/agentic/groups';
import { workflowsUsingAgent } from '@utilities/agentic/usedBy';
import { Agent } from '@models/agents';
import { countLabel, isShortName } from '@utilities/agentic/display';
import { AgenticKinds } from '@models/agentic';
import { Workflow } from '@models/workflows';

/** Props for {@link ModelsCell}. */
interface ModelsCellProps {
  /** The agent's model matchers in priority order. */
  models: { model_id: string }[] | null | undefined;
}

/** The models of an agent in priority order, each id kept whole when short, so a narrow column breaks only between models. */
const ModelsCell: FC<ModelsCellProps> = ({ models }) => {
  // an agent with no models uses the workflow's default
  if (!models || models.length === 0) return <>workflow default</>;
  return (
    <>
      {models.map((model, index) => (
        <Fragment key={`${index}-${model.model_id}`}>
          {/* the plain space between models is the only place the column may break between them */}
          {index > 0 && ' '}
          {/* the no-break space keeps the separator beside its id, so a narrow column reads "a >" then "b" */}
          <span>
            <FlexName $short={isShortName(model.model_id)}>{model.model_id}</FlexName>
            {index < models.length - 1 && '\u00a0>'}
          </span>
        </Fragment>
      ))}
    </>
  );
};

/** The longest harness name, in characters, that the Harness column keeps on one line, so a longer one yields width to the Links badges. */
const HARNESS_WHOLE_LIMIT = 16;

/** Props for {@link HarnessCell}. */
interface HarnessCellProps {
  /** The name of the agent's harness, which may end in a pinned version such as `@1.0.0`. */
  name: string;
}

/** The harness name, whole when short and wrapping when long, with a pinned version kept whole so a narrow column breaks before it. */
const HarnessCell: FC<HarnessCellProps> = ({ name }) => {
  // split the name just before a version marker so the tool name and the version stay separate runs
  const at = name.indexOf('@', 1);
  if (at < 0) return <FlexName $short={name.length <= HARNESS_WHOLE_LIMIT}>{name}</FlexName>;
  return (
    <>
      <FlexName $short={at <= HARNESS_WHOLE_LIMIT}>{name.slice(0, at)}</FlexName>
      {/* the only place the column may break */}
      <wbr />
      <NoWrap>{name.slice(at)}</NoWrap>
    </>
  );
};

/** List the AI agents the user can see. */
const AgentBrowsing: FC = () => {
  const navigate = useNavigate();
  const agents = useAgenticList<Agent>(AgenticKinds.Agent);
  // the workflows that include each agent are found by looking through every workflow the user can see
  const workflows = useAgenticList<Workflow>(AgenticKinds.WorkflowCatalog);
  // agents that share a name show a short id so their rows can be told apart
  const repeated = duplicateNames(agents.items);
  return (
    <Page title="Agents · Thorium">
      <PageHead>
        <PageTitle>Agents</PageTitle>
        <Actions>
          <Button variant={ButtonVariant.Ok} data-testid="create-agent" onClick={() => void navigate('/agents/create')}>
            + New agent
          </Button>
        </Actions>
      </PageHead>
      <BannerSlot>
        {agents.error && (
          <AlertBanner severity={Severity.Error}>
            <CodeSpans text={agents.error} />
          </AlertBanner>
        )}
      </BannerSlot>
      <LoadingSpinner loading={agents.loading} />
      {!agents.loading && agents.error === '' && agents.items.length === 0 && <Muted data-testid="agent-empty">No agents found.</Muted>}
      {/* nothing else shows in the list's place until it has loaded, and an empty list says so instead of showing a bare table */}
      {!agents.loading && agents.items.length > 0 && (
        <Section>
          {/* scrolls sideways instead of poking out of the card when the columns need more room than the window gives */}
          <TableScroll>
            <Table data-testid="agent-table">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Groups</th>
                  <th>Harness</th>
                  <th>Models</th>
                  {/* wide enough for two count badges on a line, so a row is at most two badge lines tall */}
                  <th style={{ minWidth: '10.5rem' }}>Links</th>
                </tr>
              </thead>
              <tbody>
                {agents.items.map((agent) => {
                  const meta = agent.metadata.Agent;
                  return (
                    <LinkRow key={agent.id} to={`/agents/${agent.id}`} testId={`agent-row-${agent.name}`}>
                      <td>
                        <EntityLink to={`/agents/${agent.id}`}>
                          <FlexName $short={isShortName(agent.name)}>{agent.name}</FlexName>
                        </EntityLink>
                        <ShortId id={agent.id} show={repeated.has(agent.name)} />
                        {/* the workflow count sits under the name rather than in a column of its own, so it adds no width to the table */}
                        <div data-testid={`agent-used-by-${agent.name}`}>
                          <Muted>
                            used by{' '}
                            {workflows.loading || workflows.error
                              ? '-'
                              : countLabel(workflowsUsingAgent(workflows.items, agent.id).length, 'workflow')}
                          </Muted>
                        </div>
                      </td>
                      <td>{agent.groups.join(', ')}</td>
                      <td>{meta.harness ? <HarnessCell name={meta.harness.name} /> : <Muted>none</Muted>}</td>
                      <td>
                        <ModelsCell models={meta.config.models} />
                      </td>
                      <td>
                        <InlineList>
                          <Badge>{countLabel((meta.latest_tools?.length ?? 0) + (meta.pinned_tools?.length ?? 0), 'tool')}</Badge>
                          <Badge>{countLabel(meta.tool_sets?.length ?? 0, 'toolset')}</Badge>
                          <Badge>{countLabel(meta.sub_agents?.length ?? 0, 'sub-agent')}</Badge>
                        </InlineList>
                      </td>
                    </LinkRow>
                  );
                })}
              </tbody>
            </Table>
          </TableScroll>
        </Section>
      )}
    </Page>
  );
};

export default AgentBrowsing;
