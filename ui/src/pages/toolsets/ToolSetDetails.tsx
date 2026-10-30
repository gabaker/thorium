import { FC, useCallback, useEffect, useRef, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import styled from 'styled-components';

// project imports
import {
  Actions,
  BannerSlot,
  Badge,
  CenteredTable,
  CodeSpans,
  BadgeTone,
  EntityLink,
  Field,
  InlineList,
  KeyValues,
  Muted,
  PageHead,
  PageTitle,
  Row,
  Section,
  Table,
} from '@components/agentic/shared';
import { ConfirmButton } from '@components/agentic/ConfirmButton';
import { UsedBy } from '@components/agentic/UsedBy';
import { EntityNotFound } from '@components/agentic/EntityNotFound';
import { InstanceStatusBadge } from '@components/agentic/InstanceStatusBadge';
import { useAgenticList, useEntityAccess, useRequestInstance } from '@components/agentic/hooks';
import { Notice, rootIsStale } from '@components/agentic/instances';
import { versionNote } from '@components/agentic/agents/names';
import { EntityChecks, VersionPins } from '@components/agentic/pickers';
import { buildToolSetUpdateForm, selectorText } from '@components/agentic/toolsets/forms';
import { ToolSetInstanceView } from '@components/agentic/toolsets/ToolSetInstanceView';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Button, ButtonSize } from '@components/shared/buttons';
import { ButtonVariant } from '@components/shared/buttons/types';
import TextInput from '@components/shared/inputs/TextInput';
import Page from '@components/pages/Page';
import { getAgenticEntity, listAllAgentic } from '@thorpi/agentic';
import { deleteEntity, updateEntity } from '@thorpi/entities';
import { buildRootUpdateForm, selectorError } from '@utilities/agentic/forms';
import { isInstanceReady } from '@utilities/agentic/display';
import { applyPins, toolPinChoice } from '@utilities/agentic/pins';
import { agentsUsingToolSet } from '@utilities/agentic/usedBy';
import { invalidateAgentic } from '@utilities/queries/agentic';
import { useChangeAndInvalidate } from '@utilities/queries/changes';
import { AgenticKinds } from '@models/agentic';
import { Agent } from '@models/agents';
import { Tool, ToolInstanceEntity, ToolKind } from '@models/tools';
import { ToolSet, ToolSetInstance } from '@models/toolsets';

/** The instances table with its action column pushed to the right edge and kept to the width of its buttons. */
const InstanceTable = styled(CenteredTable)`
  th:last-child,
  td:last-child {
    width: 1%;
    text-align: right;
    white-space: nowrap;
  }
`;

/** An edit field group of the toolset page that can be reset to its stored value independently of the others. */
type EditField = 'description' | 'selector';

/** Every edit field group, reset together when the page loads or is reloaded. */
const ALL_FIELDS: EditField[] = ['description', 'selector'];

/**
 * Show a toolset, its instances, and the edits and actions it supports.
 *
 * @param props - The props for the page; its state belongs to one id.
 * @param props.id - The id of the toolset to show.
 */
const ToolSetPage: FC<{ id: string }> = ({ id }) => {
  const navigate = useNavigate();
  // a change marks the cached lists and entities stale, so other pages show it at once
  const change = useChangeAndInvalidate(invalidateAgentic);
  const [set, setSet] = useState<ToolSet | null>(null);
  const [instances, setInstances] = useState<ToolSetInstance[]>([]);
  const [chosen, setChosen] = useState<string>('');
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  // the kind an opened id turned out to be when it is not a toolset
  const [other, setOther] = useState<{ id: string; kind: string } | null>(null);
  // whether the toolset read failed for a reason other than being missing or hidden (such as a server error); a retry may fix it
  const [readFailed, setReadFailed] = useState(false);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [description, setDescription] = useState('');
  const [selectorKey, setSelectorKey] = useState('');
  const [selectorValue, setSelectorValue] = useState('');
  const [addIds, setAddIds] = useState<string[]>([]);
  const [removeIds, setRemoveIds] = useState<string[]>([]);
  const [pins, setPins] = useState<Record<string, string>>({});
  const allTools = useAgenticList<Tool>(AgenticKinds.Tool);
  // the agents that use this toolset are found by looking through every agent the user can see
  const agents = useAgenticList<Agent>(AgenticKinds.Agent);
  const { deleteReason } = useEntityAccess(set?.groups ?? [], set?.submitter ?? '');
  /** Read this toolset's instances, newest revision first. */
  const readInstances = useCallback(async () => {
    // instances are separate entities that name their toolset, so list and filter them
    const all = await listAllAgentic<ToolSetInstance>(AgenticKinds.ToolSetInstance, [], setError);
    return all
      .filter((item) => item.metadata.ToolSetInstance.tool_set === id)
      .sort((a, b) => b.metadata.ToolSetInstance.revision - a.metadata.ToolSetInstance.revision);
  }, [id]);
  /** Read the toolset and its instances, and reset the given edit fields (all by default) to what is stored; other unsaved edits stay. */
  const load = useCallback(
    async (reset: EditField[] = ALL_FIELDS) => {
      setLoading(true);
      setOther(null);
      setReadFailed(false);
      const read = await getAgenticEntity<ToolSet>(id, setError, (status) => setReadFailed(![401, 403, 404].includes(status)));
      // an id of another kind (such as an agent) has no toolset metadata to show
      if (read && read.kind !== AgenticKinds.ToolSet) setOther({ id, kind: read.kind });
      const found = read && read.kind === AgenticKinds.ToolSet ? read : null;
      setSet(found);
      if (reset.includes('description')) setDescription(found?.description ?? '');
      if (reset.includes('selector')) {
        setSelectorKey(found?.metadata.ToolSet.selector?.key ?? '');
        setSelectorValue(found?.metadata.ToolSet.selector?.value ?? '');
      }
      // an id that is not this kind has no instances to read
      if (found) setInstances(await readInstances());
      setLoading(false);
    },
    [id, readInstances],
  );
  useEffect(() => {
    void load();
  }, [load]);
  // the highest revision of the toolset as last read, kept for the watch's reads to compare against
  const highestRef = useRef(0);
  highestRef.current = set?.metadata.ToolSet.highest_revision ?? 0;
  /** Read the instances for the build watch, and the toolset again when a build raised its highest revision. */
  const readForWatch = useCallback(async () => {
    const current = await readInstances();
    // a build outran the toolset the page shows, so refresh it without touching the edit fields
    if (
      rootIsStale(
        current.map((item) => item.metadata.ToolSetInstance.revision),
        highestRef.current,
      )
    ) {
      const found = await getAgenticEntity<ToolSet>(id, setError);
      if (found) setSet(found);
    }
    return current;
  }, [id, readInstances]);
  const request = useRequestInstance({ root: 'toolsets', id, instances, read: readForWatch, setInstances, setError, setNotice });
  if (!set) {
    return (
      <EntityNotFound
        what="Toolset"
        loading={loading}
        listPath="/toolsets"
        listName="toolsets"
        error={error}
        failed={readFailed}
        onRetry={() => {
          // show the spinner while the toolset is read again
          setError('');
          setLoading(true);
          void load();
        }}
        other={other ?? undefined}
      />
    );
  }
  const meta = set.metadata.ToolSet;
  const selectorProblem = selectorError(selectorKey, selectorValue);
  // a pinned member is a tool instance, so its tool is the one already in the toolset
  const memberIds = meta.tools.map((tool) =>
    tool.kind === AgenticKinds.ToolInstance ? (tool.metadata.ToolInstance as ToolInstanceEntity).tool : tool.id,
  );
  const agentic = allTools.items.filter((tool) => tool.metadata.Tool.kind === ToolKind.Agentic);
  /**
   * Send an update of the toolset, then show the notice and read it again; resolves to whether the update was applied.
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
  const remove = async () => {
    if (await change(() => deleteEntity(id, setError))) void navigate('/toolsets');
  };
  // a revision the user picked stays shown; otherwise the newest Ready instance (or the newest) follows new builds
  const shownInstance =
    instances.find((item) => item.id === chosen) ??
    instances.find((item) => isInstanceReady(item.metadata.ToolSetInstance.status)) ??
    instances[0];
  return (
    <Page title={`${set.name} · Thorium`}>
      <PageHead>
        <PageTitle data-testid="toolset-title">{set.name}</PageTitle>
        <Actions>
          <Button variant={ButtonVariant.Primary} data-testid="request-instance" onClick={() => void request()}>
            Request instance
          </Button>
          <Button variant={ButtonVariant.Secondary} data-testid="reload" onClick={() => void load()}>
            Reload
          </Button>
          <ConfirmButton testId="delete-toolset" disabled={deleteReason !== ''} title={deleteReason} onConfirm={() => void remove()}>
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
        <KeyValues>
          <dt>Groups</dt>
          <dd>{set.groups.join(', ')}</dd>
          <dt>Description</dt>
          <dd>{set.description ?? <Muted>none</Muted>}</dd>
          <dt>Read only</dt>
          <dd>{String(meta.read_only)}</dd>
          <dt>Selector</dt>
          <dd>{meta.selector ? selectorText(meta.selector) : <Muted>none</Muted>}</dd>
          <dt>Auto instance</dt>
          <dd>{String(meta.auto_instance)}</dd>
          <dt>Highest revision</dt>
          <dd>{meta.highest_revision}</dd>
          <dt>Used by agents</dt>
          <dd data-testid="toolset-used-by">
            <UsedBy
              users={agentsUsingToolSet(agents.items, set.id)}
              linkTo={(agentId) => `/agents/${agentId}`}
              loading={agents.loading}
              error={agents.error}
            />
          </dd>
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
        </Actions>
        <h3>Flags</h3>
        <Actions>
          <Button
            variant={ButtonVariant.Secondary}
            data-testid="toggle-read-only"
            onClick={() => void apply(buildToolSetUpdateForm({ readOnly: !meta.read_only }), 'Read only changed')}
          >
            {meta.read_only ? 'Turn read only off' : 'Turn read only on'}
          </Button>
          <Button
            variant={ButtonVariant.Secondary}
            data-testid="toggle-auto-instance"
            onClick={() => void apply(buildToolSetUpdateForm({ autoInstance: !meta.auto_instance }), 'Auto instance changed')}
          >
            {meta.auto_instance ? 'Turn auto instance off' : 'Turn auto instance on'}
          </Button>
        </Actions>
        <h3>Selector</h3>
        <Row>
          <Field style={{ flex: '1 1 14rem' }}>
            <span className="label">Selector key</span>
            <TextInput data-testid="edit-selector-key" value={selectorKey} onChange={(event) => setSelectorKey(event.target.value)} />
          </Field>
          <Field style={{ flex: '1 1 14rem' }}>
            <span className="label">Selector value</span>
            <TextInput data-testid="edit-selector-value" value={selectorValue} onChange={(event) => setSelectorValue(event.target.value)} />
            {selectorProblem && (
              <span className="error" data-testid="edit-selector-error">
                {selectorProblem}
              </span>
            )}
          </Field>
        </Row>
        <Actions>
          <Button
            variant={ButtonVariant.Secondary}
            data-testid="save-selector"
            disabled={selectorKey.trim() === '' || selectorValue.trim() === ''}
            onClick={() =>
              void apply(buildToolSetUpdateForm({ selector: { key: selectorKey.trim(), value: selectorValue.trim() } }), 'Selector saved', [
                'selector',
              ])
            }
          >
            Save selector
          </Button>
          <Button
            variant={ButtonVariant.Secondary}
            data-testid="clear-selector"
            disabled={!meta.selector}
            onClick={() => void apply(buildToolSetUpdateForm({ clearSelector: true }), 'Selector cleared', ['selector'])}
          >
            Clear selector
          </Button>
        </Actions>
      </Section>
      <Section>
        <h2>Explicit tools ({meta.tools.length})</h2>
        {meta.tools.length === 0 ? (
          <Muted data-testid="no-tools">No explicit tools.</Muted>
        ) : (
          <Table data-testid="toolset-tools">
            <thead>
              <tr>
                <th>Tool</th>
                <th>Version</th>
              </tr>
            </thead>
            <tbody>
              {meta.tools.map((tool) => {
                const pinned = tool.kind === AgenticKinds.ToolInstance;
                const toolId = pinned ? (tool.metadata.ToolInstance as ToolInstanceEntity).tool : tool.id;
                return (
                  <tr key={tool.id}>
                    <td>
                      <EntityLink to={`/tools/${toolId}`}>{tool.name}</EntityLink>
                    </td>
                    <td>{versionNote(tool)}</td>
                  </tr>
                );
              })}
            </tbody>
          </Table>
        )}
        <EntityChecks
          label="Add tools"
          spaced
          testId="add-tools"
          hint="Only tools in every group of this toolset can be added"
          entities={agentic.filter((tool) => !memberIds.includes(tool.id))}
          groups={set.groups}
          selected={addIds}
          onChange={setAddIds}
        />
        <VersionPins
          label="Versions of the tools to add"
          testId="add-tool-pins"
          choices={agentic.filter((tool) => addIds.includes(tool.id)).map(toolPinChoice)}
          pins={pins}
          onChange={setPins}
        />
        <EntityChecks label="Remove tools" testId="remove-tools" entities={meta.tools} selected={removeIds} onChange={setRemoveIds} />
        <Button
          variant={ButtonVariant.Secondary}
          data-testid="apply-tools"
          disabled={addIds.length === 0 && removeIds.length === 0}
          onClick={() =>
            void apply(buildToolSetUpdateForm({ addTools: applyPins(addIds, pins), removeTools: removeIds }), 'Tools changed').then(
              (applied) => {
                // the selections stay after a refused change so it can be retried
                if (!applied) return;
                setAddIds([]);
                setRemoveIds([]);
                setPins({});
              },
            )
          }
        >
          Apply tool changes
        </Button>
      </Section>
      <Section>
        <h2>Instances ({instances.length})</h2>
        {instances.length === 0 ? (
          <Muted data-testid="no-instances">No instances yet.</Muted>
        ) : (
          <InstanceTable data-testid="toolset-instances">
            <thead>
              <tr>
                <th>Instance</th>
                <th>Status</th>
                <th>Members</th>
                <th aria-label="Actions" />
              </tr>
            </thead>
            <tbody>
              {instances.map((item) => (
                <tr
                  key={item.id}
                  data-testid={`instance-row-${item.name}`}
                  aria-selected={item.id === shownInstance?.id}
                  style={item.id === shownInstance?.id ? { background: 'var(--thorium-highlight-panel-bg)' } : undefined}
                >
                  <td>
                    <InlineList>
                      <span>{item.name}</span>
                      {item.id === shownInstance?.id && <Badge $tone={BadgeTone.Info}>shown</Badge>}
                    </InlineList>
                  </td>
                  <td>
                    <InstanceStatusBadge status={item.metadata.ToolSetInstance.status} />
                  </td>
                  <td>{item.metadata.ToolSetInstance.members.length}</td>
                  <td>
                    <Button variant={ButtonVariant.Ghost} size={ButtonSize.Small} onClick={() => setChosen(item.id)}>
                      View
                    </Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </InstanceTable>
        )}
        {shownInstance && <ToolSetInstanceView instance={shownInstance} />}
      </Section>
    </Page>
  );
};

/**
 * View a toolset by the id in the URL.
 *
 * The page is keyed by the id so moving between ids (such as with back and forward) starts from a clean page:
 * the previous id's content, forms, in-flight reads, and any error they reported are dropped.
 */
const ToolSetDetails: FC = () => {
  const { id = '' } = useParams();
  // keying the page by id remounts it, so every id starts loading from a clean state
  return <ToolSetPage key={id} id={id} />;
};

export default ToolSetDetails;
