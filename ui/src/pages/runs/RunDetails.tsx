import { FC, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import styled from 'styled-components';
import { useMutation, useQueryClient } from '@tanstack/react-query';

// project imports
import { ConfirmButton } from '@components/agentic/ConfirmButton';
import { EntityNotFound } from '@components/agentic/EntityNotFound';
import { useSampleNames } from '@components/agentic/hooks';
import { LaunchError } from '@components/agentic/runs/LaunchNotices';
import { RunPlanPanel } from '@components/agentic/runs/RunPlanPanel';
import { RunResultsPanel } from '@components/agentic/runs/RunResultsPanel';
import { RunProvenancePanel, RunSourcePanel } from '@components/agentic/runs/RunSourcePanel';
import { RunStagesSection } from '@components/agentic/runs/RunStages';
import { RunSubRunsPanel } from '@components/agentic/runs/RunSubRunsPanel';
import { RunSummaryPanel } from '@components/agentic/runs/RunSummaryPanel';
import { RunUpdatesPanel } from '@components/agentic/runs/RunUpdatesPanel';
import { RunStagesView, runStagesView } from '@components/agentic/runs/stages';
import { StatusPill } from '@components/agentic/runs/StatusIndicator';
import { Actions, BannerSlot, InlineList, Muted, PageHead, PageTitle } from '@components/agentic/shared';
import { ActionHint } from '@components/agentic/tools/ActionHint';
import Page from '@components/pages/Page';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Button, ButtonVariant } from '@components/shared/buttons';
import { cancelRun, retryWorkflowRun } from '@thorpi/runs';
import { useAuth } from '@utilities/auth';
import { isDeveloperAny } from '@utilities/permissions';
import { invalidateRunLists, reactionKey, useReactionQuery, useReactionUpdates, useStageOrder } from '@utilities/queries/runs';
import { isRunFinished } from '@utilities/runs';
import { Reaction, ReactionStatus } from '@models/reactions';

/** How often the run is read while it works, in milliseconds. */
const POLL_MS = 2000;

/** The page's banner stack; the banners drop their own bottom margin so the stack's gap and the gap below it are the only spacing. */
const RunBanners = styled(BannerSlot)`
  && > * {
    margin-bottom: 0;
  }
`;

/** Props for {@link RunSections}. */
interface RunSectionsProps {
  /** The run */
  run: Reaction;
}

/**
 * The sections of one run: summary, stages (a diagram for a pipeline run, the logs alone for a run of
 * one tool, the plan otherwise), sub reactions, results, source, versions, and status updates.
 */
export const RunSections: FC<RunSectionsProps> = ({ run }) => {
  // the input files' names tell the prompt apart from an uploaded file
  const sampleNames = useSampleNames(run.samples ?? []);
  const order = useStageOrder(run);
  const updates = useReactionUpdates(run, POLL_MS);
  const view = runStagesView(run);
  return (
    <>
      <RunSummaryPanel run={run} names={sampleNames} steps={order.length} />
      {view === RunStagesView.Plan ? (
        <RunPlanPanel run={run} />
      ) : (
        <RunStagesSection run={run} order={order} updates={updates.data} view={view} />
      )}
      {run.sub_reactions > 0 && <RunSubRunsPanel run={run} />}
      <RunResultsPanel run={run} names={sampleNames} />
      <RunSourcePanel run={run} />
      <RunProvenancePanel run={run} />
      <RunUpdatesPanel updates={updates.data} error={updates.error} />
    </>
  );
};

/** Show one run: status, stages and logs, source, versions, inputs, and results. */
const RunDetails: FC = () => {
  const { group = '', id = '' } = useParams();
  const navigate = useNavigate();
  const { userInfo } = useAuth();
  const queryClient = useQueryClient();
  const { run, error, gone } = useReactionQuery(group, id, POLL_MS);
  const [actionError, setActionError] = useState('');

  const retry = useMutation({
    mutationFn: async (current: Reaction) => {
      if (!current.source || !('WorkflowRun' in current.source)) return null;
      setActionError('');
      return retryWorkflowRun(current.source.WorkflowRun.workflow_instance, current.id, setActionError);
    },
    onSuccess: (created) => {
      if (created) void navigate(`/runs/${run?.group ?? group}/${created.id}`);
    },
    // a retry starts a new run, so the cached run lists are stale
    onSettled: () => invalidateRunLists(queryClient),
  });

  const cancel = useMutation({
    mutationFn: async (current: Reaction) => {
      setActionError('');
      return cancelRun(current.group, current.id, setActionError);
    },
    // the run's state changed, so read it (and through it, its updates) again, and the run lists that show its status
    onSettled: () => Promise.all([queryClient.invalidateQueries({ queryKey: reactionKey(group, id) }), invalidateRunLists(queryClient)]),
  });

  const finished = run ? isRunFinished(run.status) : false;
  const failedWorkflow = run?.status === ReactionStatus.Failed && !!run.source && 'WorkflowRun' in run.source;
  // the API refuses a retry unless the user is an admin, an analyst, or a developer for the scaler of its tools
  const canRetry = !userInfo || isDeveloperAny(userInfo);

  // a run that can't be read, or isn't visible to this user, gets a titled message with a way back to the list
  if (!run && gone) {
    return <EntityNotFound what="Run" loading={false} listPath="/runs" listName="runs" error={error} />;
  }
  return (
    <Page title="Run · Thorium">
      <PageHead>
        <InlineList>
          <PageTitle>Run {run ? `· ${run.pipeline}` : ''}</PageTitle>
          {run && <StatusPill status={run.status} testId="run-head-status" />}
        </InlineList>
        <Actions>
          {failedWorkflow && !canRetry && (
            <ActionHint data-testid="run-retry-role-hint">Retrying a run takes a Developer, Analyst, or Admin role.</ActionHint>
          )}
          {failedWorkflow && (
            <Button
              variant={ButtonVariant.Warning}
              data-testid="run-retry"
              disabled={!canRetry || retry.isPending}
              title={canRetry ? undefined : "Your Thorium role can't start runs"}
              onClick={() => run && retry.mutate(run)}
            >
              {retry.isPending ? 'Retrying...' : 'Retry'}
            </Button>
          )}
          {/* keyed by the run, so a question left open on one run never carries over to the next; a run that finishes drops it */}
          {run && !finished && (
            <ConfirmButton
              key={`${group}/${id}`}
              testId="run-cancel"
              question="Fail this run?"
              confirmText="Yes, fail it"
              cancelText="Keep running"
              disabled={cancel.isPending}
              onConfirm={() => cancel.mutate(run)}
            >
              Cancel run
            </ConfirmButton>
          )}
        </Actions>
      </PageHead>
      <RunBanners>
        {/* a refused retry or cancel reads as a sentence, with the API's trace id kept as a hover hint */}
        <LaunchError error={actionError} testId="run-action-error" />
        {!run && error !== '' && <AlertBanner severity={Severity.Error}>{error}</AlertBanner>}
      </RunBanners>
      {!run && error === '' && <Muted>Loading run...</Muted>}
      {run && <RunSections run={run} />}
    </Page>
  );
};

export default RunDetails;
