import { useMutation, useQueryClient } from '@tanstack/react-query';
import { useCallback, useRef, useState } from 'react';

// project imports
import { InputsState } from './inputs';
import { LaunchTab } from './launch';
import { guardedLaunch, LaunchItemView, LaunchOutcome, LaunchResult, launchRuns, LaunchSubmission, SharedSettings } from './launchForms';
import { useSampleUploader } from './useLaunchSupport';
import { createReaction } from '@thorpi/reactions';
import { runTool, runWorkflow } from '@thorpi/runs';
import { invalidateRunLists } from '@utilities/queries/runs';

/**
 * Send one run request to the route of its kind.
 *
 * @param submission - The request and the run it starts.
 * @param onError - Called with the reason the request failed.
 * @returns The new run's id, or `null` when the request failed.
 */
export function sendRun(submission: LaunchSubmission, onError: (message: string) => void): Promise<{ id: string } | null> {
  if (submission.tab === LaunchTab.Workflow) return runWorkflow(submission.instanceId, submission.request, onError);
  if (submission.tab === LaunchTab.Tool) return runTool(submission.instanceId, submission.request, onError);
  return createReaction(submission.request, onError);
}

/** What a launch takes: the chosen runs, the shared settings, and the shared inputs. */
interface LaunchVariables {
  /** The chosen runs */
  views: LaunchItemView[];
  /** The shared tags and SLA */
  shared: SharedSettings;
  /** The shared inputs */
  inputs: InputsState;
}

/** The launch action of the launch page and what the last launch did. */
export interface LaunchRuns {
  /** Start every chosen run; resolves with the result, or `null` when a launch was already running */
  launch: (variables: LaunchVariables) => Promise<LaunchResult | null>;
  /** Whether a launch is running */
  busy: boolean;
  /** Why the last launch started nothing, or an empty string */
  error: string;
  /** What happened to each run of the last launch */
  outcomes: LaunchOutcome[];
}

/**
 * Launch the chosen runs as one mutation that sends a request per run, guarded so a double click starts one launch, and
 * mark the run and reaction lists stale afterwards.
 *
 * @returns The launch action, whether it is running, and the last launch's error and outcomes.
 */
export function useLaunchRuns(): LaunchRuns {
  const client = useQueryClient();
  const [error, setError] = useState('');
  const [outcomes, setOutcomes] = useState<LaunchOutcome[]>([]);
  // set for the whole of a launch, so a second click before the button disables can't start a second launch
  const submitting = useRef(false);
  const upload = useSampleUploader(setError);
  const mutation = useMutation({
    mutationFn: ({ views, shared, inputs }: LaunchVariables) => launchRuns(views, shared, inputs, upload, sendRun),
    onSettled: () => invalidateRunLists(client),
  });
  const { mutateAsync } = mutation;
  const launch = useCallback(
    (variables: LaunchVariables) =>
      guardedLaunch(submitting, async () => {
        setError('');
        setOutcomes([]);
        const result = await mutateAsync(variables);
        // an upload failure has already said why; the generic reason only fills in when nothing did
        if (result.error) setError((current) => current || result.error || '');
        setOutcomes(result.outcomes);
        return result;
      }),
    [mutateAsync],
  );
  return { launch, busy: mutation.isPending, error, outcomes };
}
