# UI Code Review — `ui/src`

**Date:** 2026-06-01
**Branch reviewed:** `PipelineImageCodeEditor`
**Scope:** ~49k LOC across 405 files (`ui/src/**`, 178 `.ts` / 227 `.tsx`)
**Lenses:** TypeScript correctness, React 19 best practices (`/react-best-practices`), composition patterns (`/composition-patterns`), and project conventions (`ui/CLAUDE.md`).

**Legend**
- `✓` — verified against the source during review.
- `⚑` — surfaced by a review pass; recommend a quick confirm before fixing.
- Checkboxes `- [ ]` are for tracking remediation.

**Overall:** The codebase is two-tiered. Newer code (config-driven entity system, 3D graph + `GraphDataContext`, `shared/buttons|Panel|windows|pipeline`, `TagSelect`, `IncidentSummary`) is well-architected — enums, compound components, disciplined effect cleanup, lifted state, tested pure logic. The older surface (`pages/users/Groups`, `pages/reactions/*`, `pages/*Browsing`, `tools/displays/*`, `inputs/selectable/*`) holds nearly all the real bugs and the heavy legacy `react-bootstrap`.

---

## Triage summary

| Severity | Count | Theme |
|----------|-------|-------|
| 🔴 High bug | 2 | Validation bypass; user-facing data loss |
| 🟠 Medium bug | 14 | Logic errors, state mutation in render, blob leaks, remounts |
| 🟡 Low bug | 7 | Cosmetic / latent / convention |
| 🏗️ Composition | 10 | Boolean proliferation, monoliths, duplication, context shape |
| 🎨 Style | 7 | react-bootstrap, enums, `any`, `==`, doc drift |
| ✅ Non-issue | 2 | False positives caught during verification |

---

## 🔴 Bugs — High (fix first)

- [x] **`hasInvalidTags` never validates** ✓ — `utilities/tags.ts:147` — _fixed 2026-06-01_
  `tags.forEach(t => { if (tagIsInvalid(t)) return true; })` — the `return` exits the callback, so the function **always returns `false`**. This is the upload validation gate (`components/pages/files/upload/originValidation.ts`), so half-empty tags bypass validation. The unit test masks it by mocking the function.
  **Fix:** `return tags.some((tag) => tagIsInvalid(tag, ignore_empty));`

- [x] **Pipeline selector collapses to one pipeline per group** ✓ — `components/pages/files/reactions/SelectPipelines.tsx:44` (and `:55`) — _fixed 2026-06-01_
  `selectablePipelines` is keyed by **group**, but the guard tests `if (pipeline.name in selectablePipelines)`. A pipeline name is never a group key, so every pipeline hits the `else` branch and overwrites `selectablePipelines[group]` with a single-entry object — **only the last pipeline per group survives**. Line 55 also writes a malformed composite key `` `${pipeline.pipeline}_${pipeline.group}` `` behind an `as unknown as` cast, so pre-selected pipelines never render.
  **Fix:**
  ```ts
  if (selectablePipelines[pipeline.group]) selectablePipelines[pipeline.group][pipeline.name] = false;
  else selectablePipelines[pipeline.group] = { [pipeline.name]: false };
  // and line 55:
  selectablePipelines[pipeline.group][pipeline.pipeline] = true;
  ```

---

## 🟠 Bugs — Medium

- [x] **Direct monitors use wrong guard/field** ✓ — `pages/users/Groups.tsx:398` — _fixed 2026-06-01_
  Guards on `'metagroups' in group.monitors` but returns `group.monitors.direct` (copy-paste from line 397).
  **Fix:** guard on `'direct' in group.monitors`.

- [ ] **Country names truncated to 6 chars** ✓ — `components/entities/details/configs/VendorDetailsConfig.tsx:23`
  `country.slice(0, namePrefix.length)` returns the **first 6 chars** instead of stripping the 6-char `' (the)'` suffix → "Bahamas (the)" becomes "Bahama".
  **Fix:** `country.slice(0, -namePrefix.length)`.

- [x] **Missing `return` → unknown node types render nothing** ✓ — `components/associations/shared/NodeInfo.tsx:249` — _fixed 2026-06-01_
  Final `else` branch is a bare JSX expression statement with no `return` (React throws "nothing was returned from render").
  **Fix:** add `return`.

- [x] **Duplicate DOM `id`** ✓ — `components/entities/details/configs/CollectionDetailsConfig.tsx:77,94` — _fixed 2026-06-01_
  Both `Form.Check` switches use `id="case-insensitive-toggle"` → invalid HTML; label/`htmlFor` hits the wrong control.
  **Fix:** give "Ignore Groups" a distinct id (e.g. `ignore-groups-toggle`).

- [ ] **`.sort()` mutates state array in render** ✓ `pages/users/UserBrowsing.tsx:338` · ⚑ `pages/pipelines/PipelineBrowsing.tsx:147`
  Sorting the `useState` array in place during render mutates state.
  **Fix:** `[...users].sort(...)` / `[...pipelines].sort(...)`.

- [ ] **Reducer has no `default` case** ✓ — `components/associations/graph/controls/controlsReducer.ts:448`
  An unmodeled action returns `undefined` and wipes `GraphControls`. CLAUDE.md explicitly wants unknown actions to return state unchanged.
  **Fix:** `default: return state;`.

- [ ] **Blob URL leak (no `revokeObjectURL`)** ✓ `entities/details/EntityDetails.tsx:378` · ⚑ `entities/shared/EntityGraphicUpload.tsx:134` · ⚑ `tools/displays/Image.tsx:39`
  `URL.createObjectURL(...)` allocated per render/file and never revoked.
  **Fix:** create in `useEffect`/`useMemo`, revoke the previous value in cleanup.

- [ ] **Sub-components defined inside a component → remount each render** ⚑ — `pages/users/Groups.tsx`
  `Groups` is a plain component (not a factory), so `LeaveGroupButton`/`DeleteGroupButton`/`UpdateGroupButton`/`GroupInfo`/`ModifyGroupButtons`/`CreateGroup` defined in its body remount on every render, dropping modal state.
  **Fix:** hoist to module scope, pass props.

- [ ] **More components-inside-components** ⚑ — `tools/SafeHtml.tsx:21` (`SanitizeHTML`), `components/pages/files/Comments.tsx:79+` (`CommentList`, `CommentAlertBanner`)
  **Fix:** hoist to module scope.

- [ ] **`Pipeline.order` typed as a one-element tuple** ✓ — `models/pipelines.ts:49`
  `order: [string[]]` types a tuple of exactly one `string[]`; real order is a list of stages (`['a', ['b','c']]`) — contradicts the transform tests.
  **Fix:** `order: (string | string[])[]` (confirm against Rust `Pipeline.order`).

- [ ] **Mutates prop arrays + reads stale state** ⚑ — `tools/displays/String.tsx:30`
  `errors.push(...)`/`warnings.push(...)` mutate prop arrays and read state set in the same tick (one render stale); dep array omits the props.
  **Fix:** derive `[...errors, ...parsedErrors]` during render; don't mutate props.

- [ ] **`logout()` can throw into the auth context** ⚑ — `thorpi/users.ts:111`
  No `.catch`; returns the raw axios promise; `auth.tsx` `revoke()` awaits it with no try/catch.
  **Fix:** catch, return boolean (follow thorpi convention).

- [ ] **Unguarded `image[0]` dereference** ⚑ — `components/pages/files/Results.tsx:90`
  Assumes every `Output[]` is non-empty → throws on empty results.
  **Fix:** guard `image?.[0]`.

- [ ] **`fetchImages` error path is dead/clobbered** ⚑ — `utilities/fetch.ts:43`
  On a group failure, `setImages([])` runs then the loop continues and overwrites it; partial results show despite the failure.
  **Fix:** `return`/flag after the error set.

---

## 🟡 Bugs — Low / latent

- [ ] `thorpi/images.ts:38` ✓ — `getImage` ignores the thorpi convention: no `errorHandler` param, hardcodes `console.log`.
- [ ] `tools/ToolResult.tsx:120` ✓ — files badge counts `result.files.length` but pluralizes on `result.children.length` → "1 Files".
- [ ] `shared/badges/FieldBadge.tsx:66` ✓ — null check tests `field != null` (always truthy here) instead of `item != null`. Dead guard / wrong variable.
- [ ] `entities/details/configs/DeviceDetailsConfig.tsx:63` ✓ — `onCreate={() => console.log('...vendor: ${}')}` (single-quoted literal `${}`, debug stub on a real callback); line 71 `LinkBadge` in `.map()` has **no `key`**.
- [ ] `utilities/tags.ts:32` ✓ — `saveTagCountToLocalStorage` computes `copy` (ATT&CK/MBC stripped) but stores `tagCounts: newTagCounts` (unstripped); the documented optimization is defeated.
- [ ] `inputs/selectable/SelectableArray.tsx:82`, `SelectableDictionary.tsx:119` ⚑ — `key={index}` on editable insert/delete lists → inputs keep wrong value/focus after a middle-row delete.
- [ ] `components/pages/files/reactions/ReactionStatus.tsx` ⚑ — `let deleteInProgress` at **module scope** is shared across instances and never reset on unmount-mid-delete.

---

## 🏗️ Composition (`/composition-patterns`)

Already done well: state is lifted into providers; `Panel` is a real compound component; render props are used sparingly and correctly (`renderEntity` passes data back — the sanctioned use).

- [ ] **React 19: drop `forwardRef`** (`react19-no-forwardref`) — `shared/buttons/Button.tsx:66`, `buttons/IconButton.tsx:21`, `Panel/Panel.tsx:55`. `ref` is a regular prop in React 19.
- [ ] **React 19: `useContext(X)` → `use(X)`** — 9 sites: `auth.tsx:183`, `entities/details/EntityDetails.tsx:93`, `entities/create/EntityCreate.tsx:44`, `entities/details/override_pages/RepoDetails.tsx:23`, `pages/GraphBuilder.tsx:20`, `files/upload/UploadContext.tsx:84`, `windows/WindowManager/use_window_manager.ts:7`, `associations/data/GraphDataContext.tsx:36`, `dashboards/IncidentSummary/IncidentDataProvider.tsx:11`.
- [ ] **Boolean-flag proliferation in `onValueClick`** (`architecture-avoid-boolean-props`) — `SuggestionPanel.tsx:220`, `CodeEditor.tsx:223`. A 7-positional-arg callback with 4 booleans; call sites like `onValueClick?.(field, '', undefined, undefined, true, undefined)` are unreadable. Replace with a discriminated union; the CodeMirror boundary (`addPreview.of`) already takes an object.
  ```ts
  type SuggestionAction =
    | { kind: 'useValue'; field: string; value: string; schema?: FieldSchema; isList?: boolean; isMapEntry?: boolean }
    | { kind: 'add' | 'populate'; field: string; schema?: FieldSchema; isList?: boolean; isMapEntry?: boolean }
    | { kind: 'remove'; field: string };
  ```
  Related: `Suggestion` (`utilities/rules/types.ts:60-63`) encodes a kind as four parallel booleans (`isList/isMapEntry/isRemoval/isReplace`).
- [ ] **Boolean modes → explicit variants** (`patterns-explicit-variants`) — `images/ImageInfo.tsx` muxes `inEditMode` × `viewMode` (Editor/Form/read-only) in one component + a ~130-line inline IIFE; split into `ImageReadOnlyView` / `ImageFormEditor` / `ImageCodeEditor`. `inputs/selectable/SelectableArray|Dictionary` switch text-vs-dropdown by prop type → `EditableStringList` / `EditableSelectList`.
- [ ] **Image edit form → compound component + provider** (`architecture-compound-components`, `state-lift-state`) — the 11 image sub-panels each take `value`/`onChange`/`mode` and `ImageInfo` wires N hand-written merge callbacks (`onChange={(r) => setEditorObj(prev => ({...prev, resources: r}))}`). Lift `editorObj` into an `ImageFormProvider`; expose `<ImageForm.Fields/>`, `<ImageForm.Resources/>`, … reading context. Removes prop-drilling, lets the Save button read validity from context instead of `useImperativeHandle`, and resolves the duplicated View/Edit/Create scaffolding. Mirror `Panel.tsx`.
- [ ] **`tools/displays/*` → `<ResultShell>`** — 7 renderers duplicate the alert+footer chrome (`useEffect(getAlerts)` + `AlertBanner` map + `ResultsFiles`/`ChildrenFiles`). Extract `useResultAlerts(result)` + a shell wrapper.
- [ ] **`tools/ToolResult.tsx:158` → renderer registry** — 9-way `type == OutputDisplayType.X && <Renderer/>` chain with hardcoded tool-name special cases. Replace with `Record<OutputDisplayType, FC>` (+ a custom sub-registry keyed by tool).
- [ ] **Context value shape** (`state-context-interface`) — `EntityDetailsContextType` (`EntityDetails.tsx:43-60`) is a flat 16-field bag mixing state/actions/meta and is rebuilt every render (all consumers re-render). Organize into `{ state, actions, meta }` and `useMemo` the value. Same for `EntityCreate`; `auth.tsx` additionally erases method types to `Promise<unknown>`.
- [ ] **`thorpi` list contract is inconsistent** — `listFiles/listEntities/listRepos/search` return `{ ..., cursor }` on failure; `listImages/listPipelines/listGroups/...` return `null` + a union, forcing `as ImageList`/`as Image[]` casts at call sites. Standardize.
- [ ] **Duplicate role helpers** — `utilities/role.ts:getThoriumRole` (used widely) vs `utilities/users.ts:getUserRole` (uses `any`, ~unused). Keep one typed helper.
- [ ] **Layout duplication** — `pages/users/Groups.tsx` (1134 lines; read vs edit halves) and `pages/reactions/ReactionStatus.tsx:271-524` (full vs compact copies) — factor into data-driven row components. Image sub-panels also each re-define an identical `const Input = styled.input` / `const Select = styled.select` → move to `shared.styled.tsx`.

---

## 🎨 Style / conventions (`ui/CLAUDE.md`)

- [ ] **Pervasive `react-bootstrap` in converted `.tsx`** (biggest theme) — `Accordion/Badge/Button/Modal/Row/Col/Form/Card/Table/Tabs/Dropdown/OverlayTrigger/Spinner` across `pages/users`, `pages/reactions`, `components/pages/files/*`, `tools/displays/*`, `tags/*`, graph controls, most entity configs — and even in shared primitives (`Card.tsx`, `LoadingSpinner.tsx`, `DeleteConfirmModal.tsx`, `ScrollableSelect.tsx`) that should be *replacing* it. Shared styled replacements (`Button`, `Panel`, `Card`, `AlertBanner`, `OverlayTipTop/...`) already exist but go unused. Swap when touching a file.
- [ ] **Union types that should be string enums** — `models/pipelines.ts:EventTrigger`, `models/images.ts:SpawnLimitsValue`/`KwargDependencyValue`, `models/reactions.ts` log `action`, `models/files.ts` protocol (`'TCP'|'Tcp'|'tcp'`), `associations/.../controls/types.ts:SelectedElement.kind`, `dashboards/IncidentSummary/types.ts` severity/status, `EditorTab`.
- [ ] **`any` / `as unknown as`** — `inputs/selectable/SelectInputArray.tsx:49,131`, `utilities/role.ts:10,33`, `utilities/users.ts:5`, `auth.tsx:214`, `pages/users/UserBrowsing.tsx:163`. The role casts indicate `ThoriumRole`↔`RoleKey` don't compose — add a type guard.
- [ ] **Loose equality** (`==`/`!=`) pervasive in older files (`tools/displays/*`, `selectable/*`, `EditableTags`, `tags/*`). Enable eslint `eqeqeq`.
- [ ] **`filter` used as `forEach`** for side effects — `utilities/tags.ts`, `tags/EditableTags.tsx` (builds + discards an array; callback returns `undefined`).
- [ ] **Doc drift** — CLAUDE.md's "Legacy JSX" list is **stale**: `files/*.jsx`, `files/reactions/*.jsx`, `images/Volumes.jsx`, `selectable/*.jsx`, `EditableTags.jsx` are all already `.tsx`. Update the doc.
- [ ] **Import ordering** — several newer files (`pages/test/code/*`, `dashboards/IncidentSummary/*`, `EntityBrowsing.tsx`, `AssociationTree.tsx`, `GraphControlsToolbar.tsx`) omit the `// project imports` separator / local→`@components`→`@thorpi`/`@utilities`→`@models` order.

---

## 🔧 In-flight feature: `CodeEditor` / rules (this branch)

The branch diff is **CodeMirror widget code** (imperative `WidgetType` / `document.createElement` / `style.cssText`) and **TS schema/suggestion utilities** — not React render trees — so most react-best-practices rules don't apply to the changed lines. The new object-list/variant helpers in `SuggestionPreview.ts` read as internally consistent. Findings on the feature's React surface (unchanged `.tsx` files):

- [ ] **`SuggestionPanel` is not memoized** (`rerender-memo`) — `CodeEditor` calls `setCursorLine` in its `updateListener`, so it re-renders whenever the cursor's line changes. For YAML/JSON, `filteredSuggestions` (same array ref) and `handleValueClick` are stable, so `React.memo(SuggestionPanel)` skips re-grouping/re-mapping on cursor-line moves. `groups`/`hasCategories` (`SuggestionPanel.tsx:236`) would then recompute only when suggestions change.
- [ ] **`CodeEditor.tsx:85-92` writes refs during render** (`advanced-use-latest`) — `onChangeRef/checkerRef/formatRef/cursorLineRef`. Harmless (idempotent) but a render-time side effect; React 19 `useEffectEvent` is the idiomatic replacement for the callback cases.
- [ ] **`onValueClick` positional booleans** — see Composition section (discriminated-union refactor; in this branch).

---

## ✅ Verified non-issues (caught during verification)

- **`ImageInfo.tsx` "Editor→Form discards edits" — NOT a bug.** Editor and Form are mutually-exclusive render branches (`ImageInfo.tsx:184`), so switching unmounts/remounts the sub-components, which re-seed from the current `editorObj`. Edits are preserved.
- **Entity factory inner components (`EntityHeader`, `EntityButtons`, …) — NOT a remount bug.** Defined inside `createEntityDetailsPage`, which runs once at module load → stable identities. (The blob-URL leak inside `EntityHeader` is still real — see Medium bugs.)

---

## Suggested order of work

1. 🔴 High bugs (#1 `hasInvalidTags`, #2 `SelectPipelines`) — validation bypass + user-facing data loss.
2. React 19 sweep (`forwardRef` ×3, `useContext`→`use` ×9) — mechanical, low-risk.
3. `onValueClick` discriminated-union refactor — small, inside the current branch.
4. Quick-win Medium bugs (missing `return`, duplicate `id`, reducer default, sort-in-render, blob leaks).
5. Larger composition refactors (`ImageFormProvider`, `<ResultShell>`, explicit variants) as dedicated tasks.

*Verify any item marked `⚑` before fixing. Run `npm run build` + `npm run lint` after changes.*
