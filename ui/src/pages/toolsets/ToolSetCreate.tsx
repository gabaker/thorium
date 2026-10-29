import { FC, useState } from 'react';
import { useNavigate } from 'react-router-dom';

// project imports
import { Actions, BannerSlot, CheckRow, CodeSpans, Field, PageHead, PageTitle, Row, Section } from '@components/agentic/shared';
import ListErrorBanner from '@components/shared/alerts/ListErrorBanner';
import { useAgenticList, useEditableGroupNames } from '@components/agentic/hooks';
import { EntityChecks, GroupChecks, VersionPins } from '@components/agentic/pickers';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Button } from '@components/shared/buttons';
import { ButtonVariant } from '@components/shared/buttons/types';
import TextInput from '@components/shared/inputs/TextInput';
import Page from '@components/pages/Page';
import { createAgenticEntity } from '@thorpi/agentic';
import { buildToolSetCreateForm, selectorError } from '@utilities/agentic/forms';
import { keepCovering } from '@utilities/agentic/groups';
import { applyPins, toolPinChoice } from '@utilities/agentic/pins';
import { AgenticKinds } from '@models/agentic';
import { Tool, ToolKind } from '@models/tools';

/** Create a toolset from agentic tools, an optional tag selector, and the read only and auto instance flags. */
const ToolSetCreate: FC = () => {
  const navigate = useNavigate();
  // only groups the user may create entities in are offered
  const groups = useEditableGroupNames();
  const tools = useAgenticList<Tool>(AgenticKinds.Tool);
  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [picked, setPicked] = useState<string[]>([]);
  const [chosenTools, setChosenTools] = useState<string[]>([]);
  const [pins, setPins] = useState<Record<string, string>>({});
  const [selectorKey, setSelectorKey] = useState('');
  const [selectorValue, setSelectorValue] = useState('');
  const [readOnly, setReadOnly] = useState(false);
  const [autoInstance, setAutoInstance] = useState(true);
  const [error, setError] = useState('');
  // only agentic tools can be in a toolset
  const agentic = tools.items.filter((tool) => tool.metadata.Tool.kind === ToolKind.Agentic);
  // a tool must be in every group of the toolset, so a selection that lost a group is dropped
  const pickedTools = keepCovering(agentic, chosenTools, picked);
  const selectorProblem = selectorError(selectorKey, selectorValue);
  const submit = async () => {
    setError('');
    const created = await createAgenticEntity(
      buildToolSetCreateForm({
        name,
        groups: picked,
        description,
        // a pinned tool is sent as the tool instance it is pinned to
        tools: applyPins(pickedTools, pins),
        selectorKey,
        selectorValue,
        readOnly,
        autoInstance,
      }),
      setError,
    );
    if (created) void navigate(`/toolsets/${created.id}`);
  };
  return (
    <Page title="Create Toolset · Thorium">
      <PageHead>
        <PageTitle>Create toolset</PageTitle>
      </PageHead>
      <BannerSlot>
        {error && (
          <AlertBanner severity={Severity.Error}>
            <CodeSpans text={error} />
          </AlertBanner>
        )}
        {/* the pickers' lists report their errors so a missing choice is explained */}
        <ListErrorBanner errors={[tools.error]} className="" />
      </BannerSlot>
      <Section>
        <Field>
          <span className="label">Name</span>
          <TextInput data-testid="toolset-name" value={name} onChange={(event) => setName(event.target.value)} />
        </Field>
        <Field>
          <span className="label">Description</span>
          <TextInput data-testid="toolset-description" value={description} onChange={(event) => setDescription(event.target.value)} />
        </Field>
        <GroupChecks groups={groups} selected={picked} onChange={setPicked} />
        <EntityChecks
          label="Tools"
          hint="Agentic tools, followed at their latest version unless pinned below; only tools in every chosen group can be picked"
          testId="tools"
          entities={agentic}
          groups={picked}
          selected={pickedTools}
          onChange={setChosenTools}
        />
        <VersionPins
          label="Tool versions"
          testId="tool-pins"
          choices={agentic.filter((tool) => pickedTools.includes(tool.id)).map(toolPinChoice)}
          pins={pins}
          onChange={setPins}
        />
        <Row>
          <Field style={{ flex: '1 1 14rem' }}>
            <span className="label">Selector key</span>
            <TextInput
              data-testid="selector-key"
              $invalid={selectorProblem !== ''}
              value={selectorKey}
              onChange={(event) => setSelectorKey(event.target.value)}
            />
          </Field>
          <Field style={{ flex: '1 1 14rem' }}>
            <span className="label">Selector value</span>
            <TextInput
              data-testid="selector-value"
              $invalid={selectorProblem !== ''}
              value={selectorValue}
              onChange={(event) => setSelectorValue(event.target.value)}
            />
            {selectorProblem && (
              <span className="error" data-testid="selector-error">
                {selectorProblem}
              </span>
            )}
          </Field>
        </Row>
        <Field as="div">
          <span className="label">Options</span>
          <CheckRow>
            <input type="checkbox" data-testid="read-only" checked={readOnly} onChange={(event) => setReadOnly(event.target.checked)} />
            Read only (leave out tools that declare effects)
          </CheckRow>
          <CheckRow>
            <input
              type="checkbox"
              data-testid="auto-instance"
              checked={autoInstance}
              onChange={(event) => setAutoInstance(event.target.checked)}
            />
            Build new instances automatically
          </CheckRow>
        </Field>
        <Actions>
          <Button
            variant={ButtonVariant.Ok}
            data-testid="submit-toolset"
            disabled={name.trim() === '' || picked.length === 0 || selectorProblem !== ''}
            onClick={() => void submit()}
          >
            Create
          </Button>
        </Actions>
      </Section>
    </Page>
  );
};

export default ToolSetCreate;
