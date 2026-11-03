import { FC, useState } from 'react';
import styled from 'styled-components';

// project imports
import { LaunchError, LaunchSection, NoticeBanner } from './LaunchNotices';
import { LAUNCH_KIND_NOUNS, LaunchItem, LaunchItemView, ToolArgsText } from './launchForms';
import { GroupField, SlaField, TagsField } from './RunSettings';
import { LaunchTab } from './launch';
import { InstanceStatusBadge } from '@components/agentic/InstanceStatusBadge';
import { Badge, EntityLink, Field, Muted, Select, TextArea } from '@components/agentic/shared';
import { Severity } from '@components/shared/alerts/AlertBanner';
import { Button, ButtonSize, ButtonVariant } from '@components/shared/buttons';
import { requestInstance } from '@thorpi/agentic';
import { rootInstanceName, toolInstanceName, toolVersionLabel } from '@utilities/agentic/display';
import { Tool } from '@models/tools';
import { WorkflowInstance } from '@models/workflows';

/** The list of chosen runs, one card each. */
const ItemList = styled.ol`
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
`;

/** One chosen run: its name and remove control, then its own options. */
const ItemCard = styled.li`
  border: 1px solid var(--thorium-panel-border);
  border-radius: 6px;
  padding: 0.75rem;
  > ${Field}:last-child, > ${NoticeBanner}:last-child {
    margin-bottom: 0;
  }
`;

/** The heading line of a run card: kind, name, and the remove control at the right. */
const ItemHead = styled.div`
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.5rem;
  margin-bottom: 0.75rem;
  > strong {
    flex: 1 1 10rem;
    overflow-wrap: anywhere;
  }
`;

/** The per-run options, side by side where there is room and stacked where there isn't. */
const ItemOptions = styled.div`
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(12rem, 1fr));
  column-gap: 0.75rem;
`;

/** The collapsible job args of a tool run. */
const ArgsDetails = styled.details`
  margin-bottom: 0.75rem;
  > summary {
    cursor: pointer;
    font-weight: 600;
    font-size: 0.9rem;
    margin-bottom: 0.5rem;
  }
`;

/** A short job args box. */
const ArgsArea = styled(TextArea)`
  min-height: 3rem;
`;

/** The line naming the workflow instance a run starts. */
const InstanceLine = styled.div`
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.5rem;
  margin-bottom: 0.75rem;
`;

/** The line saying why a run can't start yet, at the size of a field error. */
const ItemBlocker = styled.p`
  margin: 0;
  color: var(--thorium-error-bg);
  font-size: 0.8rem;
  font-weight: 600;
`;

/** The line naming the inputs every run receives. */
const InputsLine = styled.p`
  margin: 0.75rem 0 0 0;
`;

/** What a run card changes: some of its run's options. */
type ItemChange = (patch: Partial<Omit<LaunchItem, 'kind' | 'ref'>>) => void;

/** Props for {@link WorkflowItemBody}. */
interface WorkflowItemBodyProps {
  /** The chosen workflow run */
  view: LaunchItemView;
  /** The instance the run starts, once read */
  instance: WorkflowInstance | undefined;
  /** Read the workflow list again, after an instance build was queued */
  onWorkflowsChanged: () => void;
}

/**
 * The instance a workflow run starts, or a way to ask for one when the workflow has none. A workflow that is not in the
 * list read so far (still loading, or not available to the user) shows nothing here: the run's blocker line says why,
 * and no instance is requested for a ref the page never saw.
 */
const WorkflowItemBody: FC<WorkflowItemBodyProps> = ({ view, instance, onWorkflowsChanged }) => {
  const [note, setNote] = useState('');
  const [error, setError] = useState('');
  const requestBuild = async () => {
    setError('');
    const queued = await requestInstance('workflows', view.item.ref, setError);
    if (queued) {
      setNote('An instance build was queued. It is ready in a few seconds; reload the workflow list.');
      onWorkflowsChanged();
    }
  };
  if (!view.found) return null;
  if (view.instanceId === '') {
    return (
      <>
        <LaunchError error={error} testId="request-instance-error" />
        {note !== '' && <NoticeBanner severity={Severity.Info}>{note}</NoticeBanner>}
        <NoticeBanner severity={Severity.Warning}>
          <span data-testid="no-ready-instance">This workflow has no Ready instance yet.</span>{' '}
          <Button data-testid="request-instance" onClick={() => void requestBuild()}>
            Request an instance
          </Button>
        </NoticeBanner>
      </>
    );
  }
  return (
    <InstanceLine data-testid="run-instance-name">
      <Muted>Runs</Muted>
      <EntityLink to={`/workflows/instances/${view.instanceId}`}>
        {instance ? rootInstanceName(view.name, instance.metadata.WorkflowInstance.revision) : view.name}
      </EntityLink>
      {/* an instance read from an older API may carry no status, which is then left unsaid */}
      {instance?.metadata.WorkflowInstance.status && <InstanceStatusBadge status={instance.metadata.WorkflowInstance.status} />}
    </InstanceLine>
  );
};

/** Props for {@link ToolVersionField}. */
interface ToolVersionFieldProps {
  /** The tool */
  tool: Tool;
  /** The chosen version id */
  value: string;
  /** Called with the chosen version id */
  onChange: (instanceId: string) => void;
}

/** The version a tool run starts, the latest unless another is picked. */
const ToolVersionField: FC<ToolVersionFieldProps> = ({ tool, value, onChange }) => (
  <Field>
    <span className="label">Version</span>
    <span className="hint">
      {tool.metadata.Tool.kind} tool, latest {toolVersionLabel(tool.metadata.Tool.latest_version)}
    </span>
    <Select data-testid="run-tool-version" value={value} onChange={(event) => onChange(event.target.value)}>
      {/* a tool whose versions are all deprecated has no latest to preselect, and the select must show that */}
      <option value="">Choose a version</option>
      {(tool.metadata.Tool.instances ?? []).map((instance) => (
        <option key={instance.id} value={instance.id}>
          {toolInstanceName(tool.name, instance.version)}
          {instance.deprecated ? ' (deprecated)' : ''}
          {instance.id === tool.metadata.Tool.latest ? ' (latest)' : ''}
        </option>
      ))}
    </Select>
  </Field>
);

/** The job args fields of a tool run, with the test id and hint of each. */
const ARG_FIELDS: { key: keyof ToolArgsText; label: string; hint: string; testId: string }[] = [
  { key: 'positionals', label: 'Positional args', hint: 'One argument per line.', testId: 'run-args-positionals' },
  { key: 'kwargs', label: 'Keyword args', hint: 'One KEY=VALUE per line; repeat a key to add values.', testId: 'run-args-kwargs' },
  { key: 'switches', label: 'Switches', hint: 'One switch per line or comma separated.', testId: 'run-args-switches' },
];

/** Props for {@link ToolArgsFields}. */
interface ToolArgsFieldsProps {
  /** The args as typed */
  args: ToolArgsText;
  /** Called with the changed args */
  onChange: (args: ToolArgsText) => void;
}

/** The job args of a tool run, folded away until opened, and opened at once when any are set. */
const ToolArgsFields: FC<ToolArgsFieldsProps> = ({ args, onChange }) => {
  const set = [args.positionals, args.kwargs, args.switches].filter((text) => text.trim() !== '').length;
  return (
    <ArgsDetails open={set > 0 ? true : undefined}>
      <summary data-testid="run-args-toggle">Job args{set > 0 ? ` (${set} set)` : ''}</summary>
      {ARG_FIELDS.map((field) => (
        <Field key={field.key}>
          <span className="label">{field.label}</span>
          <span className="hint">{field.hint}</span>
          <ArgsArea
            data-testid={field.testId}
            value={args[field.key]}
            onChange={(event) => onChange({ ...args, [field.key]: event.target.value })}
          />
        </Field>
      ))}
    </ArgsDetails>
  );
};

/** Props for {@link LaunchItemCard}. */
interface LaunchItemCardProps {
  /** The chosen run */
  view: LaunchItemView;
  /** The tool a tool run starts, once listed */
  tool: Tool | undefined;
  /** The instance a workflow run starts, once read */
  instance: WorkflowInstance | undefined;
  /** Called with changed options of the run */
  onChange: ItemChange;
  /** Drops the run from the list */
  onRemove: () => void;
  /** Read the workflow list again */
  onWorkflowsChanged: () => void;
}

/** One chosen run: its kind and name, what it starts, its own options, and why it can't start yet. */
const LaunchItemCard: FC<LaunchItemCardProps> = ({ view, tool, instance, onChange, onRemove, onWorkflowsChanged }) => {
  const { item } = view;
  const noun = LAUNCH_KIND_NOUNS[item.kind];
  return (
    <ItemCard data-testid="run-item" data-key={view.key}>
      <ItemHead>
        <Badge>{noun}</Badge>
        <strong data-testid="run-item-name">{view.name}</strong>
        <Button
          size={ButtonSize.Small}
          variant={ButtonVariant.Secondary}
          data-testid="run-item-remove"
          aria-label={`Remove ${noun} ${view.name}`}
          onClick={onRemove}
        >
          Remove
        </Button>
      </ItemHead>
      {item.kind === LaunchTab.Workflow && <WorkflowItemBody view={view} instance={instance} onWorkflowsChanged={onWorkflowsChanged} />}
      {item.kind === LaunchTab.Pipeline && view.pipelineProblem !== '' && (
        <NoticeBanner severity={Severity.Warning}>
          <span data-testid="pipeline-blocker">{view.pipelineProblem}</span>
        </NoticeBanner>
      )}
      <ItemOptions>
        {tool && <ToolVersionField tool={tool} value={item.instanceId} onChange={(instanceId) => onChange({ instanceId })} />}
        {/* a pipeline always runs in its own group, so only workflow and tool runs choose one */}
        {item.kind !== LaunchTab.Pipeline && view.groups.length > 0 && (
          <GroupField groups={view.groups} value={item.group} onChange={(group) => onChange({ group })} />
        )}
        <TagsField
          label="Extra tags"
          hint="Added to the shared tags for this run."
          testId="run-item-tags"
          value={item.tags}
          onChange={(tags) => onChange({ tags })}
        />
        <SlaField
          label="SLA (seconds)"
          hint="Blank uses the shared SLA."
          testId="run-item-sla"
          value={item.sla}
          onChange={(sla) => onChange({ sla })}
        />
      </ItemOptions>
      {tool && <ToolArgsFields args={item.args} onChange={(args) => onChange({ args })} />}
      {view.blocker !== null && <ItemBlocker data-testid="run-item-blocker">{view.blocker}</ItemBlocker>}
    </ItemCard>
  );
};

/** Props for {@link RunsToLaunch}. */
interface RunsToLaunchProps {
  /** The chosen runs, across every tab */
  views: LaunchItemView[];
  /** The tools the user can see, for the versions of tool runs */
  tools: Tool[];
  /** The workflow instances read so far, by id */
  instances: Record<string, WorkflowInstance | undefined>;
  /** The sentence naming the inputs every run receives */
  inputsSummary: string;
  /** Called with a run's key and its changed options */
  onChange: (key: string, patch: Partial<Omit<LaunchItem, 'kind' | 'ref'>>) => void;
  /** Called with the key of a run to drop */
  onRemove: (key: string) => void;
  /** Read the workflow list again */
  onWorkflowsChanged: () => void;
}

/** Every run chosen on any tab, each with its own options and a remove control, and the inputs they all receive. */
export const RunsToLaunch: FC<RunsToLaunchProps> = ({ views, tools, instances, inputsSummary, onChange, onRemove, onWorkflowsChanged }) => (
  <LaunchSection data-testid="runs-to-launch">
    <h2>Runs to launch</h2>
    {views.length === 0 ? (
      <Muted data-testid="runs-to-launch-empty">
        Nothing chosen yet. Choose workflows, tools, or pipelines above; only the runs listed here are started.
      </Muted>
    ) : (
      <ItemList>
        {views.map((view) => (
          <LaunchItemCard
            key={view.key}
            view={view}
            tool={view.item.kind === LaunchTab.Tool ? tools.find((entry) => entry.id === view.item.ref) : undefined}
            instance={instances[view.instanceId]}
            onChange={(patch) => onChange(view.key, patch)}
            onRemove={() => onRemove(view.key)}
            onWorkflowsChanged={onWorkflowsChanged}
          />
        ))}
      </ItemList>
    )}
    <InputsLine data-testid="runs-inputs-summary">
      <Muted>{inputsSummary}</Muted>
    </InputsLine>
  </LaunchSection>
);
