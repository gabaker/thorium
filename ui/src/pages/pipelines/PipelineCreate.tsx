import { FC, useEffect, useMemo, useState } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';
import styled from 'styled-components';

// project imports
import { useAgenticList, usePipelineCreateBlockReason, usePipelineGroupNames } from '@components/agentic/hooks';
import { ActionHint } from '@components/agentic/tools/ActionHint';
import { StageEditor } from '@components/agentic/pipelines/StageEditor';
import {
  BLANK_STAGE,
  buildPipelineRequest,
  retainStageTools,
  pinStageInstances,
  listedGroup,
  pipelineProblem,
  stepsFromPipeline,
  StageStep,
} from '@components/agentic/pipelines/stages';
import { Actions, BannerSlot, Field, PageHead, PageTitle, Section, Select } from '@components/agentic/shared';
import Page from '@components/pages/Page';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Button, ButtonVariant } from '@components/shared/buttons';
import TextInput from '@components/shared/inputs/TextInput';
import { createPipeline } from '@thorpi/pipelines';
import { AgenticKinds } from '@models/agentic';
import { Pipeline } from '@models/pipelines';
import { Tool } from '@models/tools';

/** The submit error below the stage editor, held clear of the editor's buttons above it. */
const FormNote = styled.div`
  margin-top: 0.75rem;
`;

/** The submit and cancel buttons below the stage editor. */
const FormActions = styled(Actions)`
  margin-top: 0.75rem;
`;

/** Create a pipeline whose every stage is backed by a tool (following latest) or a pinned tool version. */
const PipelineCreate: FC = () => {
  const navigate = useNavigate();
  const { state } = useLocation() as { state: Pipeline | null };
  // only groups the user may create a pipeline in are offered, and a user who can create in none is told why
  const myGroups = usePipelineGroupNames();
  const blockReason = usePipelineCreateBlockReason();
  const { items: tools, error: toolsError } = useAgenticList<Tool>(AgenticKinds.Tool);
  const [name, setName] = useState(state?.name ?? '');
  const [chosenGroup, setGroup] = useState(state?.group ?? '');
  // a copied pipeline's group the user can't create in is not a choice, so it counts as unchosen
  const group = listedGroup(chosenGroup, myGroups);
  const [description, setDescription] = useState(state?.description ?? '');
  const [sla, setSla] = useState(state ? String(state.sla) : '');
  // a copy starts from the source pipeline's stages, each bound to the id its stage tool names
  const [steps, setSteps] = useState<StageStep[]>(() => (state ? stepsFromPipeline(state, {}) : [[{ ...BLANK_STAGE }]]));
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const triggers = state?.triggers ?? {};
  const triggerNames = Object.keys(triggers);
  const values = { name, group, description, sla, steps, triggers };
  const problem = useMemo(() => pipelineProblem(values), [name, group, description, sla, steps]);

  // once the tools are known, a stage bound to a tool version becomes a pinned stage of its tool; other edits are kept
  useEffect(() => {
    const instanceTool: Record<string, string> = {};
    for (const tool of tools) {
      for (const instance of tool.metadata.Tool.instances ?? []) instanceTool[instance.id] = tool.id;
    }
    setSteps((current) => pinStageInstances(current, instanceTool));
  }, [tools]);

  /** Send the create request and return to the pipeline list. */
  const submit = async () => {
    if (problem !== '' || blockReason !== '') return;
    setBusy(true);
    setError('');
    const created = await createPipeline(buildPipelineRequest(values), setError);
    setBusy(false);
    if (created) void navigate('/pipelines');
  };

  return (
    <Page title="Create Pipeline · Thorium">
      <PageHead>
        <PageTitle>Create pipeline</PageTitle>
      </PageHead>
      <BannerSlot>
        <AlertBanner severity={Severity.Info}>
          Every stage of a pipeline is backed by a tool: choose a tool to follow its latest version, or pin one version. Triggers are not
          edited here.
          {triggerNames.length > 0 && (
            <span data-testid="copied-triggers"> The copy keeps the source pipeline&apos;s triggers: {triggerNames.join(', ')}.</span>
          )}
        </AlertBanner>
        {blockReason !== '' && (
          <AlertBanner severity={Severity.Warning}>
            <span data-testid="create-blocked">{blockReason}</span>
          </AlertBanner>
        )}
        {toolsError !== '' && (
          <AlertBanner>
            <span data-testid="tools-error">{toolsError}</span>
          </AlertBanner>
        )}
      </BannerSlot>
      <Section>
        <Field>
          <span className="label">Name</span>
          <TextInput data-testid="pipeline-name" value={name} onChange={(event) => setName(event.target.value)} />
        </Field>
        <Field>
          <span className="label">Group</span>
          <Select
            data-testid="pipeline-group"
            value={group}
            onChange={(event) => {
              setGroup(event.target.value);
              // tools belong to groups, so a group change unbinds only the stages whose tool is not in the new group
              setSteps((current) => retainStageTools(current, tools, event.target.value));
            }}
          >
            <option value="">Choose a group</option>
            {myGroups.map((option) => (
              <option key={option} value={option}>
                {option}
              </option>
            ))}
          </Select>
        </Field>
        <Field>
          <span className="label">Description</span>
          <TextInput data-testid="pipeline-description" value={description} onChange={(event) => setDescription(event.target.value)} />
        </Field>
        <Field>
          <span className="label">SLA (seconds)</span>
          <span className="hint">Leave blank for one week.</span>
          <TextInput data-testid="pipeline-sla" value={sla} onChange={(event) => setSla(event.target.value)} />
        </Field>
      </Section>
      <StageEditor tools={tools} group={group} steps={steps} onChange={setSteps} />
      {error !== '' && (
        // keep the submit error clear of the stage editor's buttons above it
        <FormNote data-testid="create-error">
          <AlertBanner>{error}</AlertBanner>
        </FormNote>
      )}
      <FormActions>
        <Button
          variant={ButtonVariant.Ok}
          data-testid="create-submit"
          disabled={busy || problem !== '' || blockReason !== ''}
          onClick={() => void submit()}
        >
          Create
        </Button>
        <Button variant={ButtonVariant.Secondary} data-testid="create-cancel" onClick={() => void navigate(-1)}>
          Cancel
        </Button>
        {problem !== '' && <ActionHint data-testid="pipeline-problem">{problem}</ActionHint>}
      </FormActions>
    </Page>
  );
};

export default PipelineCreate;
