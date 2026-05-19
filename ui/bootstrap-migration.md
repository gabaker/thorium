# Bootstrap Migration Analysis

## Current Component Library Dependencies

| Library | Package | Version | Files Using | Purpose |
|---------|---------|---------|-------------|---------|
| **Bootstrap** | `bootstrap` | ^5.3.8 | — | CSS framework (SCSS imported globally) |
| **React-Bootstrap** | `react-bootstrap` | ^2.10.10 | 103 files | React component wrappers for Bootstrap |
| **react-select** | `react-select` | ^5.10.2 | 7 files (9 imports) | Dropdown select / creatable select |
| **styled-components** | `styled-components` | — | 75 imports | Already the target styling system |

Total source files: ~375. **103 files (27%) import react-bootstrap.**

---

## React-Bootstrap Component Inventory

### Layout Components

| Component | Files | Category |
|-----------|-------|----------|
| **Row** | 70 | Grid layout |
| **Col** | 45 | Grid layout |
| **Container** | 2 | Grid layout |
| **Stack** | 1 | Flex layout |

**Row** (70 files):
- `src/pages/Login.tsx`
- `src/pages/images/ImageCreate.tsx`
- `src/pages/images/ImageBrowsing.tsx`
- `src/pages/users/UserBrowsing.tsx`
- `src/pages/users/UserProfile.tsx`
- `src/pages/users/Groups.jsx`
- `src/pages/reactions/ReactionStatus.jsx`
- `src/pages/system/SystemStats.tsx`
- `src/pages/Pipelines.tsx`
- `src/pages/test/AlertBannerTest.tsx`
- `src/pages/GraphBuilder.tsx`
- `src/components/pages/search/Search.tsx`
- `src/components/pages/files/Comments.jsx`
- `src/components/pages/files/Download.jsx`
- `src/components/pages/files/upload/OriginField.tsx`
- `src/components/pages/files/upload/OriginCarved.tsx`
- `src/components/pages/files/upload/OriginMemoryDump.tsx`
- `src/components/pages/files/upload/UploadAlertBanner.tsx`
- `src/components/pages/files/upload/UploadForm.tsx`
- `src/components/pages/files/upload/UploadStatusDashboard.tsx`
- `src/components/pages/files/upload/UploadStatusTable.tsx`
- `src/components/pages/files/reactions/ReactionStatus.jsx`
- `src/components/pages/files/reactions/RunPipelines.jsx`
- `src/components/pages/files/reactions/SelectPipelines.jsx`
- `src/components/pages/images/Volumes.jsx`
- `src/components/tools/ToolResult.tsx`
- `src/components/tools/displays/String.tsx`
- `src/components/tools/displays/JSON.tsx`
- `src/components/tools/displays/Image.tsx`
- `src/components/tools/displays/Disassembly.tsx`
- `src/components/tools/displays/custom/VBA.tsx`
- `src/components/tools/displays/custom/TC2.tsx`
- `src/components/tools/displays/custom/AvMulti.tsx`
- `src/components/tools/displays/files/ResultsFiles.tsx`
- `src/components/tools/displays/files/ChildrenFiles.tsx`
- `src/components/entities/details/EntityDetails.tsx`
- `src/components/entities/details/override_pages/FileDetails.jsx`
- `src/components/entities/details/configs/CollectionDetailsConfig.tsx`
- `src/components/entities/details/configs/DeviceDetailsConfig.tsx`
- `src/components/entities/details/configs/FileSystemDetailsConfig.tsx`
- `src/components/entities/details/configs/FolderDetailsConfig.tsx`
- `src/components/entities/details/configs/NetworkConnectionDetailsConfig.tsx`
- `src/components/entities/details/configs/SigmaRuleDetailsConfig.tsx`
- `src/components/entities/details/configs/VendorDetailsConfig.tsx`
- `src/components/entities/details/configs/WindowsProcessDetailsConfig.tsx`
- `src/components/entities/details/configs/WindowsProcessTreeDetailsConfig.tsx`
- `src/components/entities/create/EntityCreate.tsx`
- `src/components/entities/create/configs/CollectionCreateConfig.tsx`
- `src/components/entities/create/configs/DeviceCreateConfig.tsx`
- `src/components/entities/create/configs/SigmaRuleCreateConfig.tsx`
- `src/components/entities/create/configs/VendorCreateConfig.tsx`
- `src/components/entities/browsing/EntityList.tsx`
- `src/components/entities/browsing/shared.tsx`
- `src/components/entities/browsing/configs/CollectionBrowsingConfig.tsx`
- `src/components/entities/browsing/configs/DeviceBrowsingConfig.tsx`
- `src/components/entities/browsing/configs/FileBrowsingConfig.tsx`
- `src/components/entities/browsing/configs/FileSystemBrowsingConfig.tsx`
- `src/components/entities/browsing/configs/FolderBrowsingConfig.tsx`
- `src/components/entities/browsing/configs/NetworkConnectionBrowsingConfig.tsx`
- `src/components/entities/browsing/configs/OtherBrowsingConfig.tsx`
- `src/components/entities/browsing/configs/RepoBrowsingConfig.tsx`
- `src/components/entities/browsing/configs/SigmaRuleBrowsingConfig.tsx`
- `src/components/entities/browsing/configs/VendorBrowsingConfig.tsx`
- `src/components/entities/browsing/configs/WindowsProcessBrowsingConfig.tsx`
- `src/components/entities/browsing/configs/WindowsProcessTreeBrowsingConfig.tsx`
- `src/components/entities/browsing/filters/BrowsingFilters.tsx`
- `src/components/entities/browsing/filters/FilterFields.tsx`
- `src/components/shared/inputs/selectable/SelectableArray.jsx`
- `src/components/shared/inputs/selectable/SelectableDictionary.jsx`
- `src/components/shared/fallback/LoadingSpinner.tsx`
- `src/components/tags/EditableTags.jsx`

**Col** (45 files): Subset of Row files above — always used together.

**Container** (2 files):
- `src/pages/users/UserProfile.tsx`
- `src/components/shared/fallback/LoadingSpinner.tsx`

**Stack** (1 file):
- `src/components/pages/search/Search.tsx`

### Surface Components

| Component | Files | Category |
|-----------|-------|----------|
| **Card** | 34 | Content container |
| **Table** | 3 | Data table |
| **Accordion** | 3 | Collapsible sections |

**Card** (34 files):
- `src/pages/Login.tsx`
- `src/pages/users/UserBrowsing.tsx`
- `src/pages/reactions/ReactionStageLogs.jsx`
- `src/pages/reactions/ReactionStatus.jsx`
- `src/pages/GraphBuilder.tsx`
- `src/components/pages/search/Search.tsx`
- `src/components/pages/files/Comments.jsx`
- `src/components/pages/files/reactions/SelectPipelines.jsx`
- `src/components/pages/files/reactions/ReactionStatus.jsx`
- `src/components/pages/files/upload/OriginForm.tsx`
- `src/components/pages/files/upload/OriginCarved.tsx`
- `src/components/pages/files/upload/TLPSelection.tsx`
- `src/components/pages/files/upload/UploadStatusDashboard.tsx`
- `src/components/pages/files/upload/UploadStatusTable.tsx`
- `src/components/tools/SafeHtml.tsx`
- `src/components/tools/ToolResult.tsx`
- `src/components/tools/displays/String.tsx`
- `src/components/tools/displays/Tables.tsx`
- `src/components/tools/displays/JSON.tsx`
- `src/components/tools/displays/Image.tsx`
- `src/components/tools/displays/Disassembly.tsx`
- `src/components/tools/displays/Markdown.tsx`
- `src/components/tools/displays/XML.tsx`
- `src/components/tools/displays/custom/VBA.tsx`
- `src/components/tools/displays/custom/TC2.tsx`
- `src/components/tools/displays/custom/AvMulti.tsx`
- `src/components/entities/details/EntityDetails.tsx`
- `src/components/entities/details/override_pages/FileDetails.jsx`
- `src/components/entities/details/override_pages/RepoDetails.tsx`
- `src/components/entities/create/EntityCreate.tsx`
- `src/components/entities/create/configs/SigmaRuleCreateConfig.tsx`
- `src/components/entities/browsing/shared.tsx`
- `src/components/shared/Card.tsx`
- `src/components/tags/EditableTags.jsx`

**Table** (3 files):
- `src/pages/system/SystemStats.tsx`
- `src/pages/system/SystemSettings.jsx`
- `src/components/tools/displays/Tables.tsx`

**Accordion** (3 files):
- `src/pages/images/ImageBrowsing.tsx`
- `src/pages/Pipelines.tsx`
- `src/pages/users/Groups.jsx`

### Interactive Components

| Component | Files | Category |
|-----------|-------|----------|
| **Button** | 33 | Actions |
| **Form** | 28 | Form controls |
| **Modal** | 14 | Dialogs |
| **Dropdown** | 6 | Menus |
| **DropdownButton** | 1 | Menus |
| **ButtonGroup** | 4 | Button grouping |
| **ButtonToolbar** | 5 | Button layout |
| **Tabs / Tab** | 3-4 | Tab navigation |
| **Nav** | 1 | Navigation |
| **Pagination** | 2 | Page navigation |
| **FormCheck** | 1 | Checkbox/radio |

**Button** (33 files):
- `src/pages/Login.tsx`
- `src/pages/images/ImageCreate.tsx`
- `src/pages/images/ImageBrowsing.tsx`
- `src/pages/users/UserBrowsing.tsx`
- `src/pages/users/UserProfile.tsx`
- `src/pages/users/Groups.jsx`
- `src/pages/reactions/ReactionStageLogs.jsx`
- `src/pages/reactions/ReactionStatus.jsx`
- `src/pages/Pipelines.tsx`
- `src/components/pages/files/Comments.jsx`
- `src/components/pages/files/reactions/SelectPipelines.jsx`
- `src/components/pages/files/reactions/RunPipelines.jsx`
- `src/components/pages/files/reactions/ReactionStatus.jsx`
- `src/components/pages/files/upload/UploadForm.tsx`
- `src/components/pages/files/upload/UploadStatusDashboard.tsx`
- `src/components/pages/files/upload/UploadStatusTable.tsx`
- `src/components/pages/files/upload/TLPSelection.tsx`
- `src/components/pages/images/Volumes.jsx`
- `src/components/pages/groups/SelectGroups.tsx`
- `src/components/tools/ToolResult.tsx`
- `src/components/entities/details/EntityDetails.tsx`
- `src/components/entities/details/ListCollectionsButton.tsx`
- `src/components/entities/details/override_pages/FileDetails.jsx`
- `src/components/entities/create/EntityCreate.tsx`
- `src/components/entities/browsing/filters/BrowsingFilters.tsx`
- `src/components/entities/browsing/filters/FilterFields.tsx`
- `src/components/shared/inputs/selectable/SelectableArray.jsx`
- `src/components/shared/inputs/selectable/SelectableDictionary.jsx`
- `src/components/shared/inputs/tags/TagSelect/TagSelect.tsx`
- `src/components/shared/badges/LinkBadge.tsx`
- `src/components/tags/TagBadge.tsx`
- `src/components/tags/EditableTags.jsx`
- `src/components/associations/graph/controls/Toolbar.styled.tsx`

**Modal** (14 files):
- `src/pages/Login.tsx`
- `src/pages/users/UserBrowsing.tsx`
- `src/pages/users/UserProfile.tsx`
- `src/pages/users/Groups.jsx`
- `src/pages/images/ImageBrowsing.tsx`
- `src/pages/reactions/ReactionStatus.jsx`
- `src/pages/Pipelines.tsx`
- `src/components/pages/files/reactions/ReactionStatus.jsx`
- `src/components/entities/details/EntityDetails.tsx`
- `src/components/entities/details/override_pages/FileDetails.jsx`
- `src/components/shared/inputs/tags/TagSelect/TagSelect.tsx`
- `src/components/shared/badges/LinkBadge.tsx`
- `src/components/tags/TagBadge.tsx`
- `src/components/tags/EditableTags.jsx`

**Tabs/Tab** (4 files):
- `src/pages/reactions/ReactionStatus.jsx`
- `src/components/pages/files/upload/OriginForm.tsx`
- `src/components/pages/files/upload/OriginCarved.tsx`
- `src/components/entities/details/override_pages/FileDetails.jsx`

### Feedback/Status Components

| Component | Files | Category |
|-----------|-------|----------|
| **Badge** | 9 | Status indicator |
| **Spinner** | 4 | Loading indicator |
| **ProgressBar** | 1 | Progress display |

**Badge** (9 files):
- `src/pages/users/UserBrowsing.tsx`
- `src/pages/users/UserProfile.tsx`
- `src/pages/users/Groups.jsx`
- `src/pages/images/ImageBrowsing.tsx`
- `src/pages/Pipelines.tsx`
- `src/components/pages/files/reactions/reactions.jsx`
- `src/components/pages/groups/GroupRoleBadge.tsx`
- `src/components/entities/details/override_pages/FileDetails.jsx`
- `src/components/shared/badges/FieldBadge.tsx`

**Spinner** (4 files):
- `src/components/shared/fallback/LoadingSpinner.tsx`
- `src/components/associations/graph/AssociationGraph.tsx`
- `src/components/associations/graph/controls/Toolbar.styled.tsx`
- `src/components/associations/browsing/AssociationTree.tsx`

**ProgressBar** (1 file):
- `src/components/pages/files/upload/ProgressBarContainer.tsx`

### Overlay Components

| Component | Files | Category |
|-----------|-------|----------|
| **Tooltip** | 5 | Tooltips |
| **OverlayTrigger** | 4 | Overlay positioning |
| **Overlay** | 2 | Manual overlay positioning |
| **Popover** | 5 | Popovers |

**Tooltip + OverlayTrigger** (shared across):
- `src/components/shared/overlay/OverlayTip.tsx`
- `src/components/pages/groups/GroupRoleBadge.tsx`
- `src/components/associations/graph/DataPreviewPanel.tsx`
- `src/components/associations/graph/AssociationGraph.tsx`
- `src/components/associations/graph/controls/ToolbarButton.tsx`

**Popover** (5 files):
- `src/components/associations/graph/AssociationGraph.tsx`
- `src/components/associations/graph/controls/Toolbar.styled.tsx`
- `src/components/associations/graph/controls/ToolbarButton.tsx`
- `src/components/associations/browsing/PreviewPopover.tsx`
- `src/components/associations/browsing/AssociationTree.tsx`

---

## Bootstrap CSS Utility Class Usage

Beyond react-bootstrap components, Bootstrap CSS utility classes are used directly in `className` strings across the codebase.

| Utility Category | Approximate Instances | Files |
|-----------------|----------------------|-------|
| **Spacing** (`mb-`, `mt-`, `ms-`, `me-`, `p-`, `px-`, `py-`, `m-`, `mx-`, `my-`) | ~354 | 71 files |
| **Flexbox/Display** (`d-flex`, `d-none`, `justify-content-*`, `align-items-*`, `text-center`) | ~130 | 71 files |
| **Button classes** (`btn`, `btn-*`) | ~86 | 35 files |
| **Form classes** (`form-control`, `form-group`, etc.) | ~1 | minimal |

**Total files with Bootstrap utility classes: ~71 files**

---

## Bootstrap SCSS Integration

### Global Import
`src/styles/main.scss` imports the entire Bootstrap SCSS library with custom spacer overrides:
```scss
@use 'bootstrap/scss/bootstrap' with (
  $spacer: 1rem,
  $spacers: ( 0: 0, 1: 0.1rem, ... 10: 7rem )
);
```

### Legacy Override Files (1,530 lines total)
`src/styles/legacy/` contains 15 SCSS files that override Bootstrap component styles:

| File | Lines | Purpose |
|------|-------|---------|
| `details.scss` | 253 | Entity detail page overrides |
| `buttons.scss` | 242 | Button style overrides |
| `upload.scss` | 227 | Upload form overrides |
| `reactions.scss` | 215 | Reaction status overrides |
| `results.scss` | 123 | Tool result display overrides |
| `image.scss` | 117 | Image page overrides |
| `tags.scss` | 116 | Tag component overrides |
| `titles.scss` | 48 | Title styling overrides |
| `groups.scss` | 45 | Group management overrides |
| `pipelines.scss` | 33 | Pipeline page overrides |
| `comments.scss` | 27 | Comments section overrides |
| `accordion.scss` | 26 | Accordion overrides |
| `logs.scss` | 25 | Log display overrides |
| `overlays.scss` | 19 | Overlay/popover overrides |
| `users.scss` | 14 | User management overrides |

### Additional Bootstrap-Dependent SCSS
- `src/styles/overrides.scss` — 463 lines, ~24 references to Bootstrap selectors
- `src/styles/colors.scss` — 557 lines, theme variable definitions (theme-independent, will survive migration)

---

## react-select Usage

react-select is used in 7 files for advanced dropdown select functionality:

| File | Components Used |
|------|----------------|
| `src/components/shared/inputs/selectable/SelectInput.tsx` | `CreatableSelect` |
| `src/components/shared/inputs/selectable/SelectInputArray.tsx` | `CreatableSelect`, `Select` |
| `src/components/shared/pipeline/PipelineOrderFlow.tsx` | `Select` |
| `src/components/entities/details/override_pages/FileDetails.jsx` | `Select` |
| `src/pages/users/Groups.jsx` | `Select`, `CreatableSelect` |
| `src/components/tags/EditableTags.jsx` | `Select`, `CreatableSelect` |

react-select is independent of Bootstrap and can remain as-is or be replaced separately.

---

## Migration Strategy

### Priority Order

The migration should be done bottom-up: start with shared/leaf components, then move up to pages.

#### Phase 1: Shared Primitive Replacements (High Impact, Low Risk)

Create styled-component replacements in `src/components/shared/` for the most-used primitives:

1. **Grid: `Row`/`Col`/`Container`** — 70+ files
   - Replace with CSS Grid or Flexbox styled-components
   - Example: `<Flex>`, `<Grid>`, or just use native CSS grid/flex in component styles
   - This is the single highest-impact replacement (affects 70 files)

2. **`Card`** — 34 files
   - Already has a custom wrapper at `src/components/shared/Card.tsx` that wraps react-bootstrap Card
   - Convert the wrapper to a pure styled-component, then all 34 consumers migrate automatically

3. **`Button`** — 33 files
   - A new custom Button exists at `src/components/shared/buttons/`
   - Expand variant support to cover all Bootstrap variants used (`primary`, `secondary`, `danger`, `outline-*`, `link`, sizes)

4. **`Form` / `Form.Control` / `Form.Group` / `Form.Label` / `Form.Select` / `Form.Check`** — 28 files
   - Create styled form primitives: `<Input>`, `<Label>`, `<FormGroup>`, `<Select>`, `<Checkbox>`

5. **`Badge`** — 9 files
   - Already has `FieldBadge` at `src/components/shared/badges/FieldBadge.tsx`
   - Create a base `Badge` styled-component and update `FieldBadge` to use it

6. **`Spinner`** — 4 files
   - Already wrapped in `LoadingSpinner` at `src/components/shared/fallback/LoadingSpinner.tsx`
   - Replace the Bootstrap Spinner with a CSS animation

#### Phase 2: Overlay/Popup Components (Medium Complexity)

7. **`Modal`** — 14 files
   - Create a custom Modal with backdrop, close button, header/body/footer slots
   - Consider using the native `<dialog>` element for accessibility

8. **`Tooltip` / `OverlayTrigger` / `Overlay`** — 5 files
   - Already wrapped in `OverlayTip` at `src/components/shared/overlay/OverlayTip.tsx`
   - Replace with CSS-based tooltips or a lightweight positioning library (e.g., Floating UI)

9. **`Popover`** — 5 files
   - Used heavily in the association graph components
   - Replace alongside Tooltip using the same positioning solution

10. **`Dropdown` / `DropdownButton`** — 6 files
    - Create a custom Dropdown with keyboard navigation
    - `ScrollableSelect` at `src/components/shared/inputs/ScrollableSelect.tsx` already wraps Bootstrap Dropdown

#### Phase 3: Specialized Components (Lower Count)

11. **`Accordion`** — 3 files
    - Create a collapsible section component with CSS transitions

12. **`Tabs` / `Tab` / `Nav`** — 4 files
    - Create a tabbed interface component

13. **`Table`** — 3 files
    - Create a styled table component (minimal styling needed)

14. **`Pagination`** — 2 files
    - Create a pagination component

15. **`ProgressBar`** — 1 file
    - Simple CSS-based progress bar

16. **`ButtonGroup` / `ButtonToolbar`** — 4-5 files
    - Trivial flex container styled-components

#### Phase 4: CSS Utility Class Elimination

17. **Replace spacing utilities** (`mb-3`, `mt-2`, `p-4`, etc.) — ~354 instances across 71 files
    - Use the existing `spacers` enum from `src/styles/index.ts` in styled-components
    - Or create utility styled-components: `<Spacer size={3} />`, `<Box p={4}>`

18. **Replace flexbox utilities** (`d-flex`, `justify-content-*`, `align-items-*`) — ~130 instances
    - Replace with styled-component flex containers

19. **Replace button classes** (`btn btn-primary`, etc.) — ~86 instances
    - Convert to the custom Button component from Phase 1

#### Phase 5: SCSS Cleanup

20. **Remove legacy override files** — 1,530 lines in `src/styles/legacy/`
    - Each legacy file becomes unnecessary as its corresponding Bootstrap component is replaced
    - Remove files one-by-one as components are migrated

21. **Clean up `overrides.scss`** — 463 lines
    - Remove Bootstrap-specific overrides as components are migrated
    - Keep theme variable definitions and HTML element resets

22. **Remove Bootstrap SCSS import** from `main.scss`
    - Final step: remove `@use 'bootstrap/scss/bootstrap'` once no Bootstrap classes remain

23. **Uninstall packages**
    - `npm uninstall react-bootstrap bootstrap`

### Estimated Scope

| Phase | Files Affected | Effort |
|-------|---------------|--------|
| Phase 1: Shared Primitives | ~103 unique files | Large — highest impact |
| Phase 2: Overlays/Popups | ~20 unique files | Medium |
| Phase 3: Specialized | ~15 unique files | Small |
| Phase 4: CSS Utilities | ~71 files | Large — tedious but mechanical |
| Phase 5: SCSS Cleanup | ~17 SCSS files | Small |

### Key Risks

1. **The entity config system** (`browsing/configs/`, `details/configs/`, `create/configs/`) uses Row/Col extensively across ~30 config files. These are highly repetitive and can be batch-migrated.
2. **Legacy `.jsx` files** (listed in CLAUDE.md) should be converted to `.tsx` during migration.
3. **Tool result displays** (`components/tools/displays/`) use Row/Col/Card heavily across ~12 files. These follow a consistent pattern and can be batch-migrated.
4. **The graph/association components** use Popover/Overlay/Tooltip extensively and have complex positioning requirements — these need careful replacement.
5. **Bootstrap's responsive grid breakpoints** (`col-md-6`, `col-lg-4`) are used in some components and need equivalent CSS media queries in replacements.

### What Can Stay

- **react-select** — Independent library, no Bootstrap dependency. Can remain or be replaced separately.
- **styled-components** — Already the target system. No migration needed.
- **Theme CSS variables** (`--thorium-*`) — Defined in `colors.scss`, independent of Bootstrap. Will survive migration.
- **`spacers` and `scaling` enums** in `src/styles/index.ts` — Already available for use in styled-components.
