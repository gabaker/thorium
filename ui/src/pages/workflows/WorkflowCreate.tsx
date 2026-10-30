import { FC, useState } from 'react';
import { useNavigate } from 'react-router-dom';

// project imports
import { Actions, BannerSlot, CheckRow, Field, PageHead, PageTitle, Section, TextArea } from '@components/agentic/shared';
import ListErrorBanner from '@components/shared/alerts/ListErrorBanner';
import { useAgenticList, useEditableGroupNames } from '@components/agentic/hooks';
import { EntityChecks, EntitySelect, GroupChecks } from '@components/agentic/pickers';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Button } from '@components/shared/buttons';
import { ButtonVariant } from '@components/shared/buttons/types';
import TextInput from '@components/shared/inputs/TextInput';
import Page from '@components/pages/Page';
import { createAgenticEntity } from '@thorpi/agentic';
import { buildWorkflowCreateForm, positiveIntError } from '@utilities/agentic/forms';
import { keepCovering } from '@utilities/agentic/groups';
import { splitEntries } from '@utilities/runs';
import { invalidateAgentic } from '@utilities/queries/agentic';
import { useChangeAndInvalidate } from '@utilities/queries/changes';
import { Agent } from '@models/agents';
import { AgenticKinds } from '@models/agentic';

/** Create a workflow from agents, an entry agent, default models, and limits. */
const WorkflowCreate: FC = () => {
  const navigate = useNavigate();
  // a change marks the cached lists and entities stale, so other pages show it at once
  const change = useChangeAndInvalidate(invalidateAgentic);
  // only groups the user may create entities in are offered
  const groups = useEditableGroupNames();
  const agents = useAgenticList<Agent>(AgenticKinds.Agent);
  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [picked, setPicked] = useState<string[]>([]);
  const [chosen, setChosen] = useState<string[]>([]);
  const [entry, setEntry] = useState('');
  const [models, setModels] = useState('');
  const [alive, setAlive] = useState('');
  const [auto, setAuto] = useState(true);
  const [error, setError] = useState('');
  // an agent must be in every group of the workflow, so a selection that lost a group is dropped
  const pickedAgents = keepCovering(agents.items, chosen, picked);
  // the entry agent must be one of the workflow's agents
  const entryChoices = agents.items.filter((agent) => pickedAgents.includes(agent.id));
  const pickedEntry = pickedAgents.includes(entry) ? entry : '';
  const aliveError = positiveIntError(alive);
  /** Set the workflow's agents, dropping the entry agent when it is no longer one of them. */
  const chooseAgents = (ids: string[]) => {
    setChosen(ids);
    // an unchecked entry agent would still be sent and refused by the API
    if (!ids.includes(entry)) setEntry('');
  };
  const submit = async () => {
    setError('');
    const created = await change(() =>
      createAgenticEntity(
        buildWorkflowCreateForm({
          name,
          groups: picked,
          description,
          agents: pickedAgents,
          entryAgent: pickedEntry,
          defaultModels: splitEntries(models),
          maxAliveSubAgents: alive,
          autoInstance: auto,
        }),
        setError,
      ),
    );
    if (created) void navigate(`/workflows/${created.id}`);
  };
  return (
    <Page title="Create Workflow · Thorium">
      <PageHead>
        <PageTitle>Create workflow</PageTitle>
      </PageHead>
      <BannerSlot>
        {error && <AlertBanner severity={Severity.Error}>{error}</AlertBanner>}
        {/* the pickers' lists report their errors so a missing choice is explained */}
        <ListErrorBanner errors={[agents.error]} className="" />
      </BannerSlot>
      <Section>
        <Field>
          <span className="label">Name</span>
          <TextInput data-testid="workflow-name" value={name} onChange={(event) => setName(event.target.value)} />
        </Field>
        <Field>
          <span className="label">Description</span>
          <TextInput data-testid="workflow-description" value={description} onChange={(event) => setDescription(event.target.value)} />
        </Field>
        <GroupChecks groups={groups} selected={picked} onChange={setPicked} />
        <EntityChecks
          label="Agents"
          hint="Only agents in every chosen group can be picked."
          testId="workflow-agents"
          entities={agents.items}
          groups={picked}
          selected={pickedAgents}
          onChange={chooseAgents}
        />
        <EntitySelect
          label="Entry agent"
          testId="workflow-entry"
          entities={entryChoices}
          value={pickedEntry}
          onChange={setEntry}
          emptyText="None (set one before running)"
        />
        <Field>
          <span className="label">Default models</span>
          <span className="hint">Model ids for agents without their own, one per line; leave empty for none.</span>
          <TextArea data-testid="workflow-models" value={models} onChange={(event) => setModels(event.target.value)} />
        </Field>
        <Field>
          <span className="label">Max alive sub-agents</span>
          <span className="hint">Leave blank for the harness default.</span>
          <TextInput
            data-testid="workflow-alive"
            $invalid={aliveError !== ''}
            value={alive}
            onChange={(event) => setAlive(event.target.value)}
          />
          {aliveError && (
            <span className="error" data-testid="workflow-alive-error">
              {aliveError}
            </span>
          )}
        </Field>
        <CheckRow>
          <input type="checkbox" data-testid="workflow-auto" checked={auto} onChange={(event) => setAuto(event.target.checked)} />
          Build new instances automatically
        </CheckRow>
        {/* keeps the button clear of the checkbox row above it */}
        <Actions style={{ marginTop: '0.75rem' }}>
          <Button
            variant={ButtonVariant.Ok}
            data-testid="submit-workflow"
            disabled={name.trim() === '' || picked.length === 0 || aliveError !== ''}
            onClick={() => void submit()}
          >
            Create
          </Button>
        </Actions>
      </Section>
    </Page>
  );
};

export default WorkflowCreate;
