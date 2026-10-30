import { test } from '@playwright/test';
import type { Page, Route } from '@playwright/test';

import { expect } from './agentic-mocks';
import { MOCK_USER } from './helpers';

/** The group the mocked user belongs to. */
const MOCK_GROUP = MOCK_USER.groups[0];

// Mocked tests: the file results and reaction tabs, the file not-found page, and the repo details
// page against a stubbed API (no live backend needed).

/** A sample SHA256 the mocked file page shows. */
const SHA = 'b'.repeat(64);

/** The id of a workflow instance whose result is stored on the sample. */
const INSTANCE = '66666666-6666-4666-8666-666666666666';

/** The key a workflow run stores its result under. */
const RESULT_KEY = `triage#r3~${INSTANCE}`;

/** A repo the mocked repo page shows. */
const REPO = 'git.example.test/sweep/demo-repo';

/** Answer a route with a JSON body. */
function json(route: Route, body: unknown, status = 200) {
  return route.fulfill({ status, contentType: 'application/json', body: JSON.stringify(body) });
}

/** Sign in as a mocked user and answer every other API call with an empty object. */
async function mockAuth(page: Page) {
  // catch-all first so the specific routes registered later take priority
  await page.route('**/api/**', (route) => json(route, {}));
  await page.route('**/api/users/whoami', (route) => json(route, MOCK_USER));
  // the tag option counts the file filters load must carry a `tags` map
  await page.route('**/api/files/count/**', (route) => json(route, { tags: {} }));
  // the association graphs are not under test, and the empty catch-all body is not a graph
  await page.route('**/api/trees/**', (route) => json(route, { error: 'no graph in this test' }, 404));
  await page.context().addCookies([{ name: 'THORIUM_TOKEN', value: MOCK_USER.token, domain: 'localhost', path: '/' }]);
}

/** A reaction listed for the sample. */
function makeReaction(id: string, pipeline: string, source?: Record<string, unknown>) {
  return {
    id,
    group: MOCK_GROUP,
    creator: 'test',
    pipeline,
    status: 'Completed',
    current_stage: 1,
    current_stage_progress: 0,
    current_stage_length: 0,
    args: {},
    sla: '2099-01-01T00:00:00Z',
    jobs: [],
    tags: [],
    sub_reactions: 0,
    completed_sub_reactions: 0,
    generators: [],
    samples: [SHA],
    ephemeral: [],
    parent_ephemeral: {},
    repos: [],
    has_cache: false,
    ...(source ? { source } : {}),
  };
}

/** Stub the sample's details. */
async function mockSample(page: Page) {
  await page.route(`**/api/files/sample/${SHA}`, (route) =>
    json(route, {
      sha256: SHA,
      sha1: 'c'.repeat(40),
      md5: 'd'.repeat(32),
      tags: {},
      submissions: [
        {
          id: 'sub-1',
          name: 'a.bin',
          description: null,
          groups: [MOCK_GROUP],
          submitter: 'test',
          uploaded: '2026-01-01T00:00:00Z',
          origin: 'None',
        },
      ],
      comments: [],
    }),
  );
}

test.describe('File results and reactions', () => {
  test('a workflow result shows its instance name and a Custom result with no renderer shows JSON', async ({ page }) => {
    await mockAuth(page);
    await mockSample(page);
    await page.route(`**/api/files/results/${SHA}**`, (route) =>
      json(route, {
        results: {
          [RESULT_KEY]: [
            {
              id: 'r1',
              groups: [MOCK_GROUP],
              uploaded: '2026-01-01T00:00:00Z',
              result: { ok: true },
              files: [],
              entities: {},
              children: {},
              display_type: 'Json',
            },
          ],
          'legacy-stub': [
            {
              id: 'r2',
              groups: [MOCK_GROUP],
              uploaded: '2026-01-01T00:00:00Z',
              result: { marker: 'LEGACY-MARKER' },
              files: [],
              entities: {},
              children: {},
              display_type: 'Custom',
            },
          ],
        },
      }),
    );
    await page.goto(`/file/${SHA}`);
    // the card header and the table of contents name the instance, not the full result key
    await expect(page.getByText('triage#r3', { exact: true }).first()).toBeVisible();
    await expect(page.getByText(INSTANCE)).toHaveCount(0);
    // the converted legacy tool's JSON shows instead of an empty body
    await expect(page.getByText('LEGACY-MARKER')).toBeVisible();
  });

  test('the reaction tab opens tool and workflow runs in the run view and pipelines in the reaction page', async ({ page }) => {
    await mockAuth(page);
    await mockSample(page);
    await page.route('**/api/users/whoami', (route) => json(route, MOCK_USER));
    await page.route(`**/api/files/results/${SHA}**`, (route) => json(route, { results: {} }));
    const provenance = { agents: [], tools: [] };
    const reactions = [
      makeReaction('11111111-1111-4111-8111-111111111111', 'tool:scan v1.0.0', { Tool: { tool_instance: 'i', provenance } }),
      makeReaction('22222222-2222-4222-8222-222222222222', 'plain-pipe', { Pipeline: { name: 'plain-pipe', provenance } }),
    ];
    await page.route('**/api/reactions/tag/**', (route) => json(route, { cursor: null, details: reactions }));
    await page.goto(`/file/${SHA}#reactionstatus`);
    await page.getByText('Reaction Status').first().click();
    await expect(page.getByText('tool:scan v1.0.0')).toBeVisible();
    await expect(page.locator('a[href="/runs/system/11111111-1111-4111-8111-111111111111"]').first()).toBeVisible();
    await expect(page.locator('a[href="/reaction/system/22222222-2222-4222-8222-222222222222"]').first()).toBeVisible();
  });

  test('below 1200 the results take the full width because the hidden table of contents reserves no column', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 900 });
    await mockAuth(page);
    await mockSample(page);
    await page.route(`**/api/files/results/${SHA}**`, (route) =>
      json(route, {
        results: {
          'scan-tool': [
            {
              id: 'r1',
              groups: [MOCK_GROUP],
              uploaded: '2026-01-01T00:00:00Z',
              result: { marker: 'WIDE-MARKER' },
              files: [],
              entities: {},
              children: {},
              display_type: 'Custom',
            },
          ],
        },
      }),
    );
    await page.goto(`/file/${SHA}`);
    await expect(page.getByText('WIDE-MARKER')).toBeVisible();
    await expect(page.locator('.results-toc-col')).toBeHidden();
    const container = await page.locator('#results-tab').boundingBox();
    const card = await page.locator('#results-tab .results-content').first().boundingBox();
    expect(container).not.toBeNull();
    expect(card).not.toBeNull();
    // the result card fills its container instead of stopping short of an empty column
    expect(card!.width).toBeGreaterThan(container!.width * 0.97);
  });

  test('the reaction tab keeps each row on one line with its checkbox at the row end, even at 1024', async ({ page }) => {
    await page.setViewportSize({ width: 1024, height: 900 });
    await mockAuth(page);
    await mockSample(page);
    await page.route(`**/api/files/results/${SHA}**`, (route) => json(route, { results: {} }));
    const provenance = { agents: [], tools: [] };
    const reactions = [
      makeReaction('11111111-1111-4111-8111-111111111111', 'workflow:jup-stub-wf-ok#r1', { Tool: { tool_instance: 'i', provenance } }),
      makeReaction('22222222-2222-4222-8222-222222222222', 'tool:jup-stub-tool v1.0.0', { Tool: { tool_instance: 'i', provenance } }),
    ];
    await page.route('**/api/reactions/tag/**', (route) => json(route, { cursor: null, details: reactions }));
    await page.goto(`/file/${SHA}#reactionstatus`);
    await page.getByText('Reaction Status').first().click();
    const label = page.getByText('workflow:jup-stub-wf-ok#r1');
    await expect(label).toBeVisible();
    const labelBox = await label.boundingBox();
    const row = page.locator('#reactionstatus-tab .reactions-grid').nth(1);
    const checkbox = await row.locator('input[type="checkbox"]').boundingBox();
    const rowBox = await row.boundingBox();
    const headerBox = await page.locator('#reactionstatus-tab .reactions-grid').first().boundingBox();
    const headerCheckbox = await page
      .locator('#reactionstatus-tab .reactions-grid')
      .first()
      .locator('input[type="checkbox"]')
      .boundingBox();
    expect(labelBox && checkbox && rowBox && headerBox && headerCheckbox).toBeTruthy();
    // the run label does not break inside its name, so its line height is one line
    expect(labelBox!.height).toBeLessThan(30);
    // the checkbox sits on the label's line, at the end of the row
    expect(Math.abs(checkbox!.y + checkbox!.height / 2 - (labelBox!.y + labelBox!.height / 2))).toBeLessThan(12);
    expect(checkbox!.x).toBeGreaterThan(rowBox!.x + rowBox!.width * 0.85);
    // the header's checkbox is in the same column as each row's
    expect(Math.abs(headerCheckbox!.x - checkbox!.x)).toBeLessThan(4);
    // the header's controls share one line
    expect(headerBox!.height).toBeLessThan(80);
  });

  test('the reaction tab shows each reaction id on one line, and the creator and group without breaking inside a word', async ({
    page,
  }) => {
    await mockAuth(page);
    await mockSample(page);
    await page.route(`**/api/files/results/${SHA}**`, (route) => json(route, { results: {} }));
    const provenance = { agents: [], tools: [] };
    const ids = ['a9951011-0092-4d70-9327-7d5554cda94d', '3ca383dd-d9fe-4760-8d6c-f80b3808d2b2', '759cd2a8-c245-449d-a435-583d7d6f5fed'];
    const reactions = ids.map((id) => makeReaction(id, 'jup-stub-pipe', { Tool: { tool_instance: 'i', provenance } }));
    await page.route('**/api/reactions/tag/**', (route) => json(route, { cursor: null, details: reactions }));
    // wide viewports show the whole id; narrow ones keep it on its line and cut it with an ellipsis
    for (const width of [1920, 1440, 1350, 1250, 1024]) {
      await page.setViewportSize({ width, height: 900 });
      await page.goto(`/file/${SHA}#reactionstatus`);
      await page.getByText('Reaction Status').first().click();
      await expect(page.locator('#reactionstatus-tab .reactions-grid')).toHaveCount(ids.length + 1);
      const cells = await page.locator('#reactionstatus-tab .reactions-grid').evaluateAll((rows) =>
        rows.map((row) => {
          // the line count of a cell is its rendered height over its line height
          const lines = (el: Element | null) => {
            if (!el) return 0;
            const style = getComputedStyle(el);
            return Math.round(el.getBoundingClientRect().height / parseFloat(style.lineHeight));
          };
          const text = (selector: string) => row.querySelector(selector)?.textContent ?? '';
          return {
            id: lines(row.querySelector('.reactions-id')),
            creator: lines(row.querySelector('.reactions-creator')),
            group: lines(row.querySelector('.reactions-group')),
            idText: text('.reactions-id'),
            idClipped: (() => {
              const el = row.querySelector('.reactions-id > *') ?? row.querySelector('.reactions-id');
              return el ? el.scrollWidth > el.clientWidth : false;
            })(),
          };
        }),
      );
      for (const cell of cells) {
        // the id, creator, and group (header or row) each take exactly one line
        expect(cell.id, `id lines at ${width}`).toBe(1);
        if (width > 1200) expect(cell.creator, `creator lines at ${width}`).toBe(1);
        expect(cell.group, `group lines at ${width}`).toBe(1);
      }
      // every row's id, creator, and group carry their full text as a title so a clipped value stays readable on hover
      const titles = await page
        .locator('#reactionstatus-tab a.reactions-id')
        .evaluateAll((links) => links.map((link) => link.getAttribute('title')));
      expect(titles, `id titles at ${width}`).toEqual(ids);
      await expect(page.locator('#reactionstatus-tab a.reactions-creator').first()).toHaveAttribute('title', 'test');
      await expect(page.locator('#reactionstatus-tab a.reactions-group').first()).toHaveAttribute('title', MOCK_GROUP);
      // at 1350 and wider every id is shown whole
      if (width >= 1350) {
        for (const cell of cells.slice(1)) expect(cell.idClipped, `id clipped at ${width}`).toBe(false);
      }
    }
  });

  test('a file that is not found shows the error and no details, tabs, or result requests', async ({ page }) => {
    await mockAuth(page);
    let resultRequests = 0;
    page.on('request', (req) => {
      if (req.url().includes('/api/files/results/')) resultRequests += 1;
    });
    await page.route(`**/api/files/sample/${SHA}`, (route) => json(route, { error: `sample ${SHA} not found` }, 404));
    await page.goto(`/file/${SHA}`);
    await expect(page.getByTestId('entity-not-found-title')).toHaveText('File not found');
    await expect(page.getByText(`sample ${SHA} not found`)).toBeVisible();
    await expect(page.getByRole('link', { name: 'Back to files' })).toHaveAttribute('href', '/files');
    await expect(page.getByRole('tab', { name: 'Reaction Status' })).toHaveCount(0);
    await expect(page.getByText('No Tool Results Available')).toHaveCount(0);
    expect(resultRequests).toBe(0);
  });

  test("deleting a file's last submission shows the success banner above the file not found page", async ({ page }) => {
    await mockAuth(page);
    await mockSample(page);
    await page.route('**/api/files/results/**', (route) => json(route, { results: {} }));
    await page.goto(`/file/${SHA}`);
    // once the submission is deleted the file is gone, so the sample read fails
    await page.route(`**/api/files/sample/${SHA}/sub-1**`, async (route) => {
      await page.route(`**/api/files/sample/${SHA}`, (r) => json(r, { error: `sample ${SHA} not found` }, 404));
      return route.fulfill({ status: 204 });
    });
    await page.getByRole('button', { name: 'Delete submission' }).click();
    await page.getByRole('button', { name: 'Confirm' }).click();
    await expect(page.getByText('Submission deleted successfully!')).toBeVisible();
    await expect(page.getByTestId('entity-not-found-title')).toHaveText('File not found');
    await expect(page.getByRole('link', { name: 'Back to files' })).toHaveAttribute('href', '/files');
    // the banner does not touch the title below it
    const bannerBox = await page.getByText('Submission deleted successfully!').boundingBox();
    const titleBox = await page.getByTestId('entity-not-found-title').boundingBox();
    expect((titleBox?.y ?? 0) - ((bannerBox?.y ?? 0) + (bannerBox?.height ?? 0))).toBeGreaterThanOrEqual(8);
    // the API's message shows once, as the muted detail, not again as a red banner
    await expect(page.getByText(`sample ${SHA} not found`)).toHaveCount(1);
  });
});

test.describe('Repo details', () => {
  test('shows the repo facts, tags, branches, git tags, and commits', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/repos/data/${REPO}`, (route) =>
      json(route, {
        provider: 'git.example.test',
        user: 'sweep',
        name: 'demo-repo',
        url: REPO,
        tags: { kind: { demo: [MOCK_GROUP] }, Results: { 'triage-tool': [MOCK_GROUP], [`triage#r3~${INSTANCE}`]: [MOCK_GROUP] } },
        submissions: [{ groups: [MOCK_GROUP], id: 's1', creator: 'test', uploaded: '2026-01-01T00:00:00Z', scheme: 'Https' }],
        earliest: '2026-01-01T00:00:00Z',
      }),
    );
    await page.route(`**/api/repos/commitish-details/${REPO}**`, (route) =>
      json(route, {
        data: [
          { Branch: { name: 'main', groups: [MOCK_GROUP], commit: 'abcdef0123456789', timestamp: '2026-01-03T00:00:00Z' } },
          { Branch: { name: 'dev', groups: [MOCK_GROUP], commit: '0123456789abcdef', timestamp: '2026-01-02T00:00:00Z' } },
          { Tag: { name: 'v1', groups: [MOCK_GROUP], commit: 'abcdef0123456789', author: 'sweep', timestamp: '2026-01-01T00:00:00Z' } },
          {
            Commit: {
              hash: 'abcdef0123456789',
              groups: [MOCK_GROUP],
              timestamp: '2026-01-03T00:00:00Z',
              author: 'sweep',
              topic: 'second commit',
              truncated: false,
            },
          },
        ],
      }),
    );
    await page.goto(`/repo/${REPO}`);
    await expect(page.getByTestId('repo-facts')).toContainText('git.example.test');
    await expect(page.getByText('kind: demo').first()).toBeVisible();
    // the internal Results tags (with the workflow instance id) stay off the tag list; the Results panel shows them
    await expect(page.getByText('Results: triage-tool')).toHaveCount(0);
    await expect(page.getByText(INSTANCE)).toHaveCount(0);
    await expect(page.getByTestId('repo-branches')).toContainText('main');
    await expect(page.getByTestId('repo-branches')).toContainText('dev');
    await expect(page.getByTestId('repo-git-tags')).toContainText('v1');
    await expect(page.getByTestId('repo-commits')).toContainText('second commit');
    // the branch, tag and commit tables share one column grid: their second columns line up
    const branchCommit = await page.getByTestId('repo-branches').locator('th').nth(1).boundingBox();
    const tagCommit = await page.getByTestId('repo-git-tags').locator('th').nth(1).boundingBox();
    const commitTopic = await page.getByTestId('repo-commits').locator('th').nth(1).boundingBox();
    expect(Math.abs(branchCommit!.x - tagCommit!.x)).toBeLessThan(1);
    expect(Math.abs(branchCommit!.x - commitTopic!.x)).toBeLessThan(1);
  });

  test('a repo that is not found shows the error and no repo header', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/repos/data/${REPO}`, (route) => json(route, { error: `repo ${REPO} not found` }, 404));
    await page.goto(`/repo/${REPO}`);
    await expect(page.getByTestId('entity-not-found-title')).toHaveText('Repo not found');
    await expect(page.getByText(`repo ${REPO} not found`)).toBeVisible();
    await expect(page.getByRole('link', { name: 'Back to repos' })).toHaveAttribute('href', '/repos');
    await expect(page.getByText('Failed to load graph')).toHaveCount(0);
    await expect(page.getByTestId('repo-facts')).toHaveCount(0);
  });

  /** Answer the repo data route with a repo that has no tags. */
  async function mockRepoData(page: Page) {
    await page.route(`**/api/repos/data/${REPO}`, (route) =>
      json(route, {
        provider: 'git.example.test',
        user: 'sweep',
        name: 'demo-repo',
        url: REPO,
        tags: {},
        submissions: [],
      }),
    );
  }

  test('a failed commitish listing shows its error and leaves the rest of the repo page', async ({ page }) => {
    await mockAuth(page);
    await mockRepoData(page);
    await page.route(`**/api/repos/commitish-details/${REPO}**`, (route) => json(route, { error: 'listing failed' }, 500));
    await page.goto(`/repo/${REPO}`);
    await expect(page.getByText('listing failed')).toBeVisible();
    await expect(page.getByTestId('repo-facts')).toContainText('git.example.test');
    await expect(page.getByTestId('repo-refs-more')).toHaveCount(0);
  });

  test('a repo with more commitishes than one page offers to load the next page', async ({ page }) => {
    await mockAuth(page);
    await mockRepoData(page);
    const cursors: Array<string | null> = [];
    await page.route(`**/api/repos/commitish-details/${REPO}**`, (route) => {
      const cursor = new URL(route.request().url()).searchParams.get('cursor');
      cursors.push(cursor);
      const branch = (name: string) => ({
        Branch: { name, groups: [MOCK_GROUP], commit: 'abcdef0123456789', timestamp: '2026-01-03T00:00:00Z' },
      });
      return cursor
        ? json(route, { data: [branch('second-page-branch')] })
        : json(route, { data: [branch('first-page-branch')], cursor: '11111111-1111-4111-8111-111111111111' });
    });
    await page.goto(`/repo/${REPO}`);
    await expect(page.getByTestId('repo-branches')).toContainText('first-page-branch');
    await page.getByTestId('repo-refs-more').click();
    await expect(page.getByTestId('repo-branches')).toContainText('second-page-branch');
    await expect(page.getByTestId('repo-branches')).toContainText('first-page-branch');
    await expect(page.getByTestId('repo-refs-more')).toHaveCount(0);
    // the dev server may read the first page twice, so only the continuation reads are checked
    expect(cursors.filter(Boolean)).toEqual(['11111111-1111-4111-8111-111111111111']);
  });
});

test.describe('Repo results', () => {
  test('shows the results stored on the repo and reads them from the repo routes', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/repos/data/${REPO}`, (route) =>
      json(route, {
        provider: 'git.example.test',
        user: 'sweep',
        name: 'demo-repo',
        url: REPO,
        tags: {},
        submissions: [{ groups: [MOCK_GROUP], id: 's1', creator: 'test', uploaded: '2026-01-01T00:00:00Z', scheme: 'Https' }],
        earliest: '2026-01-01T00:00:00Z',
      }),
    );
    await page.route(`**/api/repos/commitish-details/${REPO}**`, (route) => json(route, { cursor: null, data: [] }));
    const resultRequests: string[] = [];
    await page.route(`**/api/repos/results/${REPO}**`, (route) => {
      resultRequests.push(new URL(route.request().url()).pathname);
      return json(route, {
        results: {
          [RESULT_KEY]: [
            {
              id: 'r1',
              groups: [MOCK_GROUP],
              uploaded: '2026-01-01T00:00:00Z',
              result: { marker: 'REPO-RESULT-MARKER' },
              files: ['report.txt'],
              entities: {},
              children: {},
              display_type: 'Custom',
            },
          ],
        },
      });
    });
    let fileDownload = '';
    await page.route(`**/api/repos/result-files/**`, (route) => {
      fileDownload = new URL(route.request().url()).pathname;
      return route.fulfill({ status: 200, contentType: 'application/octet-stream', body: 'report body' });
    });
    await page.goto(`/repo/${REPO}`);
    await expect(page.getByTestId('repo-results')).toContainText('REPO-RESULT-MARKER');
    await expect(page.getByTestId('repo-results')).toContainText('triage#r3');
    expect(resultRequests.length).toBeGreaterThan(0);
    // a result file is read from the repo's result-files route, with the workflow key's # encoded
    await page.getByRole('button', { name: 'Download all results' }).click();
    await expect.poll(() => fileDownload).toContain(`/api/repos/result-files/${REPO}/triage%23r3~${INSTANCE}/r1`);
  });

  test('a short list of results keeps the panel as tall as its cards on a wide viewport', async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await mockAuth(page);
    await page.route(`**/api/repos/data/${REPO}`, (route) =>
      json(route, { provider: 'git.example.test', user: 'sweep', name: 'demo-repo', url: REPO, tags: {}, submissions: [] }),
    );
    await page.route(`**/api/repos/commitish-details/${REPO}**`, (route) => json(route, { cursor: null, data: [] }));
    const result = (marker: string) => [
      {
        id: `r-${marker}`,
        groups: [MOCK_GROUP],
        uploaded: '2026-01-01T00:00:00Z',
        result: { marker },
        files: [],
        entities: {},
        children: {},
        display_type: 'Custom',
      },
    ];
    await page.route(`**/api/repos/results/${REPO}**`, (route) =>
      json(route, { results: { 'tool-a': result('MARKER-A'), 'tool-b': result('MARKER-B') } }),
    );
    await page.goto(`/repo/${REPO}`);
    await expect(page.getByTestId('repo-results')).toContainText('MARKER-B');
    await expect(page.locator('.results-toc-col')).toBeVisible();
    const container = await page.locator('#results-tab').boundingBox();
    const cards = await page.locator('#results-tab .results-content').evaluateAll((els) => {
      const rects = els.map((el) => el.getBoundingClientRect());
      return Math.max(...rects.map((r) => r.bottom)) - Math.min(...rects.map((r) => r.top));
    });
    expect(container).not.toBeNull();
    // the table of contents does not stretch the row to the viewport height, leaving blank space under the cards
    expect(container!.height).toBeLessThan(cards + 100);
  });

  test('a repo with no results says so', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/repos/data/${REPO}`, (route) =>
      json(route, {
        provider: 'git.example.test',
        user: 'sweep',
        name: 'demo-repo',
        url: REPO,
        tags: {},
        submissions: [],
        earliest: null,
      }),
    );
    await page.route(`**/api/repos/commitish-details/${REPO}**`, (route) => json(route, { cursor: null, data: [] }));
    await page.route(`**/api/repos/results/${REPO}**`, (route) => json(route, { results: {} }));
    await page.goto(`/repo/${REPO}`);
    await expect(page.getByTestId('repo-results')).toContainText('No tool results available.');
    const container = await page.locator('#results-tab').boundingBox();
    const banner = await page.locator('#results-tab').getByText('No tool results available.').boundingBox();
    expect(container).not.toBeNull();
    expect(banner).not.toBeNull();
    // the empty-state banner spans the panel instead of stopping short of an empty table of contents column
    expect(banner!.width).toBeGreaterThan(container!.width * 0.97);
  });
});

test.describe('Reaction pages', () => {
  /** The id of the reaction the pages read. */
  const REACTION = '77777777-7777-4777-8777-777777777777';

  test('a reaction that is not found shows a titled message with a way back', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/${MOCK_GROUP}/${REACTION}`, (route) => json(route, { error: 'reaction not found' }, 404));
    await page.goto(`/reaction/${MOCK_GROUP}/${REACTION}`);
    await expect(page.getByTestId('entity-not-found-title')).toHaveText('Reaction not found');
    await expect(page.getByText('Get Reaction: reaction not found')).toBeVisible();
    await expect(page.getByRole('link', { name: 'Back to runs' })).toHaveAttribute('href', '/runs');
  });

  test('a stage log read that is denied shows its message, a title, and links back', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/logs/${MOCK_GROUP}/${REACTION}/build**`, (route) =>
      json(route, { error: 'Permission Denied' }, 401),
    );
    await page.goto(`/reaction/logs/${MOCK_GROUP}/${REACTION}/build`);
    await expect(page.getByRole('heading', { name: 'Stage logs: build' })).toBeVisible();
    await expect(page.getByTestId('stage-logs-error')).toContainText('Permission Denied');
    await expect(page.getByRole('link', { name: 'Back to reaction' })).toHaveAttribute('href', `/reaction/${MOCK_GROUP}/${REACTION}`);
    await expect(page.getByRole('link', { name: 'Open in run view' })).toHaveAttribute('href', `/runs/${MOCK_GROUP}/${REACTION}`);
    // the retry button does not touch the error banner above it
    const banner = await page.getByTestId('stage-logs-error').locator('> div').first().boundingBox();
    const retry = await page.getByTestId('stage-logs-retry').boundingBox();
    expect((retry?.y ?? 0) - ((banner?.y ?? 0) + (banner?.height ?? 0))).toBeGreaterThanOrEqual(8);
  });

  test('a denied stage log read hides the viewer and stops polling until retried', async ({ page }) => {
    await mockAuth(page);
    // a fake clock lets the test move past several tail intervals without waiting
    await page.clock.install();
    let reads = 0;
    await page.route(`**/api/reactions/logs/${MOCK_GROUP}/${REACTION}/build**`, (route) => {
      reads += 1;
      return json(route, { error: 'Permission Denied' }, 401);
    });
    await page.goto(`/reaction/logs/${MOCK_GROUP}/${REACTION}/build`);
    await expect(page.getByTestId('stage-logs-error')).toContainText('Permission Denied');
    // no empty log viewer or paging buttons sit under the error
    await expect(page.getByTestId('stage-logs-box')).toHaveCount(0);
    await expect(page.getByTestId('stage-logs-nav-button')).toHaveCount(0);
    // the tail does not keep reading while the error is shown
    const settled = reads;
    await page.clock.runFor(60_000);
    expect(reads).toBe(settled);
    // retrying reads once more
    await page.getByTestId('stage-logs-retry').click();
    await expect.poll(() => reads).toBe(settled + 1);
  });

  test('a stage with no logs says so instead of showing an empty box', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/logs/${MOCK_GROUP}/${REACTION}/build**`, (route) => json(route, { logs: [] }));
    await page.goto(`/reaction/logs/${MOCK_GROUP}/${REACTION}/build`);
    await expect(page.getByTestId('stage-logs-empty')).toContainText('No logs have been recorded for the build stage');
    await expect(page.getByTestId('stage-logs-error')).toHaveCount(0);
    // the banner leaves a gap above the log viewer card instead of touching it
    const banner = await page.getByTestId('stage-logs-empty').boundingBox();
    const card = await page.getByTestId('stage-logs-box').boundingBox();
    expect(banner && card && card.y - (banner.y + banner.height)).toBeGreaterThanOrEqual(8);
  });

  test('an empty stage of a finished reaction drops "yet" and a stage it lacks is named as missing', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/${MOCK_GROUP}/${REACTION}`, (route) => json(route, makeReaction(REACTION, 'triage')));
    await page.route(`**/api/pipelines/data/${MOCK_GROUP}/triage`, (route) => json(route, { name: 'triage', order: ['build'] }));
    await page.route(`**/api/reactions/logs/${MOCK_GROUP}/${REACTION}/*`, (route) => json(route, { logs: [] }));
    await page.goto(`/reaction/logs/${MOCK_GROUP}/${REACTION}/build`);
    await expect(page.getByTestId('stage-logs-empty')).toContainText('No logs were recorded for the build stage of this reaction');
    await expect(page.getByTestId('stage-logs-empty')).not.toContainText('yet');
    await page.goto(`/reaction/logs/${MOCK_GROUP}/${REACTION}/nope`);
    await expect(page.getByTestId('stage-logs-empty')).toContainText('This reaction has no stage named nope. Its stages are: build.');
    // a stage that can never log has no empty viewer or paging buttons, and the reaction's stages link to their logs
    await expect(page.getByTestId('stage-logs-box')).toHaveCount(0);
    await expect(page.getByTestId('stage-logs-nav-button')).toHaveCount(0);
    await expect(page.getByTestId('stage-logs-stage-links').getByRole('link', { name: 'build' })).toHaveAttribute(
      'href',
      `/reaction/logs/${MOCK_GROUP}/${REACTION}/build`,
    );
    await expect(page.getByTestId('stage-logs-stage-links')).toContainText('Open logs for:');
    // following a stage link shows that stage's logs viewer instead of the unknown-stage message
    await page.getByTestId('stage-logs-stage-links').getByRole('link', { name: 'build' }).click();
    await expect(page.getByRole('heading', { name: 'Stage logs: build' })).toBeVisible();
    await expect(page.getByTestId('stage-logs-empty')).toContainText('No logs were recorded for the build stage of this reaction');
    await expect(page.getByTestId('stage-logs-box')).toHaveCount(1);
  });

  test('a long unknown stage name wraps inside its banner at 1024 px', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/${MOCK_GROUP}/${REACTION}`, (route) => json(route, makeReaction(REACTION, 'triage')));
    await page.route(`**/api/pipelines/data/${MOCK_GROUP}/triage`, (route) => json(route, { name: 'triage', order: ['build'] }));
    await page.route(`**/api/reactions/logs/${MOCK_GROUP}/${REACTION}/*`, (route) => json(route, { logs: [] }));
    await page.setViewportSize({ width: 1024, height: 768 });
    await page.goto(`/reaction/logs/${MOCK_GROUP}/${REACTION}/${'y'.repeat(140)}`);
    await expect(page.getByTestId('stage-logs-empty')).toContainText('This reaction has no stage named yyyy');
    // the unbreakable name stays inside the banner and the page does not scroll sideways
    const banner = page.getByTestId('stage-logs-empty').locator('> div').first();
    const [scroll, client] = await banner.evaluate((el) => [el.scrollWidth, el.clientWidth]);
    expect(scroll).toBeLessThanOrEqual(client);
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
    expect(overflow).toBeLessThanOrEqual(0);
  });

  test('a stage with logs shows the lines and no message', async ({ page }) => {
    await mockAuth(page);
    await page.route(`**/api/reactions/logs/${MOCK_GROUP}/${REACTION}/build**`, (route) => json(route, { logs: ['hello from build'] }));
    await page.goto(`/reaction/logs/${MOCK_GROUP}/${REACTION}/build`);
    await expect(page.getByText('hello from build')).toBeVisible();
    await expect(page.getByTestId('stage-logs-error')).toHaveCount(0);
    await expect(page.getByTestId('stage-logs-empty')).toHaveCount(0);
  });
});
