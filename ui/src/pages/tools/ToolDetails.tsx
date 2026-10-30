import { useQueryClient } from '@tanstack/react-query';
import { FC, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import styled from 'styled-components';

// project imports
import { ConfirmButton } from '@components/agentic/ConfirmButton';
import { EntityNotFound } from '@components/agentic/EntityNotFound';
import { useEditableGroupNames, useGroupDetails } from '@components/agentic/hooks';
import {
  Actions,
  Badge,
  BadgeTone,
  BannerSlot,
  BannerStack,
  CodeSpans,
  EntityLink,
  Field,
  InlineList,
  KeyValues,
  Mono,
  Muted,
  PageHead,
  PageTitle,
  Pre,
  Section,
  Select,
  statusTone,
  Table,
  TextArea,
} from '@components/agentic/shared';
import { ActionHint } from '@components/agentic/tools/ActionHint';
import { addableGroups, pipelineUses, stillOffered } from '@components/agentic/tools/filters';
import { ImagePinNote } from '@components/agentic/tools/ImagePinNote';
import { PinnedBacking } from '@components/agentic/tools/PinnedBacking';
import { ToolVersions } from '@components/agentic/tools/ToolVersions';
import Page from '@components/pages/Page';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Button, ButtonSize, ButtonVariant } from '@components/shared/buttons';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import TextInput from '@components/shared/inputs/TextInput';
import Time from '@components/shared/Time';
import { getAgenticEntity, listToolInstances } from '@thorpi/agentic';
import { deleteEntity, updateEntity } from '@thorpi/entities';
import {
  deleteBlockReason,
  editBlockReason,
  toolDeleteBlockReason,
  toolGroupWideningNote,
  versionDeleteBlockReason,
} from '@utilities/agentic/access';
import {
  buildDeprecateForm,
  buildRootUpdateForm,
  buildToolPublishForm,
  kindWithArticle,
  parseToolSpec,
  specKindMismatch,
  specToText,
} from '@utilities/agentic/forms';
import {
  isRetryableReadStatus,
  partialDeleteMessage,
  portabilityLines,
  sortedToolTags,
  toolVersionLabel,
} from '@utilities/agentic/display';
import { pinnedToolInstance } from '@utilities/agentic/pins';
import { useAuth } from '@utilities/auth';
import { agenticKeys, useAgenticEntityReads } from '@utilities/queries/agentic';
import { usePipelineListQuery } from '@utilities/queries/pipelines';
import { AgenticKinds } from '@models/agentic';
import { Tool, ToolInstance, ToolKind, ToolVersion } from '@models/tools';

/** The intro sentence of a section, set apart from the field that follows it. */
/** The label that leads a row of tag badges; it matches the keys in {@link KeyValues}. */
const TagLabel = styled.span`
  color: var(--thorium-secondary-text);
  font-weight: 600;
`;

const Intro = styled.div`
  color: var(--thorium-secondary-text);
  margin-bottom: 0.75rem;
`;

/** A block of content kept off the content below it. */
const Block = styled.div`
  margin-bottom: 1rem;
`;

/** Where on the page the result of an action shows, so it appears beside the control that was used. */
type FeedbackArea = 'page' | 'versions' | 'publish' | 'edit';

/**
 * View a tool: its root settings, versions, specs, and the pipelines that use it; publish and deprecate versions.
 *
 * @param props - The props for the page; its state belongs to one id.
 * @param props.id - The id of the tool to show.
 */
const ToolPage: FC<{ id: string }> = ({ id }) => {
  const navigate = useNavigate();
  const client = useQueryClient();
  // a tool can only gain groups where the user may edit and that allow entities
  const myGroups = useEditableGroupNames();
  const { userInfo } = useAuth();
  const [tool, setTool] = useState<Tool | null>(null);
  const [instance, setInstance] = useState<ToolInstance | null>(null);
  const groupDetails = useGroupDetails(tool?.groups ?? []);
  const editReason = editBlockReason(groupDetails, userInfo);
  // why the user couldn't delete a version another user submitted; empty when they could
  const othersVersionReason = deleteBlockReason(groupDetails, userInfo, '');
  const versionIds = (tool?.metadata.Tool.instances ?? []).map((entry) => entry.id);
  // who submitted each version matters only when the user can't delete versions others submitted; each version is read
  // once, all at once, through the shared entity cache
  const versionReads = useAgenticEntityReads<ToolInstance>(othersVersionReason === '' ? [] : versionIds);
  // who submitted each version: a name once read, null when it can't be read, or undefined while it is read
  const submitters = Object.fromEntries(
    Object.entries(versionReads).map(([version, read]) => [version, read === undefined ? undefined : (read?.submitter ?? null)]),
  );
  // deleting the tool deletes every version, so a version the user can't delete blocks it too
  const deleteReason = toolDeleteBlockReason(
    groupDetails,
    userInfo,
    tool?.submitter ?? '',
    (tool?.metadata.Tool.instances ?? []).map((entry) => submitters[entry.id]),
  );
  const [selected, setSelected] = useState('');
  // the pipelines of the tool's groups, read through the cache every pipeline list shares
  const pipelineList = usePipelineListQuery(tool?.groups ?? [], tool !== null);
  const uses = useMemo(() => (tool ? pipelineUses(pipelineList.pipelines, [tool.id, ...versionIds]) : []), [tool, pipelineList.pipelines]);
  const [error, setError] = useState('');
  // the kind an opened id turned out to be when it is not a tool
  const [other, setOther] = useState<{ id: string; kind: string } | null>(null);
  // whether the tool read failed for a reason a retry may fix (such as a server error), not because the tool is missing or its id is bad
  const [readFailed, setReadFailed] = useState(false);
  const [notice, setNotice] = useState('');
  const [area, setArea] = useState<FeedbackArea>('page');
  // the block that shows the result of the last action, wherever it is on the page
  const feedbackRef = useRef<HTMLDivElement>(null);
  const [loading, setLoading] = useState(true);
  const [publishVersion, setPublishVersion] = useState('');
  const [publishSpec, setPublishSpec] = useState('');
  const [description, setDescription] = useState('');
  const [chosenAddGroup, setAddGroup] = useState('');
  const parsed = useMemo(() => parseToolSpec(publishSpec), [publishSpec]);

  /**
   * Read the tool and its instance list again.
   *
   * @param followLatest - Move the selected version to the new latest, as after a publish.
   */
  const load = useCallback(
    async (followLatest = false) => {
      setOther(null);
      setReadFailed(false);
      const found = await getAgenticEntity<Tool>(id, setError, (status) => setReadFailed(isRetryableReadStatus(status)));
      if (!found || found.kind !== AgenticKinds.Tool) {
        // an id of another kind (such as a tool instance) has no tool metadata to show
        if (found) setOther({ id, kind: found.kind });
        setLoading(false);
        return;
      }
      // the entity's own instance list may be trimmed (and is left out when empty), so read the full list including deprecated versions
      const instances = await listToolInstances(id, setError);
      found.metadata.Tool.instances = instances.length > 0 ? instances : (found.metadata.Tool.instances ?? []);
      setTool(found);
      setDescription(found.description ?? '');
      setLoading(false);
      // keep the selected version only while it still exists, so a deleted selection moves to latest
      const ids = (found.metadata.Tool.instances ?? []).map((entry) => entry.id);
      setSelected((current) => (!followLatest && ids.includes(current) ? current : found.metadata.Tool.latest || ids[0] || ''));
    },
    [id],
  );

  useEffect(() => {
    void load();
  }, [load]);

  // read the selected instance's spec
  useEffect(() => {
    // with no version selected (the tool has none left) no spec is shown
    if (selected === '') {
      setInstance(null);
      return;
    }
    // a read that finishes after another version was chosen must not replace that version's spec
    let cancelled = false;
    // an error from a read the user has moved past must not be shown beside the newer version's spec
    void getAgenticEntity<ToolInstance>(selected, (message) => {
      if (!cancelled) setError(message);
    }).then((found) => {
      if (cancelled) return;
      setInstance(found);
      // the version read is shared, so its submitter is not read again
      if (found) client.setQueryData(agenticKeys.entity(found.id), found);
      // start the publish form from the latest spec so a change is a small edit
      if (found && found.id === tool?.metadata.Tool.latest) {
        setPublishSpec((current) => (current === '' ? specToText(found.metadata.ToolInstance.spec) : current));
      }
    });
    return () => {
      cancelled = true;
    };
  }, [selected, tool?.metadata.Tool.latest]);

  // start the publish form from the latest spec even while another version is the one shown
  useEffect(() => {
    const latest = tool?.metadata.Tool.latest;
    // the selected spec read fills the form when the latest is the selected version, so only another selection needs its own read
    if (!latest || selected === '' || selected === latest) return;
    let cancelled = false;
    void getAgenticEntity<ToolInstance>(latest, () => undefined).then((found) => {
      if (cancelled || !found) return;
      setPublishSpec((current) => (current === '' ? specToText(found.metadata.ToolInstance.spec) : current));
    });
    return () => {
      cancelled = true;
    };
  }, [selected, tool?.metadata.Tool.latest]);

  /**
   * Run an action, then show its result beside the control it came from and read the tool again.
   *
   * @param where - The part of the page whose controls ran the action.
   * @param work - The action; resolves `true` when it succeeded.
   * @param done - The notice to show after a success.
   * @param followLatest - Move the spec view to the new latest version after a success.
   */
  const act = async (where: FeedbackArea, work: () => Promise<boolean>, done: string, followLatest = false) => {
    setArea(where);
    setError('');
    setNotice('');
    if (await work()) {
      setNotice(done);
      await load(followLatest);
    }
  };

  /**
   * Render the error and notice banners when they belong to a part of the page.
   *
   * @param where - The part of the page asking for its banners.
   */
  const feedback = (where: FeedbackArea) => {
    // the page banner sits in the page's banner slot, and a banner in a section in a stack that keeps it off the control above it
    const Wrapper = where === 'page' ? BannerSlot : BannerStack;
    if (where !== area || (error === '' && notice === '')) return null;
    // the wrapper only exists while a banner is shown, so an idle page adds no blank space of its own;
    // the API marks names with markdown backticks
    return (
      <Wrapper ref={feedbackRef}>
        {error !== '' && (
          <AlertBanner>
            {/* a message can lead with an outcome line (such as what a partial delete removed) before the API error */}
            {error.split('\n').map((line, index) => (
              <div key={index} data-testid="tool-error-line">
                <CodeSpans text={line} />
              </div>
            ))}
          </AlertBanner>
        )}
        {notice !== '' && (
          <AlertBanner severity={Severity.Success}>
            <span data-testid="tool-notice">{notice}</span>
          </AlertBanner>
        )}
      </Wrapper>
    );
  };
  // a result below the control that was used can land under the bottom of the window, so bring it into view,
  // again once the tool and the selected version are read back after the action, since each reread can move the result down the page
  useEffect(() => {
    const node = feedbackRef.current;
    if ((error === '' && notice === '') || !node) return;
    // a result already in view stays put; one out of view is centred, so a later reread that grows the page above it
    // still leaves it on screen
    const { top, bottom } = node.getBoundingClientRect();
    if (top < 0 || bottom > window.innerHeight) {
      // the window is scrolled directly: scrollIntoView can stop at a page wrapper that clips overflow and scroll only it
      window.scrollBy({ top: (top + bottom - window.innerHeight) / 2, behavior: 'instant' });
    }
  }, [error, notice, area, tool, instance]);

  if (loading)
    return (
      <Page title="Tool · Thorium">
        <LoadingSpinner loading={true} />
      </Page>
    );
  if (!tool) {
    return (
      <EntityNotFound
        what="Tool"
        loading={false}
        listPath="/tools"
        listName="tools"
        error={error}
        failed={readFailed}
        onRetry={() => {
          // show the spinner while the tool is read again
          setError('');
          setLoading(true);
          void load();
        }}
        other={other ?? undefined}
      />
    );
  }
  const meta = tool.metadata.Tool;
  const addable = addableGroups(myGroups, tool.groups);
  // a group chosen while the group details loaded may no longer be offered; it is not kept
  const addGroup = stillOffered([chosenAddGroup], addable)[0] ?? '';
  const runnable = meta.kind !== ToolKind.Harness;
  const spec = instance?.metadata.ToolInstance;
  // the Image tool version a container backed agentic tool runs
  const pinnedId = pinnedToolInstance(spec?.spec);
  const systemTags = Object.entries(tool.tags);
  // the selected version's Tool* tags (ToolId, ToolVersion, ToolKind, ToolSpecHash) live on its instance
  const versionTags = sortedToolTags(instance?.tags ?? {});
  /**
   * Explain why the user can't delete a version, from its own submitter and the tool's groups.
   *
   * @param instanceId - The version's instance id.
   */
  const versionDeleteReason = (instanceId: string) => versionDeleteBlockReason(groupDetails, userInfo, submitters[instanceId]);
  // the description only changes when the trimmed text differs, and blanking it clears it
  const descriptionChanged = description.trim() !== (tool.description ?? '').trim();
  const clearsDescription = descriptionChanged && description.trim() === '';
  const rootChanged = descriptionChanged || addGroup !== '';
  const publishProblem =
    publishVersion.trim() === ''
      ? 'Enter a version label.'
      : (parsed.error ??
        (parsed.kind !== null && parsed.kind !== meta.kind
          ? specKindMismatch(parsed.kind, `this is ${kindWithArticle(meta.kind)} tool`)
          : ''));

  return (
    <Page title={`${tool.name} · Thorium`}>
      <PageHead>
        <PageTitle data-testid="tool-title">{tool.name}</PageTitle>
        <Actions>
          {runnable && (
            <Button
              variant={ButtonVariant.Primary}
              data-testid="run-tool"
              onClick={() => void navigate(`/launch?tab=tool&tool=${tool.id}`)}
            >
              Run this tool
            </Button>
          )}
          <ConfirmButton
            size={ButtonSize.Medium}
            testId="delete-tool"
            disabled={deleteReason !== ''}
            title={deleteReason}
            confirmText="Delete the tool and all its versions"
            onConfirm={() =>
              void act(
                'page',
                async () => {
                  // a tool with versions cannot be deleted, so remove each version first
                  const deleted: ToolVersion[] = [];
                  for (const version of meta.instances ?? []) {
                    let refusal = '';
                    if (await deleteEntity(version.id, (message) => (refusal = message))) deleted.push(version.version);
                    else {
                      // the versions deleted before this refusal are gone, so say so and read the list again
                      setError(partialDeleteMessage(refusal, deleted));
                      await load();
                      return false;
                    }
                  }
                  const gone = await deleteEntity(tool.id, setError);
                  if (gone) void navigate('/tools');
                  // a refused delete leaves the tool in place, so read it again
                  else await load();
                  return false;
                },
                '',
              )
            }
          >
            Delete tool
          </ConfirmButton>
        </Actions>
      </PageHead>
      {feedback('page')}
      {editReason !== '' && (
        <BannerSlot>
          <AlertBanner severity={Severity.Info}>
            <span data-testid="tool-read-only">
              You can view this tool but not change it: changing a tool needs edit rights in every group it is in.
            </span>
          </AlertBanner>
        </BannerSlot>
      )}
      <Section>
        <h2>Overview</h2>
        <KeyValues>
          <dt>Kind</dt>
          <dd>
            <Badge $tone={BadgeTone.Info}>{meta.kind}</Badge>
          </dd>
          <dt>Latest</dt>
          <dd data-testid="tool-latest">
            {meta.latest_version ? (
              <Badge>{toolVersionLabel(meta.latest_version)}</Badge>
            ) : (
              <Badge $tone={statusTone('no live version')}>no live version</Badge>
            )}
          </dd>
          <dt>Groups</dt>
          <dd data-testid="tool-groups">{tool.groups.join(', ')}</dd>
          <dt>Description</dt>
          <dd>{tool.description ?? <Muted>none</Muted>}</dd>
          <dt>Submitter</dt>
          <dd>{tool.submitter}</dd>
          <dt>Created</dt>
          <dd>
            <Time>{tool.created}</Time>
          </dd>
          <dt>Tool id</dt>
          <dd>
            <Mono>{tool.id}</Mono>
          </dd>
          {meta.kind === ToolKind.Harness && (
            <>
              <dt>Disabled models</dt>
              <dd>{(meta.disabled_models ?? []).join(', ') || <Muted>none</Muted>}</dd>
            </>
          )}
        </KeyValues>
        {versionTags.length > 0 && spec && (
          <InlineList style={{ marginTop: '0.75rem' }} data-testid="version-tags">
            <TagLabel>Tags of {`${tool.name}@${toolVersionLabel(spec.version)}`}</TagLabel>
            {versionTags.flatMap(([key, values]) =>
              Object.keys(values).map((value) => <Badge key={`${key}=${value}`}>{`${key}=${value}`}</Badge>),
            )}
          </InlineList>
        )}
        {systemTags.length > 0 && (
          <InlineList style={{ marginTop: '0.75rem' }} data-testid="tool-tags">
            <TagLabel>Tool tags (all versions)</TagLabel>
            {systemTags.flatMap(([key, values]) =>
              Object.keys(values).map((value) => <Badge key={`${key}=${value}`}>{`${key}=${value}`}</Badge>),
            )}
          </InlineList>
        )}
      </Section>
      <Section>
        <h2>Versions</h2>
        <ToolVersions
          instances={meta.instances ?? []}
          latest={meta.latest}
          selected={selected}
          runnable={runnable}
          editReason={editReason}
          deleteReasonOf={versionDeleteReason}
          onSelect={(instanceId) => {
            // a failure to read the spec shows beside the versions; the last result is dropped so the page doesn't scroll back to it
            setArea('versions');
            setNotice('');
            setError('');
            setSelected(instanceId);
          }}
          onRun={(instanceId) => void navigate(`/launch?tab=tool&tool=${tool.id}&instance=${instanceId}`)}
          onDeprecate={(instanceId, deprecated) =>
            void act(
              'versions',
              () => updateEntity(instanceId, buildDeprecateForm(deprecated), setError),
              deprecated ? 'Version deprecated' : 'Version restored',
            )
          }
          onDelete={(instanceId) => void act('versions', () => deleteEntity(instanceId, setError), 'Version deleted')}
        />
        {meta.kind === ToolKind.Image && (
          // the gap keeps the note off the versions table above it
          <div style={{ marginTop: '0.75rem' }}>
            <ImagePinNote />
          </div>
        )}
        {feedback('versions')}
      </Section>
      {spec && (
        <Section>
          <h2>Spec of {instance ? `${tool.name}@${toolVersionLabel(spec.version)}` : ''}</h2>
          {spec.portability && !(spec.portability.strict_compatible && spec.portability.gemini_compatible) && (
            // the gap keeps the banner off the spec below it
            <Block>
              <AlertBanner severity={Severity.Warning}>
                <div data-testid="portability">
                  <div data-testid="portability-strict">
                    <CodeSpans text={portabilityLines(spec.portability).strict} />
                  </div>
                  <div data-testid="portability-gemini">
                    <CodeSpans text={portabilityLines(spec.portability).gemini} />
                  </div>
                </div>
              </AlertBanner>
            </Block>
          )}
          {pinnedId && (
            // the gap keeps the row off the spec below it
            <Block>
              <KeyValues>
                <dt>Pinned to</dt>
                <dd>
                  <PinnedBacking id={pinnedId} />
                </dd>
              </KeyValues>
            </Block>
          )}
          <Pre data-testid="spec-view">{specToText(spec.spec)}</Pre>
        </Section>
      )}
      <Section>
        <h2>Publish a new version</h2>
        <Intro>
          A version is immutable: publishing creates a new version and moves latest to it. A spec identical to the latest is refused.
        </Intro>
        <Field>
          <span className="label">Version label</span>
          <TextInput
            data-testid="publish-version"
            value={publishVersion}
            disabled={editReason !== ''}
            onChange={(event) => setPublishVersion(event.target.value)}
          />
        </Field>
        <Field>
          <span className="label">Spec (YAML or JSON)</span>
          <TextArea
            data-testid="publish-spec"
            style={{ minHeight: '14rem' }}
            value={publishSpec}
            disabled={editReason !== ''}
            onChange={(event) => setPublishSpec(event.target.value)}
          />
        </Field>
        {editReason === '' && publishProblem !== '' && publishVersion.trim() !== '' && (
          // a missing version label is a hint beside the button, while a problem with the spec is a warning as it is when creating a tool
          <AlertBanner severity={Severity.Warning} className="mb-3">
            <span data-testid="publish-problem">{publishProblem}</span>
          </AlertBanner>
        )}
        <Actions>
          <Button
            variant={ButtonVariant.Ok}
            data-testid="publish-submit"
            disabled={editReason !== '' || publishProblem !== '' || parsed.json === null}
            title={editReason || undefined}
            onClick={() =>
              void act(
                'publish',
                async () => {
                  const done = await updateEntity(
                    tool.id,
                    buildToolPublishForm({ version: publishVersion, spec: parsed.json ?? '' }),
                    setError,
                  );
                  if (done) setPublishVersion('');
                  return done;
                },
                'Version published',
                true,
              )
            }
          >
            Publish version
          </Button>
          {editReason !== '' && <ActionHint data-testid="publish-blocked">{editReason}</ActionHint>}
          {editReason === '' && publishVersion.trim() === '' && <ActionHint data-testid="publish-problem">{publishProblem}</ActionHint>}
        </Actions>
        {feedback('publish')}
      </Section>
      <Section>
        <h2>Edit root</h2>
        <Field>
          <span className="label">Description</span>
          <TextInput
            data-testid="edit-description"
            value={description}
            disabled={editReason !== ''}
            onChange={(event) => setDescription(event.target.value)}
          />
        </Field>
        <Field>
          <span className="label">Add to a group</span>
          {addGroup !== '' && (
            <span className="hint" data-testid="edit-add-group-note">
              <CodeSpans text={toolGroupWideningNote(addGroup, meta.kind === ToolKind.Image)} />
            </span>
          )}
          <Select
            data-testid="edit-add-group"
            value={addGroup}
            disabled={editReason !== ''}
            onChange={(event) => setAddGroup(event.target.value)}
          >
            <option value="">Choose a group</option>
            {addable.map((group) => (
              <option key={group} value={group}>
                {group}
              </option>
            ))}
          </Select>
        </Field>
        <Actions>
          <Button
            variant={ButtonVariant.Secondary}
            data-testid="edit-save"
            disabled={editReason !== '' || !rootChanged}
            title={editReason || undefined}
            onClick={() =>
              void act(
                'edit',
                async () => {
                  const done = await updateEntity(
                    tool.id,
                    buildRootUpdateForm({
                      description: descriptionChanged ? description : undefined,
                      clearDescription: clearsDescription,
                      addGroups: addGroup ? [addGroup] : [],
                    }),
                    setError,
                  );
                  // the group is now one of the tool's, so it must not be sent again by the next save
                  if (done) setAddGroup('');
                  return done;
                },
                'Tool updated',
              )
            }
          >
            Save changes
          </Button>
          {editReason !== '' && <ActionHint data-testid="edit-blocked">{editReason}</ActionHint>}
          {editReason === '' && !rootChanged && (
            <ActionHint data-testid="edit-unchanged">Change the description or choose a group to save.</ActionHint>
          )}
        </Actions>
        {feedback('edit')}
      </Section>
      <Section>
        <h2>Used by pipelines</h2>
        {uses.length === 0 ? (
          <Muted data-testid="no-pipeline-use">No pipeline stage uses this tool.</Muted>
        ) : (
          <Table data-testid="pipeline-use">
            <thead>
              <tr>
                <th>Pipeline</th>
                <th>Group</th>
                <th>Stages</th>
              </tr>
            </thead>
            <tbody>
              {uses.map((use) => (
                <tr key={`${use.group}/${use.name}`}>
                  <td>
                    <InlineList>
                      <EntityLink to={`/pipelines#${use.name}--${use.group}`}>{use.name}</EntityLink>
                      <Badge>pipeline</Badge>
                    </InlineList>
                  </td>
                  <td>{use.group}</td>
                  <td>{use.stages.join(', ')}</td>
                </tr>
              ))}
            </tbody>
          </Table>
        )}
      </Section>
    </Page>
  );
};

/**
 * View a tool by the id in the URL.
 *
 * The page is keyed by the id so moving between tools (such as with back and forward) starts from a clean page:
 * the previous tool's content, selected version, form and publish text, and in-flight reads (and any error they would report) are dropped,
 * and a missing or wrong-kind id never shows the tool from the previous id.
 */
const ToolDetails: FC = () => {
  const { id = '' } = useParams();
  // keying the page by id remounts it, so every id starts loading from a clean state
  return <ToolPage key={id} id={id} />;
};

export default ToolDetails;
