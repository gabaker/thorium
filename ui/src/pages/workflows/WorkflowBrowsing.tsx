import { FC } from 'react';
import { useNavigate } from 'react-router-dom';

// project imports
import {
  Actions,
  BannerSlot,
  Badge,
  EntityLink,
  LinkRow,
  Muted,
  PageHead,
  PageTitle,
  Section,
  ShortId,
  statusTone,
  Table,
  TableScroll,
} from '@components/agentic/shared';
import ListErrorBanner from '@components/shared/alerts/ListErrorBanner';
import { InstanceStatusBadge } from '@components/agentic/InstanceStatusBadge';
import { useAgenticList } from '@components/agentic/hooks';
import { EntityName } from '@components/agentic/models/ModelRows';
import { nameIndex } from '@components/agentic/agents/names';
import { entryText } from '@components/agentic/workflows/format';
import { Button } from '@components/shared/buttons';
import { ButtonVariant } from '@components/shared/buttons/types';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import Page from '@components/pages/Page';
import { duplicateNames } from '@utilities/agentic/groups';
import { Agent } from '@models/agents';
import { AgenticKinds } from '@models/agentic';
import { Workflow, WorkflowInstance } from '@models/workflows';

/** List the workflows the user can see with their entry agent and latest instance. */
const WorkflowBrowsing: FC = () => {
  const navigate = useNavigate();
  const workflows = useAgenticList<Workflow>(AgenticKinds.WorkflowCatalog);
  const instances = useAgenticList<WorkflowInstance>(AgenticKinds.WorkflowInstance);
  const agents = useAgenticList<Agent>(AgenticKinds.Agent);
  const names = nameIndex(agents.items);
  const byId = new Map(instances.items.map((item) => [item.id, item]));
  // workflows that share a name show a short id so their rows can be told apart
  const repeated = duplicateNames(workflows.items);
  return (
    <Page title="Workflows · Thorium">
      <PageHead>
        <PageTitle>Workflows</PageTitle>
        <Actions>
          <Button variant={ButtonVariant.Ok} data-testid="create-workflow" onClick={() => void navigate('/workflows/create')}>
            + New workflow
          </Button>
        </Actions>
      </PageHead>
      <BannerSlot>
        {/* the page's own list reports first, then the lists that fill in its columns */}
        <ListErrorBanner errors={[workflows.error, instances.error, agents.error]} className="" />
      </BannerSlot>
      <LoadingSpinner loading={workflows.loading} />
      {!workflows.loading && workflows.error === '' && workflows.items.length === 0 && (
        <Muted data-testid="workflow-empty">No workflows found.</Muted>
      )}
      {/* nothing else shows in the list's place until it has loaded, and an empty list says so instead of showing a bare table */}
      {!workflows.loading && workflows.items.length > 0 && (
        <Section>
          {/* names wrap between their hyphen-separated chunks, so the table fits a narrow card instead of scrolling past a column */}
          <TableScroll>
            <Table data-testid="workflow-table">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Groups</th>
                  <th>Agents</th>
                  <th>Entry agent</th>
                  <th>Latest ready instance</th>
                  <th>Revision</th>
                </tr>
              </thead>
              <tbody>
                {workflows.items.map((workflow) => {
                  const meta = workflow.metadata.WorkflowCatalog;
                  const latest = meta.latest_instance ? byId.get(meta.latest_instance) : undefined;
                  return (
                    <LinkRow key={workflow.id} to={`/workflows/${workflow.id}`} testId={`workflow-row-${workflow.name}`}>
                      <td>
                        <EntityLink to={`/workflows/${workflow.id}`}>
                          <EntityName name={workflow.name} />
                        </EntityLink>
                        <ShortId id={workflow.id} show={repeated.has(workflow.name)} />
                      </td>
                      <td>{workflow.groups.join(', ')}</td>
                      <td>{meta.agents.length}</td>
                      <td>{meta.entry_agent ? <EntityName name={entryText(meta.entry_agent, names)} /> : <Muted>none</Muted>}</td>
                      <td>
                        {latest ? (
                          <>
                            <EntityName name={latest.name} />
                            {/* the status sits under the name so the name's column doesn't also have to hold the badge */}
                            <div>
                              <InstanceStatusBadge status={latest.metadata.WorkflowInstance.status} />
                            </div>
                          </>
                        ) : instances.error ? (
                          // a failed or cut off instance list cannot tell that no instance is Ready
                          <Muted>-</Muted>
                        ) : (
                          <Badge tone={statusTone('no ready instance')}>No ready instance</Badge>
                        )}
                      </td>
                      <td>{meta.highest_revision}</td>
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

export default WorkflowBrowsing;
