import { useQueryClient } from '@tanstack/react-query';
import { FC, useEffect, useMemo, useRef, useState } from 'react';
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
import { AgenticReadError, invalidateAgentic, useAgenticEntityRead, useAgenticEntityReads } from '@utilities/queries/agentic';
import { usePipelineListQuery } from '@utilities/queries/pipelines';
import { toolKeys, useToolRead } from '@utilities/queries/tools';
import { ToolInstance, ToolKind, ToolVersion } from '@models/tools';

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
  // the tool and its full version list, read through the cache and again on every visit
  const toolQuery = useToolRead(id);
  const read = toolQuery.data;
  const tool = read && 'tool' in read ? read.tool : null;
  // the kind an opened id turned out to be when it is not a tool
  const other = read && 'other' in read ? read.other : null;
  // why the last tool read failed; a reread that fails after the tool was read leaves the tool shown
  const readError = toolQuery.error?.message ?? '';
  // whether the tool read failed for a reason a retry may fix (such as a server error), not because the tool is missing or its id is bad
  const readFailed = toolQuery.error instanceof AgenticReadError && isRetryableReadStatus(toolQuery.error.status);
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
    Object.entries(versionReads).map(([version, entity]) => [version, entity === undefined ? undefined : (entity?.submitter ?? null)]),
  );
  // deleting the tool deletes every version, so a version the user can't delete blocks it too
  const deleteReason = toolDeleteBlockReason(
    groupDetails,
    userInfo,
    tool?.submitter ?? '',
    (tool?.metadata.Tool.instances ?? []).map((entry) => submitters[entry.id]),
  );
  // the version the user chose to view; empty follows the tool's latest version
  const [chosen, setChosen] = useState('');
  // the version shown: the chosen one while it still exists, so a deleted choice moves to latest
  const selected = versionIds.includes(chosen) ? chosen : tool?.metadata.Tool.latest || versionIds[0] || '';
  // the shown version's spec; with no version left no spec is shown, and a read the user has moved past is not shown
  const selectedRead = useAgenticEntityRead<ToolInstance>(selected);
  const instance = selectedRead.entity ?? null;
  // the latest spec, which the publish form starts from so a change is a small edit
  const latestRead = useAgenticEntityRead<ToolInstance>(tool?.metadata.Tool.latest ?? '');
  // the pipelines of the tool's groups, read through the cache every pipeline list shares
  const pipelineList = usePipelineListQuery(tool?.groups ?? [], tool !== null);
  const uses = useMemo(() => (tool ? pipelineUses(pipelineList.pipelines, [tool.id, ...versionIds]) : []), [tool, pipelineList.pipelines]);
  const [error, setError] = useState('');
  // the error shown: the last action's, else a failed reread of the tool, its version list, or the shown spec
  const shownError =
    [error, tool ? readError : '', read && 'tool' in read ? read.versionsError : '', selectedRead.error].find(
      (message) => message !== '',
    ) ?? '';
  const [notice, setNotice] = useState('');
  const [area, setArea] = useState<FeedbackArea>('page');
  // the block that shows the result of the last action, wherever it is on the page
  const feedbackRef = useRef<HTMLDivElement>(null);
  const [publishVersion, setPublishVersion] = useState('');
  // the publish spec as typed; until it is typed in, the form holds the latest spec
  const [publishDraft, setPublishDraft] = useState<string | null>(null);
  const latestSpec = latestRead.entity?.metadata.ToolInstance.spec;
  const publishSpec = publishDraft ?? (latestSpec === undefined ? '' : specToText(latestSpec));
  // the description as typed; until it is typed in, the field holds the stored description
  const [descriptionDraft, setDescriptionDraft] = useState<string | null>(null);
  const description = descriptionDraft ?? tool?.description ?? '';
  const [chosenAddGroup, setAddGroup] = useState('');
  const parsed = useMemo(() => parseToolSpec(publishSpec), [publishSpec]);

  /**
   * Run an action, then show its result beside the control it came from and read the tool again. The cached lists and
   * entities are marked stale whatever the outcome, since a refused delete of a tool may have removed some versions.
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
    let succeeded = false;
    try {
      succeeded = await work();
    } finally {
      // the version shown stays shown while it exists, unless a success should move the view to the new latest
      setChosen(succeeded && followLatest ? '' : selected);
      if (succeeded) setNotice(done);
      // the action ends once this tool is read back, along with every other list and entity in use
      await invalidateAgentic(client);
      // the typed description stays until the tool is read back with it, and stays shown when that read fails
      if (succeeded && client.getQueryState(toolKeys.withVersions(id))?.status !== 'error') setDescriptionDraft(null);
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
    if (where !== area || (shownError === '' && notice === '')) return null;
    // the wrapper only exists while a banner is shown, so an idle page adds no blank space of its own;
    // the API marks names with markdown backticks
    return (
      <Wrapper ref={feedbackRef}>
        {shownError !== '' && (
          <AlertBanner>
            {/* a message can lead with an outcome line (such as what a partial delete removed) before the API error */}
            {shownError.split('\n').map((line, index) => (
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
    if ((shownError === '' && notice === '') || !node) return;
    // a result already in view stays put; one out of view is centred, so a later reread that grows the page above it
    // still leaves it on screen
    const { top, bottom } = node.getBoundingClientRect();
    if (top < 0 || bottom > window.innerHeight) {
      // the window is scrolled directly: scrollIntoView can stop at a page wrapper that clips overflow and scroll only it
      window.scrollBy({ top: (top + bottom - window.innerHeight) / 2, behavior: 'instant' });
    }
  }, [shownError, notice, area, tool, instance]);

  // the first read, and a retry of a read that never succeeded, show the spinner
  if (toolQuery.isPending || (read === undefined && toolQuery.isFetching))
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
        error={readError}
        failed={readFailed}
        onRetry={() => void toolQuery.refetch()}
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
                      // the versions deleted before this refusal are gone, so say so; the action reads the list again
                      setError(partialDeleteMessage(refusal, deleted));
                      return false;
                    }
                  }
                  // a refused delete leaves the tool in place, and the action reads it again
                  if (await deleteEntity(tool.id, setError)) void navigate('/tools');
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
            setChosen(instanceId);
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
            onChange={(event) => setPublishDraft(event.target.value)}
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
            onChange={(event) => setDescriptionDraft(event.target.value)}
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
