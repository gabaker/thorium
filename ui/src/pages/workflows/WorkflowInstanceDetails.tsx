import { createContext, FC, ReactNode, useCallback, useContext, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import styled from 'styled-components';

// project imports
import {
  Actions,
  Badge,
  BannerSlot,
  BannerStack,
  CodeSpans,
  EntityLink,
  InlineList,
  KeyValues,
  Mono,
  Muted,
  NoWrap,
  PageHead,
  PageTitle,
  Pre,
  Section,
  TableScroll,
  WideTable,
} from '@components/agentic/shared';
import { EntityNotFound } from '@components/agentic/EntityNotFound';
import { InstanceStatusBadge } from '@components/agentic/InstanceStatusBadge';
import { defaultModelsText, triggerText } from '@components/agentic/workflows/format';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Button, ButtonSize } from '@components/shared/buttons';
import { ButtonVariant } from '@components/shared/buttons/types';
import Page from '@components/pages/Page';
import { getAgenticEntity, getWorkflowToolset, InstancedAgentToolset } from '@thorpi/agentic';
import { isInstanceReady, isShortName, shortHash } from '@utilities/agentic/display';
import { AgenticKinds } from '@models/agentic';
import { WorkflowInstance } from '@models/workflows';

/** The button that shows an agent's tool specs, kept clear of the harness and config lines above it. */
const SpecsAction = styled.div`
  margin-top: 0.25rem;
`;

/** Whether the agents table gave up keeping short names whole because it would not fit its box; see {@link FitScroll}. */
const BreakNamesContext = createContext(false);

/** The styled text of an identifier in a table cell; see {@link WholeName}. */
const NameText = styled.span<{ $keepWhole: boolean }>`
  display: inline-block;
  max-width: 100%;
  ${({ $keepWhole }) => ($keepWhole ? 'white-space: nowrap;' : 'overflow-wrap: anywhere;')}
`;

/**
 * An identifier in a table cell. A short one never breaks (not even at a hyphen) so its column is at least as wide
 * as the name, unless the surrounding {@link FitScroll} reports that the table would overflow its box; then every
 * name may break inside itself so the table wraps rather than being clipped. A long one always breaks inside
 * itself when its cell is narrower.
 *
 * @param props - The component props.
 * @param props.name - The identifier to show, such as an agent or tool name. Only its own length decides whether it
 * is short.
 * @param props.suffix - Trailing punctuation that must stay on the same line as the name; it is shown with the name
 * but does not count toward the short-name limit, so a name is treated the same with or without a separator.
 */
export const WholeName: FC<{ name: string; suffix?: string }> = ({ name, suffix = '' }) => {
  const breakNames = useContext(BreakNamesContext);
  return (
    <NameText $keepWhole={!breakNames && isShortName(name)}>
      {name}
      {suffix}
    </NameText>
  );
};

/**
 * A horizontally scrolling table box that tells its {@link WholeName} descendants when keeping short names whole
 * makes the table wider than the box. Names start out whole; when that overflows the box (and again after a resize
 * or new content) they are allowed to break so the last column is not cut off.
 *
 * @param props - The component props.
 * @param props.children - The table to show.
 * @param props.content - Any value that changes when the table's content changes, so the fit is measured again.
 */
const FitScroll: FC<{ children: ReactNode; content: unknown }> = ({ children, content }) => {
  const box = useRef<HTMLDivElement>(null);
  const [breakNames, setBreakNames] = useState(false);
  // bumped on each resize so the whole-name layout is measured again at the new width
  const [probe, setProbe] = useState(0);
  // new content starts from whole names again
  useLayoutEffect(() => {
    setBreakNames(false);
  }, [content]);
  // measure the whole-name layout before it is painted and allow breaks only when it overflows
  useLayoutEffect(() => {
    const el = box.current;
    if (!breakNames && el && el.scrollWidth > el.clientWidth + 1) setBreakNames(true);
  }, [breakNames, probe, content]);
  useEffect(() => {
    const el = box.current;
    if (!el || typeof ResizeObserver === 'undefined') return;
    let width = el.clientWidth;
    // only a change in the box's width can change the fit, and the table's own wrapping never changes it
    const observer = new ResizeObserver(() => {
      if (el.clientWidth === width) return;
      width = el.clientWidth;
      setBreakNames(false);
      setProbe((count) => count + 1);
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, []);
  return (
    <BreakNamesContext.Provider value={breakNames}>
      <TableScroll ref={box}>{children}</TableScroll>
    </BreakNamesContext.Provider>
  );
};

/**
 * A comma separated list of names where each name stays whole when it fits and the line breaks between names, or
 * between a name and its detail, first.
 *
 * @param props - The component props.
 * @param props.names - The names to list; an empty list renders a muted "none".
 * @param props.details - Optional text shown after the name at the same position, as "name: detail"; it stays whole too.
 */
export const NameList: FC<{ names: string[]; details?: string[] }> = ({ names, details }) =>
  names.length === 0 ? (
    <Muted>none</Muted>
  ) : (
    <>
      {names.map((name, index) => {
        // the separating comma rides with the last piece of the entry so it never starts a line by itself
        const comma = index < names.length - 1 ? ',' : '';
        const detail = details?.[index];
        return (
          // the same name can appear twice in one list, so the position is part of the key
          <span key={`${index}-${name}`}>
            {/* the colon rides with the name so a detail never starts a line with ": " */}
            <WholeName name={name} suffix={detail !== undefined ? ':' : comma} />
            {detail !== undefined && (
              <>
                {' '}
                <WholeName name={detail} suffix={comma} />
              </>
            )}
            {index < names.length - 1 && ' '}
          </span>
        );
      })}
    </>
  );

/**
 * Show one workflow instance: its agents, tools, bindings, and per-agent tool specs.
 *
 * @param props - The props for the page; its state belongs to one id.
 * @param props.id - The id of the workflow instance to show.
 */
const WorkflowInstancePage: FC<{ id: string }> = ({ id }) => {
  const navigate = useNavigate();
  const [item, setItem] = useState<WorkflowInstance | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  // the kind an opened id turned out to be when it is not a workflow instance
  const [other, setOther] = useState<{ id: string; kind: string } | null>(null);
  // whether the instance read failed for a reason other than being missing or hidden (such as a server error); a retry may fix it
  const [readFailed, setReadFailed] = useState(false);
  const [toolset, setToolset] = useState<InstancedAgentToolset | null>(null);
  // whether the workflow this instance came from can no longer be read (deleted, or in groups the user cannot see)
  const [workflowGone, setWorkflowGone] = useState(false);
  /** Read the instance, telling a failed read apart from a missing or hidden instance. */
  const load = useCallback(async () => {
    setOther(null);
    setReadFailed(false);
    const found = await getAgenticEntity<WorkflowInstance>(id, setError, (status) => setReadFailed(![401, 403, 404].includes(status)));
    // an id of another kind (such as a tool) has no workflow instance metadata to show
    if (found && found.kind !== AgenticKinds.WorkflowInstance) setOther({ id, kind: found.kind });
    setItem(found && found.kind === AgenticKinds.WorkflowInstance ? found : null);
    setLoading(false);
  }, [id]);
  useEffect(() => {
    void load();
  }, [load]);
  const workflowId = item?.metadata.WorkflowInstance.workflow;
  useEffect(() => {
    setWorkflowGone(false);
    if (!workflowId) return;
    // probe the parent workflow so the back link is only offered when it leads somewhere; a failed read is not an error of this page
    void getAgenticEntity(workflowId, () => undefined).then((found) => setWorkflowGone(found === null));
  }, [workflowId]);
  if (!item) {
    return (
      <EntityNotFound
        what="Workflow instance"
        loading={loading}
        listPath="/workflows"
        listName="workflows"
        error={error}
        failed={readFailed}
        onRetry={() => {
          // show the spinner while the instance is read again
          setError('');
          setLoading(true);
          void load();
        }}
        other={other ?? undefined}
      />
    );
  }
  const meta = item.metadata.WorkflowInstance;
  const toolNames = new Map(meta.tools.map((tool) => [tool.tool_instance, `${tool.name}`]));
  const agentNames = new Map(meta.agents.map((agent) => [agent.agent, agent.name]));
  return (
    <Page title={`${item.name} · Thorium`}>
      <PageHead>
        <PageTitle data-testid="instance-title">{item.name}</PageTitle>
        <Actions>
          {isInstanceReady(meta.status) && !workflowGone && (
            // the launch page runs the workflow's newest Ready instance
            <Button
              variant={ButtonVariant.Ok}
              data-testid="run-workflow"
              onClick={() => void navigate(`/launch?workflow=${meta.workflow}`)}
            >
              Run this workflow
            </Button>
          )}
          {workflowGone ? (
            <Muted data-testid="instance-workflow-gone">Its workflow was deleted or is not visible to you</Muted>
          ) : (
            <EntityLink to={`/workflows/${meta.workflow}`}>Back to workflow</EntityLink>
          )}
        </Actions>
      </PageHead>
      <BannerSlot>
        {error && (
          <AlertBanner severity={Severity.Error}>
            <CodeSpans text={error} />
          </AlertBanner>
        )}
      </BannerSlot>
      <Section>
        <h2>Details</h2>
        <KeyValues data-testid="instance-info">
          <dt>Status</dt>
          <dd>
            <InstanceStatusBadge status={meta.status} />
          </dd>
          <dt>Trigger</dt>
          <dd>
            {triggerText(meta.trigger)} by {meta.triggered_by}
          </dd>
          <dt>Entry agent</dt>
          <dd>{meta.entry_agent ? (agentNames.get(meta.entry_agent) ?? meta.entry_agent) : <Muted>none</Muted>}</dd>
          <dt>Default models</dt>
          <dd>{meta.default_models && meta.default_models.length > 0 ? defaultModelsText(meta.default_models) : <Muted>none</Muted>}</dd>
          <dt>Max alive sub-agents</dt>
          <dd data-testid="instance-max-alive">{meta.limits?.max_alive_sub_agents ?? <Muted>default</Muted>}</dd>
          <dt>Digest</dt>
          <dd>
            <Mono>{meta.digest}</Mono>
          </dd>
        </KeyValues>
        {meta.warnings.length > 0 && (
          <BannerStack>
            {meta.warnings.map((warning) => (
              <AlertBanner key={warning} severity={Severity.Warning}>
                <CodeSpans text={warning} />
              </AlertBanner>
            ))}
          </BannerStack>
        )}
      </Section>
      <Section>
        <h2>Agents ({meta.agents.length})</h2>
        <FitScroll content={meta}>
          <WideTable data-testid="instance-agents">
            <thead>
              <tr>
                <th>Agent</th>
                <th>Tools</th>
                <th>Sub-agents</th>
                <th>Bindings</th>
              </tr>
            </thead>
            <tbody>
              {meta.agents.map((agent) => (
                <tr key={agent.agent}>
                  <td>
                    <WholeName name={agent.name} />
                    {/* the harness rides under the name rather than taking a column of its own, so the table fits a narrow window */}
                    <div data-testid={`agent-harness-${agent.name}`}>
                      <Muted>harness </Muted>
                      {agent.harness ? <WholeName name={toolNames.get(agent.harness) ?? agent.harness} /> : <Muted>none</Muted>}
                    </div>
                    <NoWrap as="div">
                      <Muted>config </Muted>
                      <Mono title={agent.config_hash}>{shortHash(agent.config_hash)}</Mono>
                    </NoWrap>
                    <SpecsAction>
                      <Button
                        variant={ButtonVariant.Secondary}
                        size={ButtonSize.XSmall}
                        data-testid={`toolset-${agent.name}`}
                        onClick={() => void getWorkflowToolset(id, agent.agent, setError).then(setToolset)}
                      >
                        Tool specs
                      </Button>
                    </SpecsAction>
                  </td>
                  <td>
                    <NameList names={agent.tools.map((tool) => toolNames.get(tool) ?? tool)} />
                  </td>
                  <td>
                    <NameList names={agent.sub_agents.map((sub) => agentNames.get(sub) ?? sub)} />
                  </td>
                  <td>
                    <NameList
                      names={agent.bindings.map((bind) => toolNames.get(bind.tool_instance) ?? bind.tool_instance)}
                      details={agent.bindings.map((bind) => bind.mechanism)}
                    />
                  </td>
                </tr>
              ))}
            </tbody>
          </WideTable>
        </FitScroll>
        {toolset && (
          <div data-testid="agent-toolset">
            <h3>Tools given to {toolset.agent.name}</h3>
            {toolset.tools.map((entry) => (
              <details key={entry.tool.tool_instance}>
                <summary>
                  {entry.tool.name} ({entry.tool.kind})
                </summary>
                <Pre>{JSON.stringify(entry.spec, null, 2)}</Pre>
              </details>
            ))}
          </div>
        )}
      </Section>
      <Section>
        <h2>Tools ({meta.tools.length})</h2>
        <TableScroll>
          <WideTable data-testid="instance-tools">
            <thead>
              <tr>
                <th>Tool</th>
                <th>Kind</th>
                <th>Version</th>
                <th>Function</th>
                <th>Link</th>
              </tr>
            </thead>
            <tbody>
              {meta.tools.map((tool) => (
                <tr key={tool.tool_instance}>
                  <td>
                    <InlineList>
                      <EntityLink to={`/tools/${tool.tool}`}>{tool.name}</EntityLink>
                      {tool.dependency && <Badge>dependency</Badge>}
                    </InlineList>
                    <NoWrap as="div">
                      <Muted>spec </Muted>
                      <Mono title={tool.spec_hash}>{shortHash(tool.spec_hash)}</Mono>
                    </NoWrap>
                  </td>
                  <td>{tool.kind}</td>
                  <td>{tool.version}</td>
                  <td>{tool.function_name ?? <Muted>none</Muted>}</td>
                  <td>{tool.pinned ? 'pinned' : 'latest'}</td>
                </tr>
              ))}
            </tbody>
          </WideTable>
        </TableScroll>
      </Section>
    </Page>
  );
};

/**
 * View a workflow instance by the id in the URL.
 *
 * The page is keyed by the id so moving between ids (such as with back and forward) starts from a clean page:
 * the previous id's content, forms, in-flight reads, and any error they reported are dropped.
 */
const WorkflowInstanceDetails: FC = () => {
  const { id = '' } = useParams();
  // keying the page by id remounts it, so every id starts loading from a clean state
  return <WorkflowInstancePage key={id} id={id} />;
};

export default WorkflowInstanceDetails;
