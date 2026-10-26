import { expect as baseExpect } from '@playwright/test';
import type { Page, Route } from '@playwright/test';

import { MOCK_USER } from './helpers';

/** Playwright's `expect` with room for the dev server to compile a lazily loaded page on its first visit. */
export const expect = baseExpect.configure({ timeout: 20_000 });

/** The group every mocked entity lives in (the mocked user's only group). */
export const MOCK_GROUP = MOCK_USER.groups[0];

/**
 * Answer a mocked API request with a JSON body.
 *
 * @param route - The intercepted request.
 * @param body - The body to send, serialized as JSON.
 * @param status - The HTTP status to answer with.
 */
export function fulfillJson(route: Route, body: unknown, status = 200) {
  return route.fulfill({ status, contentType: 'application/json', body: JSON.stringify(body) });
}

/**
 * Build an agentic entity in the shape the API serves it.
 *
 * @param kind - The entity kind, which is also the metadata key.
 * @param id - The entity's id.
 * @param name - The entity's name.
 * @param metadata - The kind's metadata, exactly as the API serializes it.
 * @param groups - The groups the entity is in.
 * @returns The entity.
 */
export function mockEntity(kind: string, id: string, name: string, metadata: Record<string, unknown>, groups = [MOCK_GROUP]) {
  return {
    id,
    name,
    kind,
    metadata: { [kind]: metadata },
    description: null,
    submitter: MOCK_USER.username,
    groups,
    tags: {},
    image: null,
    created: '2026-10-01T00:00:00Z',
  };
}

/**
 * Build a tool instance summary as a tool lists it.
 *
 * @param id - The instance's id.
 * @param version - The instance's SemVer label.
 * @param deprecated - Whether the instance is deprecated.
 * @returns The summary.
 */
export function mockInstanceSummary(id: string, version: string, deprecated = false) {
  return { id, version: { SemVer: version }, spec_hash: `hash-${id}`, deprecated, created: '2026-10-01T00:00:00Z' };
}

/**
 * Serve the entity list (`GET /entities/details/`) from fixed entities, filtered by the requested kind.
 *
 * @param page - The page whose requests are mocked.
 * @param entities - Every entity the list may return.
 */
export async function mockEntityList(page: Page, entities: { kind: string }[]) {
  await page.route('**/api/entities/details/**', (route) => {
    // the UI asks for one kind at a time with `kinds[]=<Kind>`
    const kinds = new URL(route.request().url()).searchParams.getAll('kinds[]');
    return fulfillJson(route, { data: entities.filter((entity) => kinds.length === 0 || kinds.includes(entity.kind)) });
  });
}

/**
 * Choose a workflow, tool, or pipeline in a launch tab's picker by its name, the way a user ticks it in the list.
 *
 * @param page - The launch page.
 * @param testId - The picker's test id (`run-workflow`, `run-tool`, or `run-pipeline`).
 * @param name - The name the choice shows; its group note after it is ignored.
 */
export async function chooseRun(page: Page, testId: string, name: string) {
  const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  await page
    .getByTestId(testId)
    .getByRole('checkbox', { name: new RegExp(`^${escaped}(\\s|\\(|$)`) })
    .first()
    .check();
}

/**
 * Open the run the last launch started, from its link in the launch results.
 *
 * @param page - The launch page after a launch.
 * @param index - Which started run to open, the first unless given.
 */
export async function openLaunchedRun(page: Page, index = 0) {
  await page.getByTestId('launch-result-link').nth(index).getByRole('link').click({ timeout: 30_000 });
  await page.waitForURL(/\/runs\/[^/]+\/[0-9a-f-]{36}$/, { timeout: 30_000 });
}
