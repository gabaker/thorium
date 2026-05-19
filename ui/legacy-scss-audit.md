# Legacy SCSS Class Audit

Audit of all global SCSS classes from `src/styles/` (excluding `overrides.scss` and color variables) that are referenced as `className` in `.tsx`/`.jsx` component files.

Generated: 2026-05-28

---

## Active Classes

### accordion.scss

| Class | Files |
|-------|-------|
| `.accordion-item-name` | `components/pages/images/ImageAccordionItem.tsx`, `components/pages/pipelines/PipelineAccordionItem.tsx`, `pages/users/Groups.jsx` |
| `.accordion-item-ownership` | `components/pages/images/ImageAccordionItem.tsx`, `components/pages/pipelines/PipelineAccordionItem.tsx`, `pages/users/Groups.jsx` |
| `.accordion-item-relation` | `components/pages/images/ImageAccordionItem.tsx`, `components/pages/pipelines/PipelineAccordionItem.tsx`, `pages/users/Groups.jsx` |
| `.accordion-item-status` | `components/pages/images/ImageAccordionItem.tsx`, `components/pages/pipelines/PipelineAccordionItem.tsx` |

### app.scss

| Class | Files |
|-------|-------|
| `.auto-width` | `components/pages/files/Comments.jsx`, `components/pages/files/reactions/RunPipelines.jsx`, `components/tools/displays/Tables.tsx` |
| `.body-panel` | `pages/reactions/ReactionStatus.jsx` |
| `.count-badge` | `pages/images/ImageBrowsing.tsx`, `pages/pipelines/PipelineBrowsing.tsx`, `pages/users/Groups.jsx` |
| `.full-min-width` | `components/entities/create/EntityCreate.tsx`, `components/entities/details/EntityDetails.tsx`, `components/entities/details/override_pages/FileDetails.jsx`, `components/entities/details/override_pages/RepoDetails.tsx`, `dashboards/IncidentSummary/IncidentSummary.tsx`, `pages/GraphBuilder.tsx`, `pages/reactions/ReactionStatus.jsx` |
| `.full-width` | `components/pages/files/reactions/ReactionStatus.jsx`, `components/pages/files/reactions/RunReactionAlerts.jsx` |
| `.login` | `pages/Login.tsx` |
| `.no-border` | `components/pages/files/reactions/ReactionStatus.jsx` |
| `.no-bullets` | `components/pages/files/Results.jsx` |
| `.no-decoration` | `components/entities/browsing/configs/CollectionBrowsingConfig.tsx`, `components/entities/browsing/configs/DeviceBrowsingConfig.tsx`, `components/entities/browsing/configs/FileBrowsingConfig.tsx`, `components/entities/browsing/configs/FileSystemBrowsingConfig.tsx`, `components/entities/browsing/configs/FolderBrowsingConfig.tsx`, `components/entities/browsing/configs/NetworkConnectionBrowsingConfig.tsx`, `components/entities/browsing/configs/OtherBrowsingConfig.tsx`, `components/entities/browsing/configs/RepoBrowsingConfig.tsx`, `components/entities/browsing/configs/SigmaRuleBrowsingConfig.tsx`, `components/entities/browsing/configs/VendorBrowsingConfig.tsx`, `components/entities/browsing/configs/WindowsProcessBrowsingConfig.tsx`, `components/entities/browsing/configs/WindowsProcessTreeBrowsingConfig.tsx`, `components/pages/files/reactions/ReactionStatus.jsx`, `components/tags/TagBadge.tsx`, `pages/reactions/ReactionStatus.jsx` |
| `.panel` | `components/entities/create/EntityCreate.tsx`, `components/entities/details/EntityDetails.tsx`, `components/entities/details/override_pages/FileDetails.jsx`, `components/entities/details/override_pages/RepoDetails.tsx`, `components/pages/files/Comments.jsx`, `components/pages/files/reactions/ReactionStatus.jsx`, `components/pages/files/reactions/SelectPipelines.jsx`, `components/pages/files/upload/OriginCarved.tsx`, `components/pages/files/upload/OriginForm.tsx`, `components/pages/files/upload/TLPSelection.tsx`, `components/pages/files/upload/UploadStatusDashboard.tsx`, `components/pages/files/upload/UploadStatusTable.tsx`, `components/pages/search/Search.tsx`, `components/shared/Card.tsx`, `components/tags/EditableTags.jsx`, `pages/GraphBuilder.tsx`, `pages/Login.tsx`, `pages/reactions/ReactionStageLogs.jsx`, `pages/reactions/ReactionStatus.jsx`, `pages/users/UserBrowsing.tsx` |
| `.secondary-text` | `pages/reactions/ReactionStageLogs.jsx`, `pages/users/UserBrowsing.tsx` |
| `.settings` | `pages/system/SystemSettings.jsx` |
| `.text` | `components/associations/shared/NodeInfo.tsx`, `components/entities/browsing/filters/BrowsingFilters.tsx`, `components/entities/details/EntityDetails.tsx`, `components/entities/details/override_pages/RepoDetails.tsx`, `components/pages/files/Comments.jsx`, `components/pages/files/reactions/RunReactionAlerts.jsx`, `components/pages/files/upload/UploadAlertBanner.tsx`, `components/pages/files/upload/UploadStatusTable.tsx`, `components/pages/images/ImageAccordionItem.tsx`, `components/pages/pipelines/PipelineAccordionItem.tsx`, `pages/GraphBuilder.tsx`, `pages/reactions/ReactionStageLogs.jsx`, `pages/test/AlertBannerTest.tsx`, `pages/users/Groups.jsx`, `pages/users/UserBrowsing.tsx` |
| `.wrap` | `components/associations/shared/NodeInfo.tsx`, `components/entities/details/override_pages/FileDetails.jsx`, `pages/reactions/ReactionStatus.jsx`, `pages/users/UserProfile.tsx` |

### buttons.scss

| Class | Files |
|-------|-------|
| `.clear-btn` | `components/entities/browsing/filters/BrowsingFilters.tsx` |
| `.danger-btn` | `components/entities/details/EntityDetails.tsx`, `components/entities/details/override_pages/FileDetails.jsx`, `components/pages/files/reactions/ReactionStatus.jsx`, `components/shared/browsing/DeleteConfirmModal.tsx`, `components/shared/inputs/selectable/SelectableArray.jsx`, `components/shared/inputs/selectable/SelectableDictionary.jsx`, `components/shared/inputs/tags/TagSelect/TagSelect.tsx`, `pages/reactions/ReactionStatus.jsx`, `pages/users/Groups.jsx`, `pages/users/UserBrowsing.tsx`, `pages/users/UserProfile.tsx` |
| `.download-btn` | `components/pages/files/Download.jsx` |
| `.icon-btn` | `components/entities/details/EntityDetails.tsx`, `components/entities/details/ListCollectionsButton.tsx`, `components/entities/details/override_pages/FileDetails.jsx`, `components/pages/files/reactions/ReactionStatus.jsx`, `components/tags/EditableTags.jsx`, `pages/images/ImageCreate.tsx`, `pages/reactions/ReactionStatus.jsx` |
| `.ok-btn` | `components/entities/browsing/filters/BrowsingFilters.tsx`, `components/entities/browsing/filters/FilterFields.tsx`, `components/pages/files/reactions/RunPipelines.jsx`, `components/pages/files/upload/UploadForm.tsx`, `components/pages/files/upload/UploadStatusDashboard.tsx`, `components/pages/pipelines/CreatePipelineModal.tsx`, `pages/Login.tsx`, `pages/images/ImageBrowsing.tsx`, `pages/images/ImageCreate.tsx`, `pages/pipelines/PipelineBrowsing.tsx`, `pages/users/Groups.jsx`, `pages/users/UserBrowsing.tsx` |
| `.primary-btn` | `components/entities/browsing/filters/FilterFields.tsx`, `components/entities/details/EntityDetails.tsx`, `components/entities/details/override_pages/FileDetails.jsx`, `components/pages/files/Comments.jsx`, `components/pages/files/reactions/ReactionStatus.jsx`, `components/pages/files/reactions/SelectPipelines.jsx`, `components/pages/groups/SelectGroups.tsx`, `components/shared/inputs/tags/TagSelect/TagSelect.tsx`, `components/tags/EditableTags.jsx`, `components/tools/ToolResult.tsx`, `pages/Login.tsx`, `pages/reactions/ReactionStageLogs.jsx`, `pages/reactions/ReactionStatus.jsx`, `pages/users/Groups.jsx`, `pages/users/UserBrowsing.tsx`, `pages/users/UserProfile.tsx` |
| `.secondary-btn` | `components/entities/create/EntityCreate.tsx`, `pages/images/ImageCreate.tsx`, `pages/users/UserBrowsing.tsx` |
| `.selected` | `components/associations/browsing/AssociationTree.tsx`, `components/pages/files/Results.jsx`, `components/pages/files/reactions/SelectPipelines.jsx`, `components/pages/files/upload/TLPSelection.tsx`, `components/pages/groups/SelectGroups.tsx` |
| `.tlp-btn` | `components/pages/files/upload/TLPSelection.tsx` |
| `.warning-btn` | `components/pages/files/upload/UploadStatusDashboard.tsx`, `components/shared/badges/LinkBadge.tsx`, `components/tags/EditableTags.jsx`, `components/tags/TagBadge.tsx`, `pages/users/Groups.jsx`, `pages/users/UserBrowsing.tsx` |

### comments.scss

| Class | Files |
|-------|-------|
| `.attachment-card` | `components/pages/files/Comments.jsx` |
| `.comment-entry` | `components/pages/files/Comments.jsx` |
| `.comments` | `components/entities/details/override_pages/FileDetails.jsx`, `components/pages/files/Comments.jsx` |
| `.single-comment` | `components/pages/files/Comments.jsx` |

### details.scss

| Class | Files |
|-------|-------|
| `.details-circle` | `components/entities/details/override_pages/FileDetails.jsx` |
| `.details-col` | `components/entities/details/override_pages/FileDetails.jsx` |
| `.details-navitem` | `components/entities/details/override_pages/FileDetails.jsx` |
| `.details-navlink` | `components/entities/details/override_pages/FileDetails.jsx` |
| `.details-sha-md5` | `components/entities/details/override_pages/FileDetails.jsx` |
| `.details-sha256` | `components/entities/details/override_pages/FileDetails.jsx` |
| `.details-tags-name` | `components/tags/EditableTags.jsx` |
| `.edit-icon` | `components/tags/EditableTags.jsx` |
| `.hide` | `components/entities/browsing/filters/BrowsingFilters.tsx`, `components/entities/details/override_pages/FileDetails.jsx`, `components/tags/TagBadge.tsx`, `pages/images/ImageCreate.tsx` |
| `.hide-sha256` | `components/entities/details/override_pages/FileDetails.jsx` |
| `.info-icon` | `components/entities/details/override_pages/FileDetails.jsx` |
| `.left-edit-tag-btn` | `components/tags/EditableTags.jsx` |
| `.lg-center-col` | `components/entities/details/override_pages/FileDetails.jsx` |
| `.lg-hide-col` | `components/entities/details/override_pages/FileDetails.jsx` |
| `.lg-show-row` | `components/entities/details/override_pages/FileDetails.jsx` |
| `.origin-field-name` | `components/entities/details/override_pages/FileDetails.jsx` |
| `.origin-sha256` | `components/entities/details/override_pages/FileDetails.jsx` |
| `.sha-md5-alignment` | `components/entities/details/override_pages/FileDetails.jsx` |
| `.short-origin-sha256` | `components/entities/details/override_pages/FileDetails.jsx` |
| `.short-sha256` | `components/entities/details/override_pages/FileDetails.jsx` |
| `.tags-col` | `components/tags/EditableTags.jsx` |

### groups.scss

| Class | Files |
|-------|-------|
| `.descr-height` | `pages/users/Groups.jsx` |
| `.edit-col` | `pages/users/Groups.jsx` |
| `.group-badge` | `components/pages/groups/GroupRoleBadge.tsx` |
| `.group-edit-badge` | `pages/users/Groups.jsx` |
| `.group-tooltip` | `pages/users/Groups.jsx` |
| `.header-col` | `components/pages/pipelines/PipelineInfo.tsx`, `components/pages/pipelines/TriggerDisplay.tsx`, `pages/users/Groups.jsx` |
| `.word-break-all` | `pages/test/AlertBannerTest.tsx`, `pages/users/Groups.jsx` |

### logs.scss

| Class | Files |
|-------|-------|
| `.log-box` | `pages/reactions/ReactionStageLogs.jsx`, `pages/reactions/ReactionStatus.jsx` |
| `.log-line-index` | `pages/reactions/ReactionStageLogs.jsx` |
| `.log-nav-button` | `pages/reactions/ReactionStageLogs.jsx` |
| `.raw-log-line` | `pages/reactions/ReactionStageLogs.jsx` |
| `.scrollable-card` | `pages/reactions/ReactionStageLogs.jsx` |

### overlays.scss

| Class | Files |
|-------|-------|
| `.tooltip-wide` | `components/shared/overlay/OverlayTip.tsx` |

### reactions.scss

| Class | Files |
|-------|-------|
| `.action-log` | `pages/reactions/ReactionStatus.jsx` |
| `.compact-reactions-row` | `pages/reactions/ReactionStatus.jsx` |
| `.full-reactions-row` | `pages/reactions/ReactionStatus.jsx` |
| `.key-log` | `pages/reactions/ReactionStatus.jsx` |
| `.pipeline-chart` | `pages/reactions/ReactionStatus.jsx` |
| `.pipeline-col` | `pages/reactions/ReactionStatus.jsx` |
| `.reaction-card` | `pages/reactions/ReactionStatus.jsx` |
| `.reaction-name-width` | `pages/reactions/ReactionStatus.jsx` |
| `.reactions-creator` | `components/pages/files/reactions/ReactionStatus.jsx` |
| `.reactions-group` | `components/pages/files/reactions/ReactionStatus.jsx` |
| `.reactions-id` | `components/pages/files/reactions/ReactionStatus.jsx` |
| `.reactions-pipeline` | `components/pages/files/reactions/ReactionStatus.jsx` |
| `.reactions-selection` | `components/pages/files/reactions/ReactionStatus.jsx` |
| `.reactions-status` | `components/pages/files/reactions/ReactionStatus.jsx` |
| `.scroll-log` | `components/tools/SafeHtml.tsx`, `components/tools/displays/Disassembly.tsx`, `components/tools/displays/Image.tsx`, `components/tools/displays/JSON.tsx`, `components/tools/displays/Markdown.tsx`, `components/tools/displays/String.tsx`, `components/tools/displays/Tables.tsx`, `components/tools/displays/XML.tsx`, `components/tools/displays/custom/AvMulti.tsx`, `components/tools/displays/custom/TC2.tsx`, `components/tools/displays/custom/VBA.tsx`, `pages/reactions/ReactionStatus.jsx` |
| `.timestamp-log` | `pages/reactions/ReactionStatus.jsx` |
| `.value-log` | `pages/reactions/ReactionStatus.jsx` |

### results.scss

| Class | Files |
|-------|-------|
| `.results-container` | `components/pages/files/Results.jsx` |
| `.results-content` | `components/tools/ToolResult.tsx` |
| `.results-toc` | `components/pages/files/Results.jsx` |
| `.results-toc-col` | `components/pages/files/Results.jsx` |
| `.results-toc-item` | `components/pages/files/Results.jsx` |
| `.tables-entry-lrg` | `components/tools/displays/Tables.tsx` |
| `.tables-entry-med` | `components/tools/displays/Tables.tsx` |
| `.tool-card` | `components/tools/ToolResult.tsx` |
| `.tool-result` | `components/tools/SafeHtml.tsx`, `components/tools/displays/Disassembly.tsx`, `components/tools/displays/Image.tsx`, `components/tools/displays/JSON.tsx`, `components/tools/displays/Markdown.tsx`, `components/tools/displays/String.tsx`, `components/tools/displays/Tables.tsx`, `components/tools/displays/XML.tsx`, `components/tools/displays/custom/AvMulti.tsx`, `components/tools/displays/custom/TC2.tsx`, `components/tools/displays/custom/VBA.tsx` |

### tags.scss

| Class | Files |
|-------|-------|
| `.clickable` | `components/tags/TagBadge.tsx`, `components/tools/ToolResult.tsx` |
| `.general-tag` | `components/tools/ToolResult.tsx` |
| `.short-tag` | `components/tags/TagBadge.tsx` |
| `.tag-item` | `components/tags/TagBadge.tsx`, `components/tools/ToolResult.tsx` |
| `.tags-hide` | `components/tags/TagBadge.tsx` |

### titles.scss

| Class | Files |
|-------|-------|
| `.simple-subtitle` | `components/shared/titles/SimpleSubtitle.tsx` |
| `.simple-title` | `components/shared/titles/SimpleTitle.tsx` |
| `.small-title` | `components/shared/titles/Title.tsx` |
| `.subtitle` | `components/shared/titles/SimpleSubtitle.tsx`, `components/shared/titles/Subtitle.tsx` |
| `.title` | `components/associations/browsing/AssociationTree.tsx`, `components/associations/shared/NodeInfo.tsx`, `components/entities/browsing/filters/BrowsingFilters.tsx`, `components/entities/browsing/filters/FilterFields.tsx`, `components/entities/create/EntityCreate.tsx`, `components/entities/details/EntityDetails.tsx`, `components/entities/details/override_pages/FileDetails.jsx`, `components/entities/details/override_pages/RepoDetails.tsx`, `components/pages/Page.tsx`, `components/pages/files/reactions/ReactionStatus.jsx`, `components/shared/titles/SimpleSubtitle.tsx`, `components/shared/titles/SimpleTitle.tsx`, `components/shared/titles/Subtitle.tsx`, `components/shared/titles/Title.tsx`, `components/tools/ToolResult.tsx`, `pages/GraphBuilder.tsx`, `pages/NotFound.tsx`, `pages/images/ImageCreate.tsx`, `pages/test/AlertBannerTest.tsx`, `pages/users/UserProfile.tsx` |
| `.title-link` | `components/tools/ToolResult.tsx` |
| `.title-link-no-color` | `components/tools/ToolResult.tsx` |

### upload.scss

| Class | Files |
|-------|-------|
| `.alt-label` | `components/pages/files/upload/UploadForm.tsx` |
| `.danger-bar` | `components/pages/files/upload/ProgressBarContainer.tsx` |
| `.info-bar` | `components/pages/files/upload/ProgressBarContainer.tsx` |
| `.link-text` | `components/pages/files/reactions/RunReactionAlerts.jsx`, `components/pages/files/upload/UploadAlertBanner.tsx`, `components/pages/files/upload/UploadStatusTable.tsx` |
| `.link-text-alt` | `components/pages/files/upload/UploadStatusTable.tsx` |
| `.name-width` | `components/pages/files/upload/OriginField.tsx`, `components/pages/files/upload/OriginMemoryDump.tsx`, `pages/reactions/ReactionStatus.jsx` |
| `.reaction-row` | `components/pages/files/upload/UploadStatusTable.tsx` |
| `.reaction-uploads-card` | `components/pages/files/upload/UploadStatusTable.tsx` |
| `.redo-btn` | `components/pages/files/upload/UploadStatusTable.tsx` |
| `.retry-button` | `components/pages/files/upload/UploadStatusDashboard.tsx` |
| `.stats-container` | `components/pages/files/upload/UploadStatusDashboard.tsx` |
| `.status-dropdown` | `components/pages/files/upload/UploadStatusTable.tsx` |
| `.status-error` | `components/pages/files/upload/UploadStatusTable.tsx` |
| `.status-file` | `components/pages/files/upload/UploadStatusTable.tsx` |
| `.status-msg` | `components/pages/files/upload/UploadStatusTable.tsx` |
| `.status-percent` | `components/pages/files/upload/UploadStatusTable.tsx` |
| `.status-sha` | `components/pages/files/upload/UploadStatusTable.tsx` |
| `.status-sha-head` | `components/pages/files/upload/UploadStatusTable.tsx` |
| `.status-sha-link` | `components/pages/files/upload/UploadStatusTable.tsx` |
| `.success-bar` | `components/pages/files/upload/ProgressBarContainer.tsx` |
| `.upload-bar` | `components/pages/files/upload/UploadForm.tsx`, `components/pages/files/upload/UploadStatusDashboard.tsx` |
| `.upload-btn` | `components/pages/files/upload/UploadForm.tsx` |
| `.upload-content` | `components/pages/files/upload/UploadStatusTable.tsx` |
| `.upload-field` | `components/pages/files/upload/UploadForm.tsx` |
| `.upload-field-name` | `components/pages/files/upload/UploadForm.tsx` |
| `.upload-field-name-alt` | `components/pages/files/upload/UploadForm.tsx` |
| `.upload_alerts` | `components/pages/files/upload/UploadForm.tsx` |
| `.warning-bar` | `components/pages/files/upload/ProgressBarContainer.tsx` |

### users.scss

| Class | Files |
|-------|-------|
| `.user-group-col` | `pages/users/UserBrowsing.tsx` |
| `.user-role-col` | `pages/users/UserBrowsing.tsx` |
| `.username-col` | `pages/users/UserBrowsing.tsx` |

---

## Unused Classes (safe to delete)

Classes defined in legacy SCSS files with zero `className` references in any `.tsx`/`.jsx` file.

| SCSS File | Class |
|-----------|-------|
| accordion.scss | `.accordion-list-buttons` |
| app.scss | `.nav-panel`, `.near-full-width`, `.secondary-panel` |
| buttons.scss | `.tlp-amber-btn`, `.tlp-clear-btn`, `.tlp-green-btn`, `.tlp-red-btn` |
| comments.scss | `.comment-box`, `.trash-comment` |
| details.scss | `.col-resize`, `.details-subtitle`, `.origin-head`, `.top-edit-tag-btn`, `.xxl-show-row` |
| overlays.scss | `.tooltip-inner` |
| reactions.scss | `.delete-card`, `.timestamp-short` |
| tags.scss | `.add-tag-btn`, `.attack-tag`, `.danger-tag`, `.info-tag`, `.mbc-tag`, `.other-tag`, `.tag-input`, `.warning-tag` |
| upload.scss | `.btn-group-xsm`, `.btn-no-outline-secondary`, `.btn-xsm`, `.card-title`, `.col-1`, `.col-md`, `.stat-card` |

---

## Summary

| SCSS File | Active Classes | Unused Classes | Total Component Files Affected |
|-----------|---------------|----------------|-------------------------------|
| accordion.scss | 4 | 1 | 3 |
| app.scss | 13 | 3 | 28 |
| buttons.scss | 10 | 4 | 25 |
| comments.scss | 4 | 2 | 2 |
| details.scss | 21 | 5 | 5 |
| groups.scss | 7 | 0 | 4 |
| logs.scss | 5 | 0 | 2 |
| overlays.scss | 1 | 1 | 1 |
| reactions.scss | 17 | 2 | 14 |
| results.scss | 9 | 0 | 5 |
| tags.scss | 5 | 8 | 2 |
| titles.scss | 7 | 0 | 20 |
| upload.scss | 28 | 7 | 9 |
| users.scss | 3 | 0 | 1 |
| **Total** | **134** | **33** | — |
