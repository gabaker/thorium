import { FC, useMemo, useState } from 'react';
import { useNavigate } from 'react-router-dom';

// project imports
import { useAgenticList, useEditableGroupNames } from '@components/agentic/hooks';
import { GroupChecks } from '@components/agentic/pickers';
import { Actions, CodeSpans, Field, PageHead, PageTitle, Section, Select, TextArea } from '@components/agentic/shared';
import { ActionHint } from '@components/agentic/tools/ActionHint';
import { stillOffered } from '@components/agentic/tools/filters';
import { SPEC_TEMPLATES } from '@components/agentic/tools/specTemplates';
import Page from '@components/pages/Page';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Button, ButtonVariant } from '@components/shared/buttons';
import TextInput from '@components/shared/inputs/TextInput';
import { createAgenticEntity } from '@thorpi/agentic';
import { buildToolCreateForm, parseToolSpec, specKindMismatch } from '@utilities/agentic/forms';
import { AgenticKinds } from '@models/agentic';
import { Tool, ToolKind } from '@models/tools';

/** Create a tool and publish its first version from a spec written as YAML or JSON. */
const ToolCreate: FC = () => {
  const navigate = useNavigate();
  // only groups the user may create entities in are offered
  const myGroups = useEditableGroupNames();
  const existing = useAgenticList<Tool>(AgenticKinds.Tool);
  const [touched, setTouched] = useState(false);
  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [version, setVersion] = useState('1.0.0');
  const [chosenGroups, setGroups] = useState<string[]>([]);
  // a group checked while the group details loaded may no longer be offered; it is not kept
  const groups = stillOffered(chosenGroups, myGroups);
  const [kind, setKind] = useState<ToolKind>(ToolKind.Image);
  const [spec, setSpec] = useState(SPEC_TEMPLATES[ToolKind.Image]);
  const [error, setError] = useState('');
  const [warnings, setWarnings] = useState<string[]>([]);
  const [createdId, setCreatedId] = useState('');
  const [busy, setBusy] = useState(false);
  const parsed = useMemo(() => parseToolSpec(spec), [spec]);
  // the kind the spec declares must match the kind chosen, since a tool's kind never changes
  const kindMismatch = parsed.kind !== null && parsed.kind !== kind;
  // the groups chosen that already hold a tool of this name, since the API accepts two tools with one name
  const duplicateGroups = useMemo(() => {
    const wanted = name.trim().toLowerCase();
    if (wanted === '') return [];
    const taken = existing.items.filter((tool) => tool.name.toLowerCase() === wanted).flatMap((tool) => tool.groups);
    return groups.filter((group) => taken.includes(group));
  }, [existing.items, name, groups]);
  const problem = (() => {
    if (name.trim() === '') return 'Enter a name.';
    if (groups.length === 0) return 'Choose at least one group.';
    if (version.trim() === '') return 'Enter a version label.';
    if (parsed.error) return parsed.error;
    if (kindMismatch && parsed.kind) return specKindMismatch(parsed.kind, `the kind chosen is ${kind}`);
    return '';
  })();

  /**
   * Remember that the user has started filling in the form, so a missing value is reported as a warning.
   *
   * @param change - The state change to apply.
   */
  const edit = (change: () => void) => {
    setTouched(true);
    change();
  };

  /** Change the kind and offer that kind's starting spec when the editor still holds a template. */
  const chooseKind = (next: ToolKind) => {
    const untouched = Object.values(SPEC_TEMPLATES).includes(spec);
    setKind(next);
    if (untouched) setSpec(SPEC_TEMPLATES[next]);
  };

  /** Send the create request and open the new tool. */
  const submit = async () => {
    if (problem !== '' || parsed.json === null || createdId !== '') return;
    setBusy(true);
    setError('');
    const created = await createAgenticEntity(
      buildToolCreateForm({ name, groups, description, version, spec: parsed.json, kind }),
      setError,
    );
    setBusy(false);
    if (!created) return;
    const issues = [...(created.portability?.strict_issues ?? []), ...(created.portability?.gemini_rewrites ?? [])];
    // keep the portability notes in view before leaving, since they are only returned at creation
    if (issues.length > 0) {
      setWarnings(issues);
      setCreatedId(created.id);
      return;
    }
    void navigate(`/tools/${created.id}`);
  };

  return (
    <Page title="Create Tool · Thorium">
      <PageHead>
        <PageTitle>Create tool</PageTitle>
      </PageHead>
      <Section>
        <Field>
          <span className="label">Name</span>
          <TextInput data-testid="tool-name" value={name} onChange={(event) => edit(() => setName(event.target.value))} />
        </Field>
        <Field>
          <span className="label">Description</span>
          <TextInput
            data-testid="tool-description"
            value={description}
            onChange={(event) => edit(() => setDescription(event.target.value))}
          />
        </Field>
        <GroupChecks groups={myGroups} selected={groups} onChange={(next) => edit(() => setGroups(next))} />
        <Field>
          <span className="label">Kind</span>
          <span className="hint">A tool&apos;s kind is fixed for its life.</span>
          <Select data-testid="tool-kind" value={kind} onChange={(event) => edit(() => chooseKind(event.target.value as ToolKind))}>
            {Object.values(ToolKind).map((option) => (
              <option key={option} value={option}>
                {option}
              </option>
            ))}
          </Select>
        </Field>
        <Field>
          <span className="label">First version</span>
          <TextInput data-testid="tool-version" value={version} onChange={(event) => edit(() => setVersion(event.target.value))} />
        </Field>
        <Field>
          <span className="label">Spec (YAML or JSON)</span>
          <span className="hint">Do not put secrets in env values: they are readable by every member of the tool&apos;s groups.</span>
          <TextArea
            data-testid="tool-spec"
            style={{ minHeight: '18rem' }}
            value={spec}
            onChange={(event) => edit(() => setSpec(event.target.value))}
          />
        </Field>
        {duplicateGroups.length > 0 && (
          <AlertBanner severity={Severity.Warning} className="mb-3">
            <span data-testid="tool-duplicate">
              A tool named {name.trim()} already exists in {duplicateGroups.join(', ')}. Creating this makes a second tool with the same
              name.
            </span>
          </AlertBanner>
        )}
        {problem !== '' && touched && (
          // a fresh form shows what is missing as a hint beside the button, and a warning once the user has started
          <AlertBanner severity={Severity.Warning} className="mb-3">
            <span data-testid="tool-problem">{problem}</span>
          </AlertBanner>
        )}
        {error !== '' && (
          <AlertBanner className="mb-3">
            <CodeSpans text={error} />
          </AlertBanner>
        )}
        {warnings.length > 0 && (
          <AlertBanner severity={Severity.Warning} className="mb-3">
            <div data-testid="tool-portability">
              The tool was created. Portability notes: <CodeSpans text={warnings.join('; ')} />
            </div>
            <Button variant={ButtonVariant.Primary} onClick={() => void navigate(`/tools/${createdId}`)}>
              Open the tool
            </Button>
          </AlertBanner>
        )}
        <Actions>
          <Button
            variant={ButtonVariant.Ok}
            data-testid="tool-submit"
            disabled={busy || problem !== '' || createdId !== ''}
            onClick={() => void submit()}
          >
            Create
          </Button>
          <Button variant={ButtonVariant.Secondary} onClick={() => void navigate('/tools')}>
            Cancel
          </Button>
          {problem !== '' && !touched && <ActionHint data-testid="tool-problem">{problem}</ActionHint>}
        </Actions>
      </Section>
    </Page>
  );
};

export default ToolCreate;
