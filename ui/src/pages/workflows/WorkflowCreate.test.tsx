import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { MemoryRouter } from 'react-router-dom';
import { afterEach, describe, expect, it, vi } from 'vitest';

// project imports
import WorkflowCreate from './WorkflowCreate';
import { AgenticKinds } from '@models/agentic';

/** The per-kind list errors, set by each test. */
const listState = vi.hoisted((): { errors: Record<string, string> } => ({ errors: {} }));

// the page shell needs the auth provider, and the lists need the API; neither is under test here
vi.mock('@components/pages/Page', () => ({
  default: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
}));
vi.mock('@components/agentic/hooks', () => ({
  useAgenticList: (kind: AgenticKinds) => ({ items: [], loading: false, error: listState.errors[kind] ?? '', reload: () => undefined }),
  useEditableGroupNames: () => ['g'],
}));

/** Render the create page. */
const render = (): string =>
  renderToStaticMarkup(
    <MemoryRouter>
      <WorkflowCreate />
    </MemoryRouter>,
  );

afterEach(() => {
  listState.errors = {};
});

describe('WorkflowCreate list errors', () => {
  it('shows the error of the Agent list', () => {
    listState.errors[AgenticKinds.Agent] = 'List Agent: showing only the first 2000 entities';
    expect(render().replace(/<[^>]*>/g, ' ')).toContain('List Agent: showing only the first 2000 entities');
  });
});
