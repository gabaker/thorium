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

Always use these aliases in imports rather than relative paths.

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

- **Do not introduce new Bootstrap usage.** The codebase has legacy Bootstrap/SCSS from earlier development — do not add to it. New styling must use styled-components.
- There are legacy `.jsx` files in the codebase (see list below). When you touch these files, convert them to `.tsx` as part of the change.

### Skills

When writing new components or refactoring existing ones, load the Vercel engineering skills for guidance:

- Use `/react-best-practices` for React/Next.js performance patterns
- Use `/composition-patterns` for scalable component architecture (compound components, render props, context)

For Thorium-specific API interaction, instance discovery, and CLI usage, the base skill is at [skills/THORIUM.md](../skills/THORIUM.md).

New development skills go in `skills/developer/`, user workflow skills in `skills/user/`, and admin skills in `skills/admin/`.

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
