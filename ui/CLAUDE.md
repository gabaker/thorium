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

- **Do not introduce new Bootstrap usage.** The codebase has legacy Bootstrap/SCSS from earlier development — do not add to it. Prefer custom styled-components over Bootstrap components for new UI.
- There are legacy `.jsx` files in the codebase (see list below). When you touch these files, convert them to `.tsx` as part of the change unless the user explicitly states otherwise.

### TypeScript Conventions

- **Prefer `enum` over string union types.** Use string-valued enums (`enum Foo { Bar = 'bar' }`) instead of union types (`type Foo = 'bar' | 'baz'`). Enums provide better refactoring support, autocomplete, and exhaustiveness checking.
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

- **Prefer custom components over Bootstrap components** for new UI. Wrap or replace Bootstrap usage with styled-components when modifying existing code.
- **Write clean, DRY code.** Extract shared logic into reusable components, hooks, and utilities. Deduplicate repeated patterns. Avoid copy-pasting code between components — factor out the common parts.
- **Comment sections within functions** that define related behavior. Always add a brief comment to functions explaining their purpose. Keep comments concise — one line is ideal.

### Skills

When writing new components or refactoring existing ones, load the Vercel engineering skills for guidance:

- Use `/react-best-practices` for React/Next.js performance patterns
- Use `/composition-patterns` for scalable component architecture (compound components, render props, context)

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

These files have not yet been converted to TypeScript. When modifying any of them, convert to `.tsx`:

- `src/pages/reactions/ReactionStageLogs.jsx`, `ReactionStatus.jsx`
- `src/pages/system/SystemSettings.jsx`
- `src/pages/users/Groups.jsx`
- `src/components/entities/details/override_pages/FileDetails.jsx`
- `src/components/pages/files/Comments.jsx`, `Download.jsx`, `Results.jsx`
- `src/components/pages/files/reactions/*.jsx`
- `src/components/pages/images/Arguments.jsx`, `Dependencies.jsx`, `EnvironmentVariables.jsx`, `Fields.jsx`, `NetworkPolicies.jsx`, `OutputCollection.jsx`, `Resources.jsx`, `SecurityContext.jsx`, `Volumes.jsx`
- `src/components/shared/inputs/selectable/SelectableArray.jsx`, `SelectableDictionary.jsx`
- `src/components/tags/EditableTags.jsx`

## Build Verification

After writing or refactoring code, always verify the build succeeds:

```bash
npm run build              # production build (tsc + vite)
npm run build-check        # full type-check including .jsx checkJs — use this when touching JSX files
```

When converting `.jsx` to `.tsx`, `npm run build` (which uses `tsconfig.prod.json` without `checkJs`) is the correct check. Use `npm run build-check` to additionally validate remaining `.jsx` files.

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

### E2E Tests (Playwright)

E2E tests live in `e2e/` and run against a live Thorium instance. They require `THORIUM_API_URL=http://localhost:8080`. See the full guide: [e2e/TESTING.md](e2e/TESTING.md).

```bash
THORIUM_API_URL=http://localhost:8080 npm run test:e2e           # headless
THORIUM_API_URL=http://localhost:8080 npm run test:e2e:headed    # visible browser
```

Existing E2E specs: `graph`, `upload`, `sidebar`, `sigma-editor`, `yara-editor`, `data-manager`, `upload-components`, `images`, `pipelines`.

### Testing Rules

1. **All new pages require E2E tests.** No exceptions.
2. **Before writing any test**, check what tests already exist for the feature. Do not duplicate test code — extend existing specs when possible.
3. **After modifying a feature**, update both unit tests and E2E tests so they stay current. Stale tests are worse than no tests.
4. **Run existing tests** before and after your change to confirm nothing breaks:
   ```bash
   npm run test
   THORIUM_API_URL=http://localhost:8080 npm run test:e2e
   ```
