import { FC, useCallback, useEffect, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';

// project imports
import {
  Actions,
  Badge,
  BadgeTone,
  BannerSlot,
  CheckRow,
  CodeSpans,
  EntityLink,
  Field,
  KeyValues,
  Muted,
  NoWrap,
  PageHead,
  PageTitle,
  Section,
  SpacedField,
  Table,
  TableScroll,
  TextArea,
} from '@components/agentic/shared';
import { ConfirmButton } from '@components/agentic/ConfirmButton';
import { EntityNotFound } from '@components/agentic/EntityNotFound';
import { InstanceStatusBadge } from '@components/agentic/InstanceStatusBadge';
import { useAgenticList, useEntityAccess, useRequestInstance } from '@components/agentic/hooks';
import { Notice } from '@components/agentic/instances';
import { EntityChecks, EntitySelect } from '@components/agentic/pickers';
import { nameIndex } from '@components/agentic/agents/names';
import {
  defaultModelsText,
  entryText,
  MODEL_AUTH_LEVELS,
  matchersForIds,
  mergeAuthLevels,
  newestFirst,
  triggerText,
} from '@components/agentic/workflows/format';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Button, ButtonSize } from '@components/shared/buttons';
import { ButtonVariant } from '@components/shared/buttons/types';
import TextInput from '@components/shared/inputs/TextInput';
import Page from '@components/pages/Page';
import { getAgenticEntity, listAllAgentic } from '@thorpi/agentic';
import { deleteEntity, updateEntity } from '@thorpi/entities';
import { buildRootUpdateForm, buildWorkflowUpdateForm, parsePositiveInt, positiveIntError } from '@utilities/agentic/forms';
import { splitEntries } from '@utilities/runs';
import { invalidateAgentic } from '@utilities/queries/agentic';
import { useChangeAndInvalidate } from '@utilities/queries/changes';
import { Agent } from '@models/agents';
import { AgenticKinds } from '@models/agentic';
import { Workflow, WorkflowInstance } from '@models/workflows';

/** An edit field of the workflow page that can be reset to its stored value independently of the others. */
type EditField = 'description' | 'models' | 'alive' | 'auth' | 'entry';

/** Every edit field, reset together when the page loads or is reloaded. */
const ALL_FIELDS: EditField[] = ['description', 'models', 'alive', 'auth', 'entry'];

/**
 * Show a workflow, its instances, and the edits and actions it supports.
 *
 * @param props - The props for the page; its state belongs to one id.
 * @param props.id - The id of the workflow to show.
 */
const WorkflowPage: FC<{ id: string }> = ({ id }) => {
  const navigate = useNavigate();
  // a change marks the cached lists and entities stale, so other pages show it at once
  const change = useChangeAndInvalidate(invalidateAgentic);
  const [workflow, setWorkflow] = useState<Workflow | null>(null);
  const [instances, setInstances] = useState<WorkflowInstance[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  // the kind an opened id turned out to be when it is not a workflow
  const [other, setOther] = useState<{ id: string; kind: string } | null>(null);
  // whether the workflow read failed for a reason other than being missing or hidden (such as a server error); a retry may fix it
  const [readFailed, setReadFailed] = useState(false);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [addIds, setAddIds] = useState<string[]>([]);
  const [removeIds, setRemoveIds] = useState<string[]>([]);
  const [entry, setEntry] = useState('');
  const [description, setDescription] = useState('');
  const [models, setModels] = useState('');
  const [alive, setAlive] = useState('');
  const [authLevels, setAuthLevels] = useState<string[]>([]);
  const agents = useAgenticList<Agent>(AgenticKinds.Agent);
  const { deleteReason } = useEntityAccess(workflow?.groups ?? [], workflow?.submitter ?? '');
  /** Read this workflow's instances, newest first. */
  const readInstances = useCallback(async () => {
    // instances are separate entities that name their workflow, so list and filter them
    const all = await listAllAgentic<WorkflowInstance>(AgenticKinds.WorkflowInstance, [], setError);
    return newestFirst(all.filter((item) => item.metadata.WorkflowInstance.workflow === id));
  }, [id]);
  /** Read the workflow and its instances, and reset the given edit fields (all by default) to what is stored; other unsaved edits stay. */
  const load = useCallback(
    async (reset: EditField[] = ALL_FIELDS) => {
      setLoading(true);
      setOther(null);
      setReadFailed(false);
      const read = await getAgenticEntity<Workflow>(id, setError, (status) => setReadFailed(![401, 403, 404].includes(status)));
      // an id of another kind (such as a tool) has no workflow metadata to show
      if (read && read.kind !== AgenticKinds.WorkflowCatalog) setOther({ id, kind: read.kind });
      const found = read && read.kind === AgenticKinds.WorkflowCatalog ? read : null;
      setWorkflow(found);
      const stored = found?.metadata.WorkflowCatalog;
      if (reset.includes('description')) setDescription(found?.description ?? '');
      if (reset.includes('models')) setModels((stored?.default_models ?? []).map((model) => model.model_id).join('\n'));
      if (reset.includes('alive')) setAlive(stored?.limits?.max_alive_sub_agents?.toString() ?? '');
      if (reset.includes('auth')) setAuthLevels(stored?.model_auth?.levels ?? []);
      if (reset.includes('entry')) setEntry(stored?.entry_agent ?? '');
      // an entry choice that is no longer a member (its agent was just removed) falls back to what is stored
      else setEntry((chosen) => (stored?.agents.some((agent) => agent.id === chosen) ? chosen : (stored?.entry_agent ?? '')));
      // an id that is not this kind has no instances to read
      if (found) setInstances(await readInstances());
      setLoading(false);
    },
    [id, readInstances],
  );
  useEffect(() => {
    void load();
  }, [load]);
  const request = useRequestInstance({ root: 'workflows', id, instances, read: readInstances, setInstances, setError, setNotice });
  if (!workflow) {
    return (
      <EntityNotFound
        what="Workflow"
        loading={loading}
        listPath="/workflows"
        listName="workflows"
        error={error}
        failed={readFailed}
        onRetry={() => {
          // show the spinner while the workflow is read again
          setError('');
          setLoading(true);
          void load();
        }}
        other={other ?? undefined}
      />
    );
  }
  const meta = workflow.metadata.WorkflowCatalog;
  const memberIds = meta.agents.map((agent) => agent.id);
  const names = nameIndex(agents.items, meta.agents);
  const aliveError = positiveIntError(alive);
  /**
   * Send an update of the workflow, then show the notice and read it again; resolves to whether the update was applied.
   * Only the edit fields in `reset` return to their stored values, so unsaved edits in other fields stay.
   */
  const apply = async (form: FormData, message: string, reset: EditField[] = []): Promise<boolean> => {
    setError('');
    setNotice(null);
    // a refused update has already set the error banner
    if (!(await change(() => updateEntity(id, form, setError)))) return false;
    setNotice({ text: message, severity: Severity.Success });
    await load(reset);
    return true;
  };
  return (
    <Page title={`${workflow.name} · Thorium`}>
      <PageHead>
        <PageTitle data-testid="workflow-title">{workflow.name}</PageTitle>
        <Actions>
          <Button variant={ButtonVariant.Ok} data-testid="run-workflow" onClick={() => void navigate(`/launch?workflow=${id}`)}>
            Run this workflow
          </Button>
          <Button variant={ButtonVariant.Primary} data-testid="request-instance" onClick={() => void request()}>
            Request instance
          </Button>
          <Button variant={ButtonVariant.Secondary} data-testid="reload" onClick={() => void load()}>
            Reload
          </Button>
          <ConfirmButton
            testId="delete-workflow"
            size={ButtonSize.Medium}
            disabled={deleteReason !== ''}
            title={deleteReason}
            onConfirm={() =>
              void change(() => deleteEntity(id, setError)).then((done) => {
                if (done) void navigate('/workflows');
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
        {notice && <AlertBanner severity={notice.severity}>{notice.text}</AlertBanner>}
      </BannerSlot>
      <Section>
        <h2>Settings</h2>
        <KeyValues data-testid="workflow-info">
          <dt>Groups</dt>
          <dd>{workflow.groups.join(', ')}</dd>
          <dt>Description</dt>
          <dd>{workflow.description ?? <Muted>none</Muted>}</dd>
          <dt>Entry agent</dt>
          <dd>{meta.entry_agent ? entryText(meta.entry_agent, names) : <Muted>none</Muted>}</dd>
          <dt>Default models</dt>
          <dd>{meta.default_models && meta.default_models.length > 0 ? defaultModelsText(meta.default_models) : <Muted>none</Muted>}</dd>
          <dt>Max alive sub-agents</dt>
          <dd>{meta.limits?.max_alive_sub_agents ?? <Muted>default</Muted>}</dd>
          <dt>Model auth levels</dt>
          <dd>{(meta.model_auth?.levels ?? []).join(', ') || <Muted>all</Muted>}</dd>
          <dt>Auto instance</dt>
          <dd>{meta.auto_instance ? 'on' : 'off'}</dd>
        </KeyValues>
        <h3>Edit settings</h3>
        <Field>
          <span className="label">Description</span>
          <TextInput data-testid="edit-description" value={description} onChange={(event) => setDescription(event.target.value)} />
        </Field>
        <Actions>
          <Button
            variant={ButtonVariant.Secondary}
            data-testid="save-description"
            onClick={() =>
              void apply(buildRootUpdateForm({ description, clearDescription: description.trim() === '' }), 'Description saved', [
                'description',
              ])
            }
          >
            Save description
          </Button>
          <Button
            variant={ButtonVariant.Secondary}
            data-testid="toggle-auto-instance"
            onClick={() => void apply(buildWorkflowUpdateForm({ autoInstance: !meta.auto_instance }), 'Auto instance changed')}
          >
            {meta.auto_instance ? 'Turn auto instance off' : 'Turn auto instance on'}
          </Button>
        </Actions>
        <SpacedField>
          <span className="label">Default models</span>
          <span className="hint">Model ids for agents without their own, one per line; leave empty for none.</span>
          <TextArea data-testid="edit-models" value={models} onChange={(event) => setModels(event.target.value)} />
        </SpacedField>
        <Actions>
          <Button
            variant={ButtonVariant.Secondary}
            data-testid="save-models"
            onClick={() =>
              void apply(
                buildWorkflowUpdateForm({ defaultModels: matchersForIds(meta.default_models, splitEntries(models)) }),
                'Default models saved',
                ['models'],
              )
            }
          >
            Save default models
          </Button>
        </Actions>
        <SpacedField>
          <span className="label">Max alive sub-agents</span>
          <span className="hint">Leave blank for the harness default.</span>
          <TextInput
            data-testid="edit-alive"
            $invalid={aliveError !== ''}
            value={alive}
            onChange={(event) => setAlive(event.target.value)}
          />
          {aliveError && (
            <span className="error" data-testid="edit-alive-error">
              {aliveError}
            </span>
          )}
        </SpacedField>
        <Actions>
          <Button
            variant={ButtonVariant.Secondary}
            data-testid="save-alive"
            disabled={aliveError !== ''}
            onClick={() =>
              void apply(buildWorkflowUpdateForm({ maxAliveSubAgents: parsePositiveInt(alive) ?? null }), 'Limits saved', ['alive'])
            }
          >
            Save limits
          </Button>
        </Actions>
        <SpacedField as="div">
          <span className="label">Model auth levels</span>
          <span className="hint">The levels that may supply model credentials; none checked allows every level.</span>
          {MODEL_AUTH_LEVELS.map((level) => (
            <CheckRow key={level}>
              <input
                type="checkbox"
                data-testid={`edit-auth-${level}`}
                checked={authLevels.includes(level)}
                onChange={(event) =>
                  setAuthLevels(event.target.checked ? [...authLevels, level] : authLevels.filter((entry) => entry !== level))
                }
              />
              {level}
            </CheckRow>
          ))}
        </SpacedField>
        <Actions>
          <Button
            variant={ButtonVariant.Secondary}
            data-testid="save-auth"
            onClick={() =>
              void apply(
                buildWorkflowUpdateForm({ modelAuthLevels: mergeAuthLevels(meta.model_auth?.levels, authLevels) }),
                'Model auth levels saved',
                ['auth'],
              )
            }
          >
            Save model auth
          </Button>
        </Actions>
      </Section>
      <Section>
        <h2>Agents ({meta.agents.length})</h2>
        <TableScroll>
          <Table data-testid="workflow-agents">
            <thead>
              <tr>
                <th>Agent</th>
                <th>Role</th>
              </tr>
            </thead>
            <tbody>
              {meta.agents.map((agent) => (
                <tr key={agent.id}>
                  <td>
                    <EntityLink to={`/agents/${agent.id}`}>
                      <NoWrap>{agent.name}</NoWrap>
                    </EntityLink>
                  </td>
                  <td>{agent.id === meta.entry_agent ? <Badge $tone={BadgeTone.Info}>entry</Badge> : <Badge>member</Badge>}</td>
                </tr>
              ))}
            </tbody>
          </Table>
        </TableScroll>
        <EntityChecks
          label="Add agents"
          spaced
          testId="add-agents"
          hint="Only agents in every group of this workflow can be added."
          entities={agents.items.filter((agent) => !memberIds.includes(agent.id))}
          groups={workflow.groups}
          selected={addIds}
          onChange={setAddIds}
        />
        <EntityChecks label="Remove agents" testId="remove-agents" entities={meta.agents} selected={removeIds} onChange={setRemoveIds} />
        <Actions>
          <Button
            variant={ButtonVariant.Secondary}
            data-testid="apply-agents"
            disabled={addIds.length === 0 && removeIds.length === 0}
            onClick={() =>
              void apply(buildWorkflowUpdateForm({ addAgents: addIds, removeAgents: removeIds }), 'Agents changed').then((applied) => {
                // the selections stay after a refused change so it can be retried
                if (!applied) return;
                setAddIds([]);
                setRemoveIds([]);
              })
            }
          >
            Apply agent changes
          </Button>
        </Actions>
        <EntitySelect
          label="Entry agent"
          spaced
          testId="entry-select"
          entities={meta.agents}
          value={entry}
          onChange={setEntry}
          emptyText={meta.entry_agent ? 'Choose an agent' : 'None (set one before running)'}
        />
        <Actions>
          <Button
            variant={ButtonVariant.Secondary}
            data-testid="set-entry"
            disabled={entry === '' || entry === meta.entry_agent}
            onClick={() => void apply(buildWorkflowUpdateForm({ entryAgent: entry }), 'Entry agent set', ['entry'])}
          >
            Set entry agent
          </Button>
          <Button
            variant={ButtonVariant.Secondary}
            data-testid="clear-entry"
            disabled={!meta.entry_agent}
            onClick={() => void apply(buildWorkflowUpdateForm({ clearEntryAgent: true }), 'Entry agent cleared', ['entry'])}
          >
            Clear entry agent
          </Button>
        </Actions>
      </Section>
      <Section>
        <h2>Instances ({instances.length})</h2>
        {instances.length > 0 && (
          <TableScroll>
            <Table data-testid="workflow-instances">
              <thead>
                <tr>
                  <th>Instance</th>
                  <th>Status</th>
                  <th>Trigger</th>
                  <th>Triggered by</th>
                </tr>
              </thead>
              <tbody>
                {instances.map((item) => {
                  const instance = item.metadata.WorkflowInstance;
                  return (
                    <tr key={item.id} data-testid={`instance-row-${item.name}`}>
                      <td>
                        <EntityLink to={`/workflows/instances/${item.id}`}>
                          <NoWrap>{item.name}</NoWrap>
                        </EntityLink>
                      </td>
                      <td>
                        <InstanceStatusBadge status={instance.status}>
                          {instance.warnings.length > 0 && <Badge $tone={BadgeTone.Warn}>{instance.warnings.length} warnings</Badge>}
                        </InstanceStatusBadge>
                      </td>
                      <td>{triggerText(instance.trigger)}</td>
                      <td>{instance.triggered_by}</td>
                    </tr>
                  );
                })}
              </tbody>
            </Table>
          </TableScroll>
        )}
        {instances.length === 0 && <Muted data-testid="no-instances">No instances yet.</Muted>}
      </Section>
    </Page>
  );
};

/**
 * View a workflow by the id in the URL.
 *
 * The page is keyed by the id so moving between ids (such as with back and forward) starts from a clean page:
 * the previous id's content, forms, in-flight reads, and any error they reported are dropped.
 */
const WorkflowDetails: FC = () => {
  const { id = '' } = useParams();
  // keying the page by id remounts it, so every id starts loading from a clean state
  return <WorkflowPage key={id} id={id} />;
};

export default WorkflowDetails;
