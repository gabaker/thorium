import { FC, Fragment, useState } from 'react';
import { Link, useParams } from 'react-router-dom';
import { useMutation, useQueryClient } from '@tanstack/react-query';

// project imports
import { EntityNotFound } from '@components/agentic/EntityNotFound';
import { ConfirmButton } from '@components/agentic/ConfirmButton';
import { RunStagesSection } from '@components/agentic/runs/RunStages';
import { RunSubRunsPanel } from '@components/agentic/runs/RunSubRunsPanel';
import { RunSummaryPanel } from '@components/agentic/runs/RunSummaryPanel';
import { RunUpdatesPanel } from '@components/agentic/runs/RunUpdatesPanel';
import { RunStagesView } from '@components/agentic/runs/stages';
import { StatusPill } from '@components/agentic/runs/StatusIndicator';
import { Actions, BannerSlot, EntityLink, InlineList, Mono, Muted, PageHead, PageTitle, Pre } from '@components/agentic/shared';
import Page from '@components/pages/Page';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { deleteReaction } from '@thorpi/reactions';
import { invalidateRunLists, reactionKey, useReactionQuery, useReactionUpdates, useStageOrder } from '@utilities/queries/runs';
import { ReactionReadView, reactionReadView, runKind } from '@utilities/runs';
import { RunKind } from '@models/runs';
import { Reaction } from '@models/reactions';

/** How often the reaction is read while it works, in milliseconds. */
const POLL_MS = 5000;

/** Props for {@link ReactionSections}. */
interface ReactionSectionsProps {
  /** The reaction */
  reaction: Reaction;
}

/**
 * The sections of the single reaction status page: the shared run summary with the fields only this page
 * shows (args, generators, ephemeral files), the stage diagram with each stage's jobs and logs, sub
 * reactions, and status updates.
 */
export const ReactionSections: FC<ReactionSectionsProps> = ({ reaction }) => {
  const order = useStageOrder(reaction);
  const updates = useReactionUpdates(reaction, POLL_MS);
  const generators = reaction.generators ?? [];
  const ephemeral = reaction.ephemeral ?? [];
  const parentEphemeral = Object.entries(reaction.parent_ephemeral ?? {});
  return (
    <>
      <RunSummaryPanel run={reaction} steps={order.length}>
        <dt>Args</dt>
        <dd>
          {Object.keys(reaction.args ?? {}).length === 0 ? (
            <Muted>none</Muted>
          ) : (
            <Pre data-testid="reaction-args">{JSON.stringify(reaction.args, null, 2)}</Pre>
          )}
        </dd>
        {generators.length > 0 && (
          <>
            <dt>Generators</dt>
            <dd data-testid="reaction-generators">
              <InlineList>
                {generators.map((job) => (
                  <Mono key={job}>{job}</Mono>
                ))}
              </InlineList>
            </dd>
          </>
        )}
        {ephemeral.length > 0 && (
          <>
            <dt>Ephemeral</dt>
            <dd data-testid="reaction-ephemeral">{ephemeral.join(', ')}</dd>
          </>
        )}
        {parentEphemeral.length > 0 && (
          <>
            <dt>Parent ephemeral</dt>
            <dd data-testid="reaction-parent-ephemeral">
              {parentEphemeral.map(([file, parent]) => (
                <div key={file}>
                  {file} <Muted>from</Muted> <EntityLink to={`/reactions/${reaction.group}/${parent}`}>{parent}</EntityLink>
                </div>
              ))}
            </dd>
          </>
        )}
      </RunSummaryPanel>
      <RunStagesSection run={reaction} order={order} updates={updates.data} view={RunStagesView.Diagram} />
      {reaction.sub_reactions > 0 && <RunSubRunsPanel run={reaction} />}
      <RunUpdatesPanel updates={updates.data} error={updates.error} />
    </>
  );
};

/** The single reaction status page: one reaction's status, stages, jobs, logs, and status updates. */
const ReactionStatus: FC = () => {
  const { group = '', reactionID = '' } = useParams();
  const queryClient = useQueryClient();
  // the samples of a deleted reaction, kept for the links back to them; `null` until it is deleted
  const [deletedSamples, setDeletedSamples] = useState<string[] | null>(null);
  const [deletionError, setDeletionError] = useState('');
  const deleted = deletedSamples !== null;
  // a deleted reaction is not read again
  const { run: reaction, error } = useReactionQuery(group, reactionID, POLL_MS, !deleted);
  const remove = useMutation({
    mutationFn: async (current: Reaction) => {
      setDeletionError('');
      const done = await deleteReaction(current.group, current.id, setDeletionError);
      return done ? current.samples : null;
    },
    onSuccess: (samples) => {
      if (samples === null) return;
      setDeletedSamples(samples);
      // nothing about the reaction is left to read
      queryClient.removeQueries({ queryKey: reactionKey(group, reactionID) });
      void queryClient.invalidateQueries({ queryKey: ['reaction-sub-runs'] });
    },
    // the cached run lists may still hold the reaction, even after a refused delete that removed part of it
    onSettled: () => invalidateRunLists(queryClient),
  });

  // a reaction that was never read (it doesn't exist or the user can't see it) shows only a titled message with a way back
  const readView = reactionReadView(Boolean(reaction), error);
  if (readView === ReactionReadView.NotFound && !deleted) {
    return <EntityNotFound what="Reaction" loading={false} listPath="/runs" listName="runs" error={error} />;
  }
  if (deleted) {
    return (
      <Page title="Reaction · Thorium">
        <AlertBanner severity={Severity.Success}>
          <span data-testid="reaction-deleted">
            Reaction deleted successfully.
            {/* a reaction that ran on no samples has none to return to */}
            {deletedSamples.length > 0 && ` Return to sample${deletedSamples.length > 1 ? 's' : ''} `}
            {deletedSamples.map((sample, index) => (
              <Fragment key={sample}>
                {index > 0 && ', '}
                <Link to={`/file/${sample}`}>{sample}</Link>
              </Fragment>
            ))}
          </span>
        </AlertBanner>
      </Page>
    );
  }
  return (
    <Page title="Reaction · Thorium">
      <PageHead>
        <InlineList>
          <PageTitle>Reaction {reaction ? `· ${reaction.pipeline}` : ''}</PageTitle>
          {reaction && <StatusPill status={reaction.status} testId="reaction-head-status" />}
        </InlineList>
        <Actions>
          {reaction && (
            <ConfirmButton
              testId="reaction-delete"
              confirmText="Delete it"
              title="Delete this reaction. Only system admins, group owners and managers, and the submitter can delete a reaction."
              disabled={remove.isPending}
              onConfirm={() => remove.mutate(reaction)}
            >
              Delete reaction
            </ConfirmButton>
          )}
        </Actions>
      </PageHead>
      {reaction && (
        <>
          <BannerSlot>
            {deletionError !== '' && (
              <AlertBanner severity={Severity.Error}>
                <span data-testid="reaction-delete-error">{deletionError}</span>
              </AlertBanner>
            )}
            {readView === ReactionReadView.StaleDetails && (
              // the reaction loaded earlier, so keep showing it and say the latest refresh failed
              <AlertBanner severity={Severity.Warning}>
                <span data-testid="reaction-refresh-error">Could not refresh this reaction, showing the last details read. {error}</span>
              </AlertBanner>
            )}
            <AlertBanner severity={Severity.Info}>
              <span data-testid="run-banner">
                {runKind(reaction) === RunKind.Pipeline
                  ? 'This reaction is a pipeline run. '
                  : `This reaction is a ${runKind(reaction).toLowerCase()} run. `}
                <Link to={`/runs/${reaction.group}/${reaction.id}`}>Open it in the run view</Link> for its results, source, and versions.
              </span>
            </AlertBanner>
          </BannerSlot>
          <ReactionSections reaction={reaction} />
        </>
      )}
      {!reaction && <Muted>Loading reaction...</Muted>}
    </Page>
  );
};

export default ReactionStatus;
