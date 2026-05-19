# Thorium UI Development Guide

> See the root [CLAUDE.md](../CLAUDE.md) for full project overview, build commands, dev environment setup, and backend architecture.

## Project Structure

The frontend is a React 19 + TypeScript SPA built with Vite.

### Source Layout

```
src/
  Thorium.tsx             # app root / router (config-driven entity routes)
  main.tsx                # entry point
  models/                 # TypeScript type definitions (API data shapes)
  thorpi/                 # API client layer (axios-based, one module per resource)
    client.ts             # axios instance with auth interceptor
  components/
    associations/         # graph visualizations (three.js 3D, tree, data context)
    entities/             # entity system (config-driven browsing, create, details)
      browsing/configs/   # per-type browsing configs (Device, Vendor, SigmaRule, etc.)
      create/configs/     # per-type create configs
      details/configs/    # per-type details configs
    pages/                # page-level layout (NavBanner, file/image sub-panels)
    shared/               # reusable UI primitives
      inputs/             # form inputs (code/CodeEditor, selectable, tags/TagSelect)
    tags/                 # tag display/edit components
    tools/                # analysis tool components
  pages/                  # route-level page components (one per route)
    entities/             # unified entity pages (EntityBrowsing, EntityCreate, EntityDetails)
    test/code/            # dev-only test pages (sigma, yara, image/pipeline editor)
  styles/                 # SCSS global styles + TS spacing/scaling utilities
  utilities/              # shared helpers (auth, fetch, sorting, rules, transforms)
  dashboards/             # dashboard views
```

### Path Aliases

Configured in both `vite.config.ts` and `tsconfig.base.json`:

| Alias | Path |
|-------|------|
| `@assets` | `src/assets` |
| `@components` | `src/components` |
| `@entities` | `src/components/entities` |
| `@models` | `src/models` |
| `@pages` | `src/pages` |
| `@styles` | `src/styles` |
| `@thorpi` | `src/thorpi` |
| `@utilities` | `src/utilities` |

Always use these aliases in imports from outside of the local component directory/subdirectory rather than relative paths.

### Where Code Goes

- **API types and interfaces** go in `src/models/`. Each resource has its own file (e.g., `files.ts`, `images.ts`, `pipelines.ts`). These mirror the shapes defined by the Rust API models in `api/src/models/`.
- **API client functions** go in `src/thorpi/`. One module per resource (e.g., `thorpi/files.ts` wraps `/api/files` endpoints). The client uses the shared axios instance in `thorpi/client.ts`.
- **Reusable components** go in `src/components/shared/`. Domain-specific components go in the appropriate `src/components/` subdirectory.
- **Route-level pages** go in `src/pages/`, organized by resource (e.g., `pages/images/`, `pages/files/`).
- **Utilities and helpers** go in `src/utilities/`.

### Backend Reference

When building or modifying API-connected features, reference the backend source:

- **API route handlers**: `api/src/routes/` (one file per resource — `files.rs`, `images.rs`, `pipelines.rs`, etc.)
- **API data models**: `api/src/models/` (canonical type definitions — `files.rs`, `images.rs`, `pipelines.rs`, etc.)
- **DB layer**: `api/src/models/backends/`

Frontend models in `src/models/` should stay aligned with the Rust types in `api/src/models/`.

## Style and Composition

### Technology Stack

All new code must be written in **TypeScript + React + styled-components**. No exceptions.

- **Do not add third-party UI component libraries without explicit user approval.** This includes Bootstrap, Material UI, Ant Design, Chakra UI, and any similar library. The codebase has legacy react-bootstrap usage from earlier development — do not add new usage of it.
- **Actively replace react-bootstrap components with custom styled-components** stored in `src/components/shared/`. When you touch a file that uses react-bootstrap components (`Accordion`, `Badge`, `Button`, `Modal`, `Row`, `Col`, etc.), replace them with custom equivalents as part of the change. Build shared replacements that can be reused across the codebase.
- **Bundle optimization for new dependencies.** When a new library is approved and added, register it in `ui/bundle/chunks.json` to control chunk splitting. Use `ui/bundle/generateBundleChunks.py` to compute transitive dependency closures and `ui/bundle/validateChunks.py` to verify chunk assignments. Large libraries should get their own chunk group to enable lazy loading; small utilities can go in the default vendors chunk. Always verify the production build output (`npm run build`) to check chunk sizes after adding dependencies.
- All source is now TypeScript — the legacy `.jsx` migration is complete. Write new code as `.tsx`/`.ts` only; do not introduce new `.jsx`/`.js` source files.

### TypeScript Conventions

- **Prefer `enum` over string union types.** Use string-valued enums (`enum Foo { Bar = 'bar' }`) instead of union types (`type Foo = 'bar' | 'baz'`). Enums provide better refactoring support, autocomplete, and exhaustiveness checking.
- **Every API resource must have equivalent TypeScript types.** All fields, enums, and nested structures from the Rust API models (`api/src/models/`) must have corresponding TypeScript definitions in `src/models/`. When adding or modifying API-connected features, verify that the frontend types match the backend schema — add missing types rather than using inline literals, `string`, or `any`. Preserve the same doc comments and structural grouping as the Rust source (additional TypeScript-specific comments are fine). TypeScript types don't need to exactly replicate Rust struct layout since the languages have different capabilities, but always flag structural differences to the user so they can determine if the divergence is intentional.
- **API and model types** (`interface`, `type`, `enum`) that represent API data shapes or are shared across multiple components go in `src/models/`. Each API resource has its own file.
- **Internal component types** (`interface`, `type`, `enum`) that are only used for local state, controls, or display logic should be colocated with the component(s) that depend on them — typically in a `types.ts` file next to the component.

### Imports

Separate imports into two sections: **library imports** first, then **project imports** preceded by a `// project imports` comment. Within each section, sort named imports from a single module alphabetically.

Order project imports from most local to most global:
1. Local relative paths (`./types`, `./Toolbar.styled`, `../shared/scaling`)
2. `@components/`
3. `@thorpi/`, `@utilities/`
4. `@models/`

```ts
import React, { useCallback, useState } from 'react';
import styled from 'styled-components';

// project imports
import { GraphNode } from './types';
import { ToolbarContainer } from './Toolbar.styled';
import { OverlayTipTop } from '@components/shared/overlay/tips';
import { fetchImages } from '@utilities/fetch';
import { NodeType } from '@models/trees';
```

### Styling

Styling follows a hierarchy based on scope and purpose:

1. **Theme variables and library overrides** go in `src/styles/` (SCSS). This includes color variables, theme definitions, and overrides for third-party library components (Bootstrap, react-bootstrap) and built-in HTML elements. This is the only place global styles belong.
2. **Component styles use styled-components.** All new component styling must use styled-components, colocated with the component (in the same file or a companion `.styled.tsx`). Avoid creating new SCSS files or adding to existing ones outside `src/styles/`.
3. **Avoid styled-components when CSS variation would cause very large stylesheets** — for example, highly dynamic styles with many prop-driven variants. In these cases, use inline styles or CSS custom properties.
4. **Inline styles** should be used sparingly: only when a single property (or very small number) must be adjusted on an existing component. If you need more than 2-3 inline style properties, create a styled wrapper instead.

### Code Quality

- **Write clean, DRY code.** Extract shared logic into reusable components, hooks, and utilities. Deduplicate repeated patterns. Avoid copy-pasting code between components — factor out the common parts.
- **Before creating anything new, search for existing implementations.** The codebase already has shared components, styled-components, hooks, utility functions, API client calls, type definitions, constants, validation helpers, and transform functions. Before writing a new one, check whether an equivalent already exists in `src/components/shared/`, `src/utilities/`, `src/thorpi/`, `src/models/`, or colocated with related components (e.g., `shared.styled.tsx`, `types.ts`). This applies to all reusable patterns: UI primitives, styled wrappers, data fetching, error handling, form validation, sorting/filtering helpers, and theme variables. If an existing implementation is close but not exact, extend or wrap it rather than duplicating.
- **Comments should explain *why*, not *what*.** The code shows what it does; comments capture intent, rationale, tradeoffs, and non-obvious constraints (edge cases, workarounds, why an approach was chosen over an alternative).
  - **Add a one-line purpose comment** to functions whose role isn't obvious from the name and signature. Skip it when the name already says everything — don't restate the code.
  - **Comment non-obvious sections within functions** to group related logic and flag subtle behavior.
  - **Never let comments restate the code** (`// increment i`) or drift out of sync. A stale or redundant comment is worse than none — update or delete comments when you change the code they describe.
  - **Keep inline comments concise** — one line is usually enough.
- **Document functions with JSDoc doc comments.** Every exported function — and **especially `@thorpi` API client functions and `@utilities` helpers** — must have a full JSDoc block (`/** ... */`) describing what it does, each parameter (`@param`), the return value (`@returns`), and any thrown errors (`@throws`) or notable side effects. Document non-obvious type parameters with `@template`. The JSDoc describes the contract (what/inputs/outputs); inline `//` comments still cover the *why* per the rules above.

  ```ts
  /**
   * Fetch a single image by name, scoped to a group.
   *
   * @param group - The group the image belongs to.
   * @param name - The image name to fetch.
   * @param errorHandler - Called with a formatted message if the request fails.
   * @returns The image, or `null` if not found or the request failed.
   */
  export const getImage = async (
    group: string,
    name: string,
    errorHandler: (error: string) => void,
  ): Promise<Image | null> => { ... };
  ```

### Skills

When writing new components, refactoring existing ones, or reviewing code, load the Vercel engineering skills (if available):

- Use `/react-best-practices` for React/Next.js performance patterns
- Use `/composition-patterns` for scalable component architecture (compound components, context providers, explicit variants, children over render props, boolean prop avoidance)

These skills should be referenced during code reviews to verify that new or modified components follow composition best practices. In particular, flag boolean prop proliferation, monolithic components with conditional rendering, and state trapped inside components that should be lifted to providers.

For Thorium-specific API interaction, instance discovery, and CLI usage, the base skill is at [skills/THORIUM.md](../skills/THORIUM.md).

New development skills go in `skills/developer/`, user workflow skills in `skills/user/`, and admin skills in `skills/admin/`.

### Naming Conventions

**Files:**
- **Standalone component files**: PascalCase matching the component name (`AlertBanner.tsx`, `EntityBrowsing.tsx`).
- **Combined files** containing styled-components and small related visual primitives: lowercase (`shared.tsx`, `types.ts`, `config.ts`).
- **Styled-component companion files**: `.styled.tsx` suffix (`Toolbar.styled.tsx`).
- **Config files** follow `{EntityName}{View}Config.tsx` (`DeviceBrowsingConfig.tsx`, `CollectionDetailsConfig.tsx`).
- **Test files**: co-located with source using `*.test.ts` / `*.test.tsx`.

**Code:**
- **Components**: PascalCase (`AlertBanner`, `EntityBrowsing`).
- **Styled-components**: PascalCase, descriptive (`BrowsingCard`, `LinkFields`, `ControlRow`). Use `$` prefix for transient props (`$bold`, `$active`).
- **Constants**: UPPER_SNAKE_CASE for true constants (`NODE_COLORS`, `ICON_EDGE_PAD`). PascalCase for blank/default model constants (`BlankDevice`, `BlankCreateDevice`).
- **Hooks**: `use` prefix, camelCase (`useAuth`, `useGraphData`).
- **Thorpi functions**: CRUD verb prefix (`createEntity`, `getFile`, `updateImage`, `deleteReaction`, `listPipelines`).
- **Model types**: bare name for API response (`Device`), `Create` prefix for create requests (`CreateDevice`), `{Entity}MetaFields` / `{Entity}CreateMetaFields` for metadata subtypes.

## Patterns and Architecture

### Resource Data Models

Thorium resources (files, images, entities, etc.) often have **separate models for each operation**. A resource's API response shape, create request, and update request may all differ. Components reference the model matching their action:

- **Response/display types** mirror the API response (e.g., `Device`, `Image`, `Pipeline`).
- **Create types** mirror the create request body (e.g., `CreateDevice`, `ImageCreate`). These often use IDs (`string[]`) where response types embed full objects.
- **Blank constants** (`BlankDevice`, `BlankCreateDevice`) provide empty defaults for initializing forms.

When building components, use the type that matches the operation — don't reuse a response type for a create form or vice versa.

### Entity System

The entity system is **config-driven**: browsing, create, and details pages are generated from configuration objects. Three registries drive the three views:

- **Browsing configs** (`components/entities/browsing/configs/`): `EntityBrowseConfig<T>` with `renderEntity`, `entityHeaders`, `fetchEntities`.
- **Details configs** (`components/entities/details/configs/`): `EntityDetailsConfig<T>` with `getEntityDetails`, `EntityMetaInfo` component, `BlankEntity`.
- **Create configs** (`components/entities/create/configs/`): `EntityCreateConfig<K>` with `EntityMetadata` form component, `BlankCreateEntity`.

Factory functions (`createEntityBrowsingPage`, `createEntityDetailsPage`, `createEntityCreatePage`) produce React components from these configs. The factory handles common UI (name, groups, description, tags, buttons) while the config provides type-specific content.

**Adding a new entity type:**
1. Add the variant to the `Entities` enum in `src/models/entities/entities.ts`
2. Create a model file in `src/models/entities/` with response type, create type, meta types, and blank constants
3. Add re-exports in `src/models/entities/index.ts` and update union types in `entities.ts`
4. Create browsing, details, and (optionally) create configs in their respective `configs/` directories
5. Register configs in each config registry (`config.ts` / `configs.ts`)
6. Add route entries in `EntityBrowsingRoutes`, `EntityDetailsRoutes`, and optionally `EntityCreateRoutes`
7. Add a nav item in `src/components/pages/navConfig.ts`

Routes are generated dynamically from these route maps — `Thorium.tsx` does not need to be modified.

### API Client (thorpi)

Every thorpi module follows the same pattern:

```ts
export const createThing = async (
  data: FormData,
  errorHandler: (error: string) => void,
): Promise<ThingResponse | null> => {
  return client
    .post('/things/', data)
    .then((res) => {
      if (res?.status && res.status == 200 && res.data) {
        return res.data;
      }
      return null;
    })
    .catch((error) => {
      parseRequestError(error, errorHandler, 'Create Thing');
      return null;
    });
};
```

Key rules:
- Every function takes an `errorHandler` callback — never throws.
- On failure, returns `null` (or `false` / empty array) — callers check the return value, not try/catch.
- Uses `parseRequestError(error, errorHandler, 'Label')` for consistent error formatting.
- Two axios instances: `client` (default) for standard JSON, `bigIntClient` for responses with BigInt values.

`src/utilities/fetch.ts` contains higher-level wrappers that orchestrate thorpi calls with loading state management.

### Routing and Auth

**Route structure** in `Thorium.tsx`:
- `BrowserRouter` > `Auth` context > `WindowManager` > `Site` (nav + sidebar + routes).
- Entity routes are generated dynamically from route maps (no hardcoded entity paths).
- All other routes are `React.lazy()` loaded with dynamic imports.

**Auth gating** via `PageWrapper` (`components/pages/Page.tsx`):
- `auth={true}` (default): wraps in `RequireAuth` — redirects to `/auth` if no token.
- `admin={true}`: wraps in `RequireAuth` + `RequireAdmin` — redirects if not admin.
- `auth={false}`: no protection (login page only).

**Page-level components must use `React.lazy()`** with dynamic imports for code splitting. This is required for all route-level pages in `src/pages/`.

### State Management

- **React Context** for cross-cutting state shared across component trees: `Auth` (app-wide), `GraphDataContext` (per graph), `UploadContext` (per upload flow). Every context follows: `createContext<T | undefined>(undefined)` + a `useXxx()` hook that throws if undefined + a `XxxProvider` component.
- **`useReducer`** only for complex imperative state machines (e.g., 3D graph controls that must synchronize with the ForceGraph3D imperative API).
- **`useState`** for everything else — page state, form state, loading flags, errors.

### Shared Components

Reuse these existing components instead of creating new ones:

| Component | Location | Purpose |
|-----------|----------|---------|
| `AlertBanner` | `shared/alerts/AlertBanner.tsx` | Themed alert with `Severity` enum (Error, Warning, Info, Success) |
| `OverlayTipTop/Right/Bottom/Left` | `shared/overlay/tips.tsx` | Tooltip wrappers — preferred over raw OverlayTrigger |
| `FieldBadge` | `shared/badges/FieldBadge.tsx` | Display badge for entity fields (arrays, objects, booleans) |
| `LinkBadge` | `shared/badges/LinkBadge.tsx` | Clickable badge for URLs with external link confirmation |
| `LoadingSpinner` | `shared/fallback/LoadingSpinner.tsx` | Bootstrap spinner with `loading` prop |
| `SelectInput` | `shared/inputs/selectable/SelectInput.tsx` | Single-value creatable select (react-select) |
| `SelectInputArray` | `shared/inputs/selectable/SelectInputArray.tsx` | Multi-value select with `valuesMap` for ID-to-label |
| `ScrollableSelect` | `shared/inputs/ScrollableSelect.tsx` | Compact numeric select with scroll |
| `TagSelect` | `shared/inputs/tags/TagSelect/TagSelect.tsx` | Tag key-value entry with autocomplete |
| `CodeEditor` | `shared/inputs/code/CodeEditor/CodeEditor.tsx` | CodeMirror editor with yara/sigma support |
| `Card` | `shared/Card.tsx` | Themed card wrapper (`panel` prop) |
| `Time` | `shared/Time.tsx` | Date/time formatter (`verbose` prop for long format) |
| `UploadDropzone` | `shared/UploadDropzone.tsx` | File drag-and-drop (react-dropzone) |
| `InfoHeader` / `InfoValue` | `entities/shared/` | Label + value columns for entity detail rows |
| Browsing primitives | `entities/browsing/shared.tsx` | `BrowsingCard`, `LinkFields`, `EntityName`, `EntityGroups` |

### Theme Variables

Themes (`Dark`, `Light`, `Ocean`, `Crab`) are defined in `src/styles/colors.scss`. The active theme is set via a `[theme]` attribute on the root element, toggled by the auth context from `userInfo.settings.theme`. Always use `--thorium-*` CSS variables for theme-aware colors:

| Variable | Purpose |
|----------|---------|
| `--thorium-body-bg` | Page background |
| `--thorium-text` | Primary text |
| `--thorium-secondary-text` | Muted text |
| `--thorium-highlight-text` | Highlighted/accent text |
| `--thorium-link-text` | Link color |
| `--thorium-panel-bg` | Panel/card background |
| `--thorium-secondary-panel-bg` | Form field/secondary panel background |
| `--thorium-highlight-panel-bg` | Hover/highlight background |
| `--thorium-nav-panel-bg` | Navigation background |
| `--thorium-panel-border` | Panel border |
| `--thorium-highlight-panel-border` | Highlighted border |
| `--thorium-danger-bg` | Danger/delete |
| `--thorium-error-bg` | Error alert background |
| `--thorium-warning-bg` / `--thorium-warning-secondary-bg` | Warning colors |
| `--thorium-info-bg` / `--thorium-info-secondary-bg` | Info colors |
| `--thorium-ok-bg` | Success color |
| `--thorium-button-text` | Button text |

TypeScript spacing/scaling utilities are exported from `src/styles/index.ts` (`scaling` enum for breakpoints, `spacers` enum for spacing values).

### Legacy JSX Files

The `.jsx` → `.tsx` migration is complete — there are no remaining `.jsx`/`.js` source files in `src/`. Keep it that way: write all new code as `.tsx`/`.ts`.

## Build Verification

After writing or refactoring code, always verify the build succeeds:

```bash
npm run build              # production build (tsc + vite, uses tsconfig.prod.json)
npm run build-check        # stricter type-check build (tsconfig.json, checkJs: true)
```

`npm run build` is the standard check. `npm run build-check` runs `tsc` with the stricter `tsconfig.json` (`checkJs: true`) — use it when you want the most thorough type validation.

## Linting

Run the linter after every change and fix all issues in new code:

```bash
npm run lint               # eslint check
npm run fix                # eslint --fix (auto-fixable issues)
npm run validate-style     # prettier check
npm run format             # prettier write
```

When modifying files that have pre-existing lint issues, fix those issues in the same change. Do not leave a file worse than you found it.

## Testing

### Unit Tests (Vitest)

Unit tests live alongside source files using the `*.test.ts` / `*.test.tsx` pattern. Config is in `vitest.config.ts`.

```bash
npm run test               # single run
npm run test:watch         # watch mode
```

#### What to unit test

Unit test **pure logic** that can run without a browser DOM:

- **Reducers** (`useReducer` state machines) — test every action type, including edge cases like unknown actions returning state unchanged.
- **Validation functions** — input validation, form building, origin validation.
- **Transform functions** — data conversion between API shapes and UI shapes (e.g., `utilities/transforms/`).
- **Rule checkers** — sigma, yara, and image rule validation (e.g., `utilities/rules/`).
- **Utility helpers** — sorting, formatting, tag manipulation, any pure function in `utilities/`.

Do **not** unit test React components or hooks that depend on DOM context — those are covered by E2E tests.

#### Testability: export pure logic

Reducers and pure helper functions defined inside hook files should be **exported** so they can be tested directly. If a function is pure (no side effects, no hooks), export it from the module even if it's only used internally by the hook.

```ts
// Good — reducer is exported for direct testing
export function uploadReducer(state: UploadState, action: UploadAction): UploadState { ... }

export function useFileUpload() {
  const [state, dispatch] = useReducer(uploadReducer, DEFAULT_UPLOAD_STATE);
  ...
}
```

#### `@thorpi` modules in unit tests

`@thorpi/client.ts` guards its `window`/`document` access so it can be imported safely in Vitest's `node` environment. Additionally, `vitest.setup.ts` auto-mocks `@thorpi/client` with stub axios instances so all `@thorpi/*` modules resolve without making real HTTP calls.

This means you **do not need per-file `vi.mock()` calls** for `@thorpi` modules. Just import the module under test directly:

```ts
import { describe, it, expect } from 'vitest';

// project imports — no vi.mock() needed for @thorpi
import { uploadReducer } from './useFileUpload';
```

If you need to control what a thorpi function returns in a specific test, mock the individual thorpi module in that test file:

```ts
vi.mock('@thorpi/files', () => ({
  uploadFile: vi.fn().mockResolvedValue({ sha256: 'abc' }),
}));
```

#### Keeping `@thorpi/client.ts` safe for node

`client.ts` uses `typeof window === 'undefined'` and `typeof document === 'undefined'` guards so it doesn't crash outside a browser. If you modify `client.ts`, do not add bare `window.*` or `document.*` access at the module level — always guard with `typeof` checks.

#### Unit test file structure

```ts
import { describe, it, expect } from 'vitest';

// project imports
import { myFunction } from './myModule';
import { DEFAULT_STATE } from './types';

// Helper to create state variants without repetition
function stateWith(patch: Partial<MyState>): MyState {
  return { ...structuredClone(DEFAULT_STATE), ...patch };
}

describe('myFunction', () => {
  it('handles the base case', () => {
    const result = myFunction(DEFAULT_STATE);
    expect(result).toEqual(expectedValue);
  });
});
```

### E2E Tests (Playwright)

E2E tests live in `e2e/` and run against a live Thorium instance. They require `THORIUM_API_URL=http://localhost:8080`. See the full guide: [e2e/TESTING.md](e2e/TESTING.md).

```bash
THORIUM_API_URL=http://localhost:8080 npm run test:e2e           # headless
THORIUM_API_URL=http://localhost:8080 npm run test:e2e:headed    # visible browser
```

Existing E2E specs: `graph`, `upload`, `sidebar`, `sigma-editor`, `yara-editor`, `data-manager`, `upload-components`, `images`, `pipelines`.

### Unit vs E2E: when to use which

| Scenario | Test type | Why |
|----------|-----------|-----|
| Pure function, reducer, validation logic | **Unit** | Fast, isolated, no infra needed |
| Form rendering, field visibility, tab switching | **E2E** | Needs real DOM and component tree |
| API integration (upload, create, delete) | **E2E (live)** | Verifies real API contract |
| Component layout, navigation, visual state | **E2E (mock or live)** | Needs browser rendering |
| Data transforms between API and UI shapes | **Unit** | Pure input/output, no DOM |
| Error handling / validation feedback in UI | **E2E** | Needs rendered error banners, alerts |

When adding a new feature, write **both**: unit tests for the pure logic layer, E2E tests for the rendered behavior.

### Testing Rules

1. **All new pages require E2E tests.** No exceptions.
2. **All new pure logic requires unit tests.** Reducers, validators, transforms, and utility functions must have colocated `*.test.ts` files.
3. **Before writing any test**, check what tests already exist for the feature. Do not duplicate test code — extend existing specs when possible.
4. **After modifying a feature**, update both unit tests and E2E tests so they stay current. Stale tests are worse than no tests.
5. **Run existing tests** before and after your change to confirm nothing breaks:
   ```bash
   npm run test
   THORIUM_API_URL=http://localhost:8080 npm run test:e2e
   ```

## Development Process

Follow this workflow for non-trivial UI work (new components, pages, features, or significant modifications):

### 1. Spec

Write or update a spec as a markdown file in the target component or page directory (e.g., `src/pages/images/SPEC.md`, `src/components/shared/buttons/SPEC.md`). The spec should define:

- **Purpose** — what the component/feature does and why
- **Requirements** — functional behavior, inputs/outputs, conditionals (scaler type, user role, component state such as view/edit/create modes)
- **Visual layout** — ASCII diagrams, section ordering, responsive breakpoints
- **Data dependencies** — API resources, models, transforms needed
- **Edge cases** — empty states, error states, loading states, permission gates

For modifications to existing components, update the existing spec rather than creating a new one. The spec is the source of truth for intended behavior.

Every source file that implements a spec must include a reference to its spec file at the top of the file, immediately after the imports:

```ts
// spec: ./SPEC.md
```

### 2. Plan

Develop an implementation plan from the spec:

- Identify files to create or modify
- Check for existing components, utilities, and patterns to reuse
- Note any new types, API calls, or shared components needed
- Consider composition patterns (load `/composition-patterns` and `/react-best-practices` skills if available)
- Flag any architectural decisions that need user input

### 3. Implement

Execute the plan:

- Follow all conventions in this document (TypeScript, styled-components, imports, naming)
- Build incrementally — verify the build compiles after each major change
- Run linting and fix issues as you go

### 4. Test

Write and run tests:

- Unit tests for pure logic (reducers, validators, transforms)
- E2E tests for rendered behavior (layout, interactions, API integration)
- Run existing tests to confirm no regressions

### 5. Validate

Compare the implementation against the spec:

- Verify each spec requirement is met by the implementation and covered by tests
- Update the spec if the implementation revealed requirements that changed during development
- Flag any spec requirements that could not be met and why
