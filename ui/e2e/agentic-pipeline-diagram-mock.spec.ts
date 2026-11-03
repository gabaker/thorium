import { test, Page } from '@playwright/test';

import { expect, fulfillJson, MOCK_GROUP, mockEntity, mockEntityList, mockInstanceSummary } from './agentic-mocks';
import { setupMockAuth } from './helpers';

const SCAN = mockEntity('Tool', '00000000-0000-0000-0000-00000000ee01', 'scan', { kind: 'Image', latest: null, modified: '' });
const EXTRACT_INSTANCE = '00000000-0000-0000-0000-00000000ee12';
const EXTRACT = mockEntity('Tool', '00000000-0000-0000-0000-00000000ee02', 'extract', {
  kind: 'Image',
  latest: EXTRACT_INSTANCE,
  modified: '',
  instances: [mockInstanceSummary(EXTRACT_INSTANCE, '1.2.0')],
});
// a tool that is not an Image tool can't back a stage, so the menu never offers it
const ASSISTANT = mockEntity('Tool', '00000000-0000-0000-0000-00000000ee03', 'assistant', { kind: 'Agentic', latest: null, modified: '' });

/** The group the mocked pipelines live in; the mocked user owns it. */
const GROUP = {
  name: MOCK_GROUP,
  owners: { combined: ['test'], direct: ['test'], metagroups: [] },
  managers: { combined: [], direct: [], metagroups: [] },
  analysts: [],
  users: { combined: [], direct: [], metagroups: [] },
  monitors: { combined: [], direct: [], metagroups: [] },
  description: '',
  allowed: {
    files: true,
    repos: true,
    tags: true,
    images: true,
    pipelines: true,
    reactions: true,
    results: true,
    comments: true,
    entities: true,
  },
};

/** A pipeline whose stages are all backed by tools. */
const CONVERTED = {
  group: MOCK_GROUP,
  name: 'converted',
  creator: 'test',
  order: ['unpack', 'scan'],
  sla: 86400,
  triggers: {},
  stage_tools: { unpack: EXTRACT.id, scan: SCAN.id },
};

/** A pipeline of image stages: its stages are image names and no tool backs them. */
const LEGACY = { group: MOCK_GROUP, name: 'legacy', creator: 'test', order: ['old-image'], sla: 86400, triggers: {} };

/** The body of each PATCH the page sent, newest last. */
type Patches = Record<string, unknown>[];

/**
 * Mock the pipeline list, the tools, and the pipeline update, and record the updates.
 *
 * @param page - A signed in page.
 * @param tools - The tools the entity list serves.
 * @returns The recorded update bodies.
 */
async function mockPipelines(page: Page, tools: { kind: string }[]): Promise<Patches> {
  const patches: Patches = [];
  await page.route('**/api/groups/details/**', (route) => fulfillJson(route, { details: [GROUP] }));
  await page.route('**/api/pipelines/list/**/details/**', (route) => fulfillJson(route, { details: [CONVERTED, LEGACY] }));
  await mockEntityList(page, tools);
  await page.route('**/api/pipelines/*/*', (route) => {
    if (route.request().method() === 'PATCH') {
      patches.push(route.request().postDataJSON() as Record<string, unknown>);
      return route.fulfill({ status: 204 });
    }
    const name = route.request().url().split('/').pop();
    return fulfillJson(route, name === 'legacy' ? LEGACY : CONVERTED);
  });
  return patches;
}

/**
 * Open a pipeline's accordion on the pipelines page and wait for its diagram.
 *
 * @param page - A signed in page.
 * @param index - The pipeline's position in the list.
 * @param stages - The number of stages the diagram should show.
 */
async function openPipeline(page: Page, index: number, stages: number) {
  await page.goto('/pipelines');
  await page.locator('[data-testid="accordion-header"]').nth(index).click();
  await expect(page.locator('.react-flow__node-imageStep')).toHaveCount(stages);
}

/**
 * Open the diagram's add menu by right-clicking its empty pane and choosing Insert Stage.
 *
 * @param page - A page showing an editable diagram.
 */
async function openAddMenu(page: Page) {
  await page
    .locator('.react-flow')
    .first()
    .click({ button: 'right', position: { x: 40, y: 20 } });
  await page.getByText('Insert Stage').click();
}

test.describe('pipeline order diagram', () => {
  test.beforeEach(async ({ page }) => {
    await setupMockAuth(page);
  });

  test('offers the Image tools of the group, each at latest and at every version', async ({ page }) => {
    await mockPipelines(page, [SCAN, EXTRACT, ASSISTANT]);
    await openPipeline(page, 0, 2);
    await openAddMenu(page);
    await expect(page.getByRole('option')).toHaveText(['extract · follow latest', 'extract · pin 1.2.0', 'scan · follow latest']);
  });

  test('keeps each option of a long-named tool on one line', async ({ page }) => {
    const longTool = mockEntity('Tool', '00000000-0000-0000-0000-00000000ee04', 'a-tool-with-a-rather-long-name-huge', {
      kind: 'Image',
      latest: null,
      modified: '',
    });
    await mockPipelines(page, [SCAN, longTool]);
    await openPipeline(page, 0, 2);
    await openAddMenu(page);
    const options = page.getByRole('option');
    await expect(options).toHaveCount(2);
    const heights = await options.evaluateAll((nodes) => nodes.map((node) => node.getBoundingClientRect().height));
    expect(new Set(heights).size).toBe(1);
  });

  test('add a stage from the diagram, then save sends the order and its stage tool', async ({ page }) => {
    const patches = await mockPipelines(page, [SCAN, EXTRACT]);
    await openPipeline(page, 0, 2);
    await page.getByTestId('header-btn-edit').first().click();
    await openAddMenu(page);
    await page.getByRole('option', { name: 'extract · follow latest' }).click();
    // the new stage is named after its tool
    await expect(page.locator('.react-flow__node-imageStep')).toHaveCount(3);
    await page.getByTestId('header-btn-accept').first().click();
    await expect.poll(() => patches.length).toBe(1);
    const order = patches[0]['order'] as string[];
    expect(order.flat().sort()).toEqual(['extract', 'scan', 'unpack']);
    expect(patches[0]['add_stage_tools']).toEqual({ extract: EXTRACT.id });
  });

  test('a pinned version is recorded as its instance id, and a second stage of a tool gets its own name', async ({ page }) => {
    const patches = await mockPipelines(page, [SCAN, EXTRACT]);
    await openPipeline(page, 0, 2);
    await page.getByTestId('header-btn-edit').first().click();
    await openAddMenu(page);
    await page.getByRole('option', { name: 'extract · pin 1.2.0' }).click();
    await openAddMenu(page);
    await page.getByRole('option', { name: 'scan · follow latest' }).click();
    await expect(page.locator('.react-flow__node-imageStep')).toHaveCount(4);
    await page.getByTestId('header-btn-accept').first().click();
    await expect.poll(() => patches.length).toBe(1);
    expect(patches[0]['add_stage_tools']).toEqual({ extract: EXTRACT_INSTANCE, 'scan-2': SCAN.id });
  });

  test('remove a stage from the diagram, then save sends the shorter order and no stage tool', async ({ page }) => {
    const patches = await mockPipelines(page, [SCAN, EXTRACT]);
    await openPipeline(page, 0, 2);
    await page.getByTestId('header-btn-edit').first().click();
    await page.locator('.react-flow__node-imageStep').first().click({ button: 'right' });
    await page.getByText('Remove Stage').click();
    await expect(page.locator('.react-flow__node-imageStep')).toHaveCount(1);
    await page.getByTestId('header-btn-accept').first().click();
    await expect.poll(() => patches.length).toBe(1);
    expect((patches[0]['order'] as string[]).flat()).toHaveLength(1);
    expect(patches[0]).not.toHaveProperty('add_stage_tools');
  });

  test('a stage added on the details diagram is applied with its stage tool', async ({ page }) => {
    const patches = await mockPipelines(page, [SCAN, EXTRACT]);
    await openPipeline(page, 0, 2);
    await openAddMenu(page);
    await page.getByRole('option', { name: 'extract · follow latest' }).click();
    await expect(page.locator('.react-flow__node-imageStep')).toHaveCount(3);
    await page.getByRole('button', { name: 'Apply' }).click();
    await expect.poll(() => patches.length).toBe(1);
    expect((patches[0]['order'] as string[]).flat().sort()).toEqual(['extract', 'scan', 'unpack']);
    expect(patches[0]['add_stage_tools']).toEqual({ extract: EXTRACT.id });
  });

  test('a pipeline of image stages shows its image stages and a stage added to it carries only its own tool', async ({ page }) => {
    const patches = await mockPipelines(page, [SCAN, EXTRACT]);
    await openPipeline(page, 1, 1);
    await expect(page.locator('.react-flow__node-imageStep')).toHaveText(['old-image']);
    await expect(page.getByTestId('stage-tool-row-old-image')).toContainText('needs a tool');
    await openAddMenu(page);
    await page.getByRole('option', { name: 'scan · follow latest' }).click();
    await page.getByRole('button', { name: 'Apply' }).click();
    await expect.poll(() => patches.length).toBe(1);
    expect(patches[0]['add_stage_tools']).toEqual({ scan: SCAN.id });
  });

  test('a group with no stage tools shows an empty state that links to creating a tool', async ({ page }) => {
    const patches = await mockPipelines(page, [ASSISTANT]);
    await openPipeline(page, 0, 2);
    await openAddMenu(page);
    const empty = page.getByTestId('stage-tool-empty');
    await expect(empty).toContainText(`No stage tools in group ${MOCK_GROUP}`);
    await empty.getByRole('link', { name: 'Create a tool' }).click();
    await expect(page).toHaveURL(/\/tools\/create$/);
    expect(patches).toHaveLength(0);
  });
});
