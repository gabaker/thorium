import { FC, useCallback, useEffect, useReducer, useState } from 'react';
import { useSearchParams } from 'react-router-dom';

// project imports
import { InputsForm } from '@components/agentic/runs/InputsForm';
import { inputsBlocker, InputsState, inputsSummary } from '@components/agentic/runs/inputs';
import { LaunchError, LaunchSection, RunRoleHint } from '@components/agentic/runs/LaunchNotices';
import { LaunchResults, LaunchSubmit } from '@components/agentic/runs/LaunchSubmit';
import { LaunchTab, launchTabFromParam } from '@components/agentic/runs/launch';
import {
  countByKind,
  describeItem,
  initialLaunchState,
  LaunchActionType,
  LaunchCatalog,
  LaunchItem,
  launchBlocker,
  launchLabel,
  launchReducer,
  launchRequirement,
  launchSearchParams,
  LAUNCH_TAB_LABELS,
} from '@components/agentic/runs/launchForms';
import { PipelineLaunch } from '@components/agentic/runs/PipelineLaunch';
import { SlaField, TagsField } from '@components/agentic/runs/RunSettings';
import { RunsToLaunch } from '@components/agentic/runs/RunsToLaunch';
import { ToolLaunch } from '@components/agentic/runs/ToolLaunch';
import { useLaunchRuns } from '@components/agentic/runs/useLaunchRuns';
import { useRepoUrls } from '@components/agentic/runs/useLaunchSupport';
import { WorkflowLaunch } from '@components/agentic/runs/WorkflowLaunch';
import { useGroupNames } from '@components/agentic/hooks';
import { PageHead, PageTitle } from '@components/agentic/shared';
import Page from '@components/pages/Page';
import { Tabs } from '@components/shared/tabs';
import { useAgenticEntities, useAgenticListQuery } from '@utilities/queries/agentic';
import { usePipelineListQuery } from '@utilities/queries/pipelines';
import { useSampleChecks } from '@utilities/queries/samples';
import { AgenticKinds } from '@models/agentic';
import { Tool } from '@models/tools';
import { Workflow, WorkflowInstance } from '@models/workflows';

/** No search text on any tab's picker. */
const EMPTY_SEARCHES: Record<LaunchTab, string> = { [LaunchTab.Workflow]: '', [LaunchTab.Tool]: '', [LaunchTab.Pipeline]: '' };

/**
 * Launch several runs at once: workflows, tools, and pipelines chosen on their tabs, all on the same prompt, files,
 * samples, or repo.
 *
 * Every chosen run is listed under "Runs to launch" with its own options, and only the listed runs are started. The
 * chosen runs, tool versions, and inputs are kept in the URL so the page can be shared and reloaded.
 */
const LaunchRun: FC = () => {
  const [params, setParams] = useSearchParams();
  const [state, dispatch] = useReducer(launchReducer, params, initialLaunchState);
  // without a tab in the URL, a link that names runs opens the tab of the first one
  const [defaultTab] = useState(() => state.items[0]?.kind ?? LaunchTab.Workflow);
  const tab = launchTabFromParam(params.get('tab'), defaultTab);
  const userGroups = useGroupNames();
  const counts = countByKind(state.items);
  // a list is read once its tab is shown or a chosen run needs it
  const wants = (kind: LaunchTab) => tab === kind || counts[kind] > 0;
  const workflows = useAgenticListQuery<Workflow>(AgenticKinds.WorkflowCatalog, wants(LaunchTab.Workflow));
  const tools = useAgenticListQuery<Tool>(AgenticKinds.Tool, wants(LaunchTab.Tool));
  const pipelines = usePipelineListQuery(userGroups, wants(LaunchTab.Pipeline));
  const chosenWorkflows = state.items.filter((item) => item.kind === LaunchTab.Workflow);
  const instances = useAgenticEntities<WorkflowInstance>(
    chosenWorkflows.map((item) => workflows.items.find((entry) => entry.id === item.ref)?.metadata.WorkflowCatalog.latest_instance ?? ''),
  );
  const checks = useSampleChecks(state.inputs.samples);
  const repoUrls = useRepoUrls();
  const { launch, busy, error, outcomes } = useLaunchRuns();
  const catalog: LaunchCatalog = {
    workflows: workflows.items,
    tools: tools.items,
    pipelines: pipelines.pipelines,
    instances,
    loading: [
      ...(workflows.loading ? [LaunchTab.Workflow] : []),
      ...(tools.loading ? [LaunchTab.Tool] : []),
      ...(pipelines.loading ? [LaunchTab.Pipeline] : []),
    ],
  };
  const views = state.items.map((item) => describeItem(item, catalog, userGroups));
  const requirement = launchRequirement(state.items);
  const blocker = launchBlocker(views, state.shared, inputsBlocker(state.inputs, checks, requirement));
  const chooseTab = (next: LaunchTab) => {
    setParams(
      (current) => {
        const updated = new URLSearchParams(current);
        updated.set('tab', next);
        return updated;
      },
      { replace: true },
    );
  };
  // keep the shareable choices in the URL, replacing the history entry so typing doesn't fill the back button
  useEffect(() => {
    setParams((current) => launchSearchParams(current, state), { replace: true });
  }, [state]);
  // the tool versions a link names wait for the tool list, which says which tool each belongs to
  const toolsReady = wants(LaunchTab.Tool) && !tools.loading;
  useEffect(() => {
    if (!toolsReady) return;
    const known = tools.items.map((tool) => ({
      id: tool.id,
      latest: tool.metadata.Tool.latest ?? null,
      instances: (tool.metadata.Tool.instances ?? []).map((instance) => instance.id),
    }));
    dispatch({ type: LaunchActionType.ToolVersions, tools: known });
  }, [toolsReady, tools.items]);
  const onInputsChange = useCallback((inputs: InputsState) => dispatch({ type: LaunchActionType.Inputs, inputs }), []);
  const onItemChange = useCallback(
    (key: string, patch: Partial<Omit<LaunchItem, 'kind' | 'ref'>>) => dispatch({ type: LaunchActionType.Item, key, patch }),
    [],
  );
  const onRemove = useCallback((key: string) => dispatch({ type: LaunchActionType.Remove, key }), []);
  // each tab's picker search, kept here because only the shown tab's picker is mounted
  const [searches, setSearches] = useState<Record<LaunchTab, string>>(EMPTY_SEARCHES);
  const searchOf = (kind: LaunchTab) => ({
    search: searches[kind],
    onSearch: (search: string) => setSearches((current) => ({ ...current, [kind]: search })),
  });
  const refsOf = (kind: LaunchTab) => state.items.filter((item) => item.kind === kind).map((item) => item.ref);
  const select = (kind: LaunchTab) => (refs: string[]) => {
    // a newly chosen tool starts on its latest version
    const latest = Object.fromEntries(tools.items.map((tool) => [tool.id, tool.metadata.Tool.latest ?? '']));
    dispatch({ type: LaunchActionType.Select, kind, refs, latest });
  };
  const submit = async () => {
    if (blocker !== null) return;
    const result = await launch({ views, shared: state.shared, inputs: state.inputs });
    // the runs that started leave the list, so a second launch can't start them again; failed runs stay to retry
    result?.outcomes.filter((outcome) => outcome.error === '').forEach((outcome) => onRemove(outcome.key));
  };
  return (
    <Page title="Launch · Thorium">
      <PageHead>
        <PageTitle>Launch runs</PageTitle>
      </PageHead>
      <RunRoleHint />
      <Tabs
        active={tab}
        onChange={chooseTab}
        aria-label="Run kind"
        tabs={Object.values(LaunchTab).map((key) => ({
          key,
          label: LAUNCH_TAB_LABELS[key],
          count: counts[key] > 0 ? counts[key] : undefined,
          testId: `launch-tab-${key}`,
        }))}
      />
      {/* only the shown tab's picker is mounted; the chosen runs and searches live in the page state, so switching tabs keeps them */}
      {tab === LaunchTab.Workflow && (
        <WorkflowLaunch
          workflows={workflows}
          selected={refsOf(LaunchTab.Workflow)}
          onSelect={select(LaunchTab.Workflow)}
          {...searchOf(LaunchTab.Workflow)}
        />
      )}
      {tab === LaunchTab.Tool && (
        <ToolLaunch tools={tools} selected={refsOf(LaunchTab.Tool)} onSelect={select(LaunchTab.Tool)} {...searchOf(LaunchTab.Tool)} />
      )}
      {tab === LaunchTab.Pipeline && (
        <PipelineLaunch
          pipelines={pipelines}
          selected={refsOf(LaunchTab.Pipeline)}
          onSelect={select(LaunchTab.Pipeline)}
          {...searchOf(LaunchTab.Pipeline)}
        />
      )}
      <RunsToLaunch
        views={views}
        tools={tools.items}
        instances={instances}
        inputsSummary={inputsSummary(state.inputs)}
        onChange={onItemChange}
        onRemove={onRemove}
        onWorkflowsChanged={workflows.reload}
      />
      <LaunchSection>
        <h2>Inputs and shared settings</h2>
        <InputsForm
          value={state.inputs}
          onChange={onInputsChange}
          repoUrls={repoUrls}
          checks={checks}
          requirement={requirement}
          promptOptional={state.items.length > 0 && state.items.every((item) => item.kind === LaunchTab.Pipeline)}
        />
        <TagsField
          label="Tags"
          hint="Added to every run."
          testId="run-tags"
          value={state.shared.tags}
          onChange={(tags) => dispatch({ type: LaunchActionType.Shared, patch: { tags } })}
        />
        <SlaField
          label="SLA (seconds)"
          hint="Used by every run that sets none of its own."
          testId="run-sla"
          value={state.shared.sla}
          onChange={(sla) => dispatch({ type: LaunchActionType.Shared, patch: { sla } })}
        />
        <LaunchError error={error} />
        <LaunchSubmit label={launchLabel(state.items.length)} busy={busy} blocker={blocker} onSubmit={() => void submit()} />
        <LaunchResults outcomes={outcomes} />
      </LaunchSection>
    </Page>
  );
};

export default LaunchRun;
