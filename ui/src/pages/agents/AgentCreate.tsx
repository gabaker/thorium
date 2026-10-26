import { FC, useState } from 'react';
import { useNavigate } from 'react-router-dom';

// project imports
import { Actions, BannerSlot, CodeSpans, Field, PageHead, PageTitle, Section, TextArea } from '@components/agentic/shared';
import { useAgenticList, useEditableGroupNames } from '@components/agentic/hooks';
import { EntityChecks, EntitySelect, GroupChecks, VersionPins } from '@components/agentic/pickers';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Button } from '@components/shared/buttons';
import { ButtonVariant } from '@components/shared/buttons/types';
import TextInput from '@components/shared/inputs/TextInput';
import Page from '@components/pages/Page';
import { createAgenticEntity } from '@thorpi/agentic';
import { buildAgentCreateForm, positiveIntError } from '@utilities/agentic/forms';
import { keepCovering } from '@utilities/agentic/groups';
import { applyPins, toolPinChoice, toolSetPinChoice } from '@utilities/agentic/pins';
import { splitEntries } from '@utilities/runs';
import { Agent } from '@models/agents';
import { AgenticKinds } from '@models/agentic';
import { Tool, ToolKind } from '@models/tools';
import { ToolSet, ToolSetInstance } from '@models/toolsets';

/** Create an agent: prompts, models, harness, and the tools, toolsets, and sub-agents it links. */
const AgentCreate: FC = () => {
  const navigate = useNavigate();
  // only groups the user may create entities in are offered
  const groups = useEditableGroupNames();
  const tools = useAgenticList<Tool>(AgenticKinds.Tool);
  const sets = useAgenticList<ToolSet>(AgenticKinds.ToolSet);
  const setInstances = useAgenticList<ToolSetInstance>(AgenticKinds.ToolSetInstance);
  const agents = useAgenticList<Agent>(AgenticKinds.Agent);
  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [picked, setPicked] = useState<string[]>([]);
  const [systemPrompt, setSystemPrompt] = useState('');
  const [prompt, setPrompt] = useState('');
  const [models, setModels] = useState('');
  const [maxTurns, setMaxTurns] = useState('');
  const [functionName, setFunctionName] = useState('');
  const [modelDescription, setModelDescription] = useState('');
  const [harness, setHarness] = useState('');
  const [chosenTools, setChosenTools] = useState<string[]>([]);
  const [chosenSets, setChosenSets] = useState<string[]>([]);
  const [chosenAgents, setChosenAgents] = useState<string[]>([]);
  const [pins, setPins] = useState<Record<string, string>>({});
  const [error, setError] = useState('');
  const harnesses = tools.items.filter((tool) => tool.metadata.Tool.kind === ToolKind.Harness);
  const callable = tools.items.filter((tool) => tool.metadata.Tool.kind === ToolKind.Agentic);
  // a linked entity must be in every group of the agent, so a selection that lost a group is dropped
  const pickedHarness = keepCovering(harnesses, harness ? [harness] : [], picked)[0] ?? '';
  const pickedTools = keepCovering(callable, chosenTools, picked);
  const pickedSets = keepCovering(sets.items, chosenSets, picked);
  const pickedAgents = keepCovering(agents.items, chosenAgents, picked);
  const maxTurnsError = positiveIntError(maxTurns);
  const harnessChoices = harnesses.filter((tool) => tool.id === pickedHarness).map(toolPinChoice);
  const toolChoices = callable.filter((tool) => pickedTools.includes(tool.id)).map(toolPinChoice);
  const setChoices = sets.items
    .filter((set) => pickedSets.includes(set.id))
    .map((set) => toolSetPinChoice(set.id, set.name, setInstances.items));
  const submit = async () => {
    setError('');
    const created = await createAgenticEntity(
      buildAgentCreateForm({
        name,
        groups: picked,
        description,
        systemPrompt,
        prompt,
        models: splitEntries(models),
        maxTurns,
        functionName,
        modelDescription,
        // a pinned link is sent as the instance it is pinned to
        harness: applyPins(pickedHarness ? [pickedHarness] : [], pins)[0] ?? '',
        tools: applyPins(pickedTools, pins),
        toolSets: applyPins(pickedSets, pins),
        subAgents: pickedAgents,
      }),
      setError,
    );
    if (created) void navigate(`/agents/${created.id}`);
  };
  return (
    <Page title="Create Agent · Thorium">
      <PageHead>
        <PageTitle>Create agent</PageTitle>
      </PageHead>
      <BannerSlot>
        {error && (
          <AlertBanner severity={Severity.Error}>
            <CodeSpans text={error} />
          </AlertBanner>
        )}
      </BannerSlot>
      <Section>
        <Field>
          <span className="label">Name</span>
          <TextInput data-testid="agent-name" value={name} onChange={(event) => setName(event.target.value)} />
        </Field>
        <Field>
          <span className="label">Description</span>
          <TextInput data-testid="agent-description" value={description} onChange={(event) => setDescription(event.target.value)} />
        </Field>
        <GroupChecks groups={groups} selected={picked} onChange={setPicked} />
        <Field>
          <span className="label">System prompt</span>
          <TextArea data-testid="agent-system-prompt" value={systemPrompt} onChange={(event) => setSystemPrompt(event.target.value)} />
        </Field>
        <Field>
          <span className="label">Task prompt</span>
          <span className="hint">A run adds its input files and repos to this prompt</span>
          <TextArea data-testid="agent-prompt" value={prompt} onChange={(event) => setPrompt(event.target.value)} />
        </Field>
        <Field>
          <span className="label">Models</span>
          <span className="hint">Model ids, highest priority first, one per line; empty uses the workflow default</span>
          <TextArea data-testid="agent-models" value={models} onChange={(event) => setModels(event.target.value)} />
        </Field>
        <Field>
          <span className="label">Max turns</span>
          <TextInput
            data-testid="agent-max-turns"
            $invalid={maxTurnsError !== ''}
            value={maxTurns}
            onChange={(event) => setMaxTurns(event.target.value)}
          />
          {maxTurnsError && (
            <span className="error" data-testid="agent-max-turns-error">
              {maxTurnsError}
            </span>
          )}
        </Field>
        <Field>
          <span className="label">Function name</span>
          <span className="hint">Needed when another agent delegates to this one: the function its model calls</span>
          <TextInput data-testid="agent-function-name" value={functionName} onChange={(event) => setFunctionName(event.target.value)} />
        </Field>
        <Field>
          <span className="label">Model description</span>
          <span className="hint">Needed when another agent delegates to this one: what its model is told this agent does</span>
          <TextArea
            data-testid="agent-model-description"
            value={modelDescription}
            onChange={(event) => setModelDescription(event.target.value)}
          />
        </Field>
        <EntitySelect
          label="Harness"
          testId="agent-harness"
          entities={harnesses}
          groups={picked}
          value={pickedHarness}
          onChange={setHarness}
          emptyText="No harness (sub-agents only)"
        />
        <VersionPins label="Harness version" testId="harness-pins" choices={harnessChoices} pins={pins} onChange={setPins} />
        <EntityChecks
          label="Tools"
          hint="Followed at their latest version unless pinned below; only tools in every chosen group can be picked"
          testId="agent-tools"
          entities={callable}
          groups={picked}
          selected={pickedTools}
          onChange={setChosenTools}
        />
        <VersionPins label="Tool versions" testId="tool-pins" choices={toolChoices} pins={pins} onChange={setPins} />
        <EntityChecks
          label="Toolsets"
          testId="agent-toolsets"
          entities={sets.items}
          groups={picked}
          selected={pickedSets}
          onChange={setChosenSets}
        />
        <VersionPins label="Toolset instances" testId="toolset-pins" choices={setChoices} pins={pins} onChange={setPins} />
        <EntityChecks
          label="Sub-agents"
          testId="agent-subagents"
          entities={agents.items}
          groups={picked}
          selected={pickedAgents}
          onChange={setChosenAgents}
        />
        <Actions>
          <Button
            variant={ButtonVariant.Ok}
            data-testid="submit-agent"
            disabled={name.trim() === '' || picked.length === 0 || maxTurnsError !== ''}
            onClick={() => void submit()}
          >
            Create
          </Button>
        </Actions>
      </Section>
    </Page>
  );
};

export default AgentCreate;
