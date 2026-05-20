# Playwright E2E Testing Guide

This guide explains how to write, run, and debug Playwright tests for the Thorium UI. These tests drive a real Chromium browser against a running Thorium instance and capture screenshots for visual validation.

## Prerequisites

Before running tests you need:

1. **Thorium API** running on `localhost:8080` — start with `minithor/expose`
2. **Chromium** installed — run `npx playwright install chromium` once
3. **Vite dev server** — Playwright auto-starts it via `playwright.config.ts`, or reuse an existing one on port 8000

## Running Tests

```bash
cd ui

# Run all E2E tests
THORIUM_API_URL=http://localhost:8080 npm run test:e2e

# Run a single test by name
THORIUM_API_URL=http://localhost:8080 npx playwright test -g "seeded entity"

# Watch the browser (headed mode)
THORIUM_API_URL=http://localhost:8080 npm run test:e2e:headed

# Interactive UI debugger (timeline, DOM snapshots, network log)
THORIUM_API_URL=http://localhost:8080 npx playwright test --ui

# Record a trace for step-by-step replay
THORIUM_API_URL=http://localhost:8080 npx playwright test --trace on
npx playwright show-trace test-results/*/trace.zip
```

The `THORIUM_API_URL` env var tells the Playwright config where the API lives. It is mapped to `REACT_APP_API_URL` for the Vite dev server internally. Without it the frontend defaults to `http://localhost/api` (port 80) which won't reach the API.

## Test Structure

### File Layout

```
e2e/
  helpers.ts                 # Shared utilities: API client, auth (loginViaUI, setupMockAuth),
                             #   editor helpers (waitForEditor, setEditorContent), snapshot
  graph.spec.ts              # 3D graph visual validation (real API)
  data-manager.spec.ts       # Shared data manager / tree sync (real API)
  upload.spec.ts             # File upload flow (real API)
  upload-components.spec.ts  # Upload form components detailed testing (real API)
  sidebar.spec.ts            # Sidebar navigation (mock API)
  sigma-editor.spec.ts       # Sigma rule CodeMirror editor (mock API)
  yara-editor.spec.ts        # YARA rule CodeMirror editor (mock API)
  images.spec.ts             # Image browsing and create pages (mock API)
  pipelines.spec.ts          # Pipelines page (mock API)
  screenshots/               # Output directory for PNGs (gitignored)
    .gitkeep
```

### Test Lifecycle

Tests follow a **seed, navigate, screenshot, assert** pattern:

1. **`beforeAll`** — Seeds test data via direct API calls (no browser). Authenticates, creates entities/files/associations.
2. **Each test** — Gets a fresh browser page. Logs in via the UI, navigates to the target page, interacts with it, takes screenshots, runs assertions.
3. **`afterAll`** — Cleans up seeded data via API. Errors are swallowed so cleanup is best-effort.

### Authentication

Tests log in through the UI rather than setting cookies directly. This avoids issues with the `Secure` cookie flag over HTTP.

```ts
async function loginViaUI(page: Page) {
  await page.goto('/');
  await page.waitForLoadState('networkidle');
  await page.locator('input[placeholder="username"]').fill(USER);
  await page.locator('input[placeholder="password"]').fill(PASS);
  await page.locator('button:has-text("Login")').click();
  await page.waitForURL((url) => !url.pathname.includes('/auth'), { timeout: 15000 });
}
```

Default credentials for minithor dev deployments: `test` / `INSECURE_DEV_PASSWORD`. Override with `THORIUM_USER` and `THORIUM_PASS` env vars.

## Writing New Tests

### 1. Add API helpers if needed

`e2e/helpers.ts` provides reusable functions for API interaction. All calls go to `THORIUM_API_URL` (default `http://localhost:8080`).

Available helpers:

**API functions:**
- `authenticate(username, password)` — Returns a token string
- `createEntity(token, name, kind, groups)` — Returns entity UUID
- `deleteEntity(token, id)` — Deletes an entity
- `uploadFile(token, content, filename, groups)` — Returns `{ sha256, id }`, handles 409 (already exists)
- `createAssociation(token, request)` — Creates an association between entities/files
- `buildTree(token, seed)` — Builds a relationship tree, returns `{ id, initial, growable }`
- `snapshot(page, dir, name)` — Captures paired screenshot + HTML snapshot

**Auth helpers:**
- `loginViaUI(page)` — Logs in via the UI with `TEST_USER`/`TEST_PASS` credentials
- `setupMockAuth(page)` — Sets up mock auth routes (whoami, generic API fallback, cookie)
- `TEST_USER`, `TEST_PASS` — Default credentials (`test` / `INSECURE_DEV_PASSWORD`)
- `MOCK_USER` — Mock user object for route interception

**CodeMirror editor helpers:**
- `waitForEditor(page)` — Waits for `.cm-editor` to appear
- `waitForLinter(page)` — Waits for linter to process
- `setEditorContent(page, text)` — Replaces CodeMirror editor content programmatically

The `buildClient(token?)` function creates an axios instance with the correct base URL and auth header. Use it for any new API endpoints.

**Important details:**
- Auth header format: `Authorization: token <base64(token)>`
- Entity creation uses multipart form (`name`, `kind`, `groups` fields)
- File upload uses multipart form with field name `data` (not `file`)
- Associations use JSON body with `AssociationTarget` variants: `{ Entity: { id, name } }`, `{ File: "<sha256>" }`, `{ Repo: "<url>" }`
- Use group `system` or `static` (not `admins`) for dev deployments — check available groups with `GET /api/groups/`

### 2. Write the spec

Create a new file in `e2e/` named `<feature>.spec.ts`:

```ts
import { test, expect } from '@playwright/test';
import path from 'path';
import {
  authenticate, createEntity, deleteEntity, snapshot,
  loginViaUI, setupMockAuth, TEST_USER, TEST_PASS,
} from './helpers';

const SCREENSHOT_DIR = path.join(import.meta.dirname, 'screenshots');

test.describe('Feature Name', () => {
  let token: string;

  test.beforeAll(async () => {
    token = await authenticate(TEST_USER, TEST_PASS);
    // Seed test data via API
  });

  test.afterAll(async () => {
    // Clean up seeded data
  });

  test('renders correctly', async ({ page }) => {
    await loginViaUI(page);
    await page.goto('/your-page');
    await page.waitForLoadState('networkidle');

    // Interact with the page
    // ...

    await snapshot(page, SCREENSHOT_DIR, 'your-feature');

    // DOM assertions
    await expect(page.locator('.your-selector')).toBeVisible();
  });
});
```

### 3. Screenshot conventions

- Save all screenshots to `e2e/screenshots/` with descriptive names
- Use `fullPage: true` for full-page captures
- Use element-scoped `.screenshot()` for component-level captures:
  ```ts
  const card = page.locator('.card').filter({ hasText: 'Section Title' }).first();
  await card.screenshot({ path: path.join(SCREENSHOT_DIR, 'section.png') });
  ```
- Screenshots are gitignored (`e2e/screenshots/*.png`) — they are test artifacts, not committed

### 4. Waiting strategies

The UI uses lazy-loaded components and WebGL canvases that take time to render:

- `await page.waitForLoadState('networkidle')` — Wait for initial API calls to finish
- `await page.waitForSelector('canvas', { timeout: 30000 })` — Wait for WebGL canvas
- `await page.waitForTimeout(5000)` — Give force simulations time to settle (use sparingly)
- `await page.waitForURL(...)` — Wait for navigation (e.g., after login)

### 5. Common patterns

**Filling the graph seed input:**
```ts
const seedJson = JSON.stringify({ entities: [entityId] });
const input = page.locator('input.form-control').first();
await input.fill(seedJson);
```

**Opening the 3D graph toolbar controls:**
```ts
// Click gear icon to expand toolbar
const gearBtn = page.locator('button[title="Toggle controls"]');
await gearBtn.click();

// Click a toolbar section (Graph, Forces, Nodes, Edges, Export)
const graphBtn = page.locator('button[title="Graph"]');
await graphBtn.click();
```

**Capturing a specific card section:**
```ts
const card = page.locator('.card').filter({ hasText: 'Association Graph 3D' }).first();
await card.screenshot({ path: path.join(SCREENSHOT_DIR, 'graph-3d.png') });
```

## AI Agent Visual Validation

Claude Code can read screenshot PNGs natively via the `Read` tool. After running tests:

```
Read("ui/e2e/screenshots/graph-full-page.png")
```

This enables a closed-loop workflow:
1. Edit code
2. Run `npm run test:e2e`
3. Read screenshots — visually validate rendering
4. If something looks wrong, edit code and repeat

No MCP server or additional tooling is needed. The AI can evaluate semantic correctness ("are nodes connected by edges?", "is the controls panel open?") rather than pixel-level diffs.

## Troubleshooting

**API not reachable / socket hang up:**
The port-forward from `minithor/expose` may have died. Re-run it:
```bash
cd minithor && ./expose
```

**Login redirects instead of reaching the page:**
The `REACT_APP_API_URL` env var is missing. The frontend defaults to `http://localhost/api` (port 80), which doesn't reach the API on port 8080.

**Entity creation returns 404 with "groups must exist":**
The group name doesn't exist in this deployment. Check available groups:
```bash
curl -s http://localhost:8080/api/groups/ -H "Authorization: token $(echo -n '<token>' | base64)"
```
Dev deployments typically have `system` and `static` groups.

**File upload returns 409:**
The file already exists (same content was uploaded before). The `uploadFile` helper handles this by computing the SHA256 locally and returning it.

**Canvas never appears (timeout):**
- Check the browser console for JS errors (`--headed` mode or `--trace on`)
- Verify the 3D graph component is imported in the page (`GraphBuilder.tsx`)
- WebGL may fail in some headless configurations — Playwright's bundled Chromium supports it

## Configuration Reference

| File | Purpose |
|------|---------|
| `playwright.config.ts` | Test runner config: test dir, timeouts, base URL, dev server auto-start |
| `e2e/helpers.ts` | Shared test utilities: API client, auth helpers, editor helpers, snapshot |
| `e2e/*.spec.ts` | Test specs (see File Layout above for full inventory) |
| `e2e/screenshots/` | Screenshot output (gitignored) |

| Env Var | Default | Purpose |
|---------|---------|---------|
| `THORIUM_API_URL` | `http://localhost:8080` | API URL for test helpers and Vite dev server (mapped to `REACT_APP_API_URL` internally) |
| `THORIUM_USER` | `test` | Login username |
| `THORIUM_PASS` | `INSECURE_DEV_PASSWORD` | Login password |
