import { renderToStaticMarkup } from 'react-dom/server';
import { MemoryRouter } from 'react-router-dom';
import { describe, expect, it } from 'vitest';

// project imports
import { describeItem, LaunchCatalog, newLaunchItem } from './launchForms';
import { LaunchTab } from './launch';
import { RunsToLaunch } from './RunsToLaunch';
import { Workflow } from '@models/workflows';

/** A workflow with no Ready instance. */
const BARE = {
  id: 'w',
  name: 'triage',
  groups: ['system'],
  metadata: { WorkflowCatalog: { latest_instance: null } },
} as unknown as Workflow;

/** A catalog listing the bare workflow, or nothing while its list loads. */
const catalog = (workflows: Workflow[], loading: LaunchTab[]): LaunchCatalog => ({
  workflows,
  tools: [],
  pipelines: [],
  instances: {},
  loading,
});

/** Render the chosen workflow runs of a catalog as static markup. */
const render = (refs: string[], known: LaunchCatalog): string =>
  renderToStaticMarkup(
    <MemoryRouter>
      <RunsToLaunch
        views={refs.map((ref) => describeItem(newLaunchItem(LaunchTab.Workflow, ref), known, ['system']))}
        tools={[]}
        instances={{}}
        inputsSummary=""
        onChange={() => undefined}
        onRemove={() => undefined}
        onWorkflowsChanged={() => undefined}
      />
    </MemoryRouter>,
  );

describe('RunsToLaunch workflow runs', () => {
  it('offers to request an instance only for a workflow in the list', () => {
    const markup = render(['w'], catalog([BARE], []));
    expect(markup).toContain('data-testid="no-ready-instance"');
    expect(markup).toContain('data-testid="request-instance"');
  });
  it('offers no instance request for a ref that is not in the list', () => {
    const markup = render(['../../users/delete-me'], catalog([BARE], []));
    expect(markup).not.toContain('request-instance');
    expect(markup).not.toContain('no-ready-instance');
    expect(markup).toContain('is not available to you.');
  });
  it('offers no instance request while the list loads', () => {
    const markup = render(['w'], catalog([], [LaunchTab.Workflow]));
    expect(markup).not.toContain('request-instance');
    expect(markup).not.toContain('no-ready-instance');
    expect(markup).toContain('Loading the workflow list.');
  });
});
