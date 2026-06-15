// spec: ./SPEC.md

// project imports
import { BucketSource, SeriesKey, type BucketCount, type DashboardStats } from './types';
import { ClauseCondition, type Clause } from '@components/shared/inputs/omnibar/ClauseTypes';
import { Entities } from '@models/entities';
import type { Sample } from '@models/files';
import { TreeNode, TreeNodeKey } from '@models/trees';

/**
 * The tag key that carries a file's classified type (e.g. `PE32`, `ELF`, `PDF`).
 *
 * Buckets derived from this tag are clickable and inject a tag clause; when a file lacks this tag we
 * fall back to its filename extension (a non-clickable bucket). Kept in sync with the `FileType` entry
 * in {@link FileInfoTagKeys} (`@components/tags/tag_groups`).
 */
const FILE_TYPE_TAG_KEY = 'FileType';

/**
 * The single time-bucket key emitted in v1.
 *
 * {@link DashboardStats} is shaped `Map<SeriesKey, BucketCount[]>` so a future temporal view can emit
 * multiple buckets per series; today every count belongs to this one `'all'` bucket.
 */
const BUCKET_ALL = 'all';

/**
 * The omnibar clause `field`/`category` that whitelists (narrows to) an entity/node kind.
 *
 * Matches the entity-layer lexicon added by `addEntityLayerOptions` and read back by
 * `getEntityLayerConfigFromClauses`/`getStringFieldListFromClauses(clauses, 'Include')`; the value is a
 * raw {@link NodeType} enum value so it maps straight to a layer-policy key.
 */
const INCLUDE_FIELD = 'Include';

/**
 * Build the `Include` (kind-whitelist) omnibar clause for a node kind.
 *
 * The single source of the Types-click clause contract, imported by both {@link barToClause} and the stats
 * panel so the clause shape can't drift between them. Narrowing requires an `Include` (whitelist) rather
 * than a `Show`, because the browser's default `Show` fallback already renders every kind.
 *
 * @param kind - The raw entity/node kind to whitelist (the clause value).
 * @returns The `Include` clause for `kind`.
 */
export function makeIncludeClause(kind: string): Clause {
  return {
    category: INCLUDE_FIELD,
    field: INCLUDE_FIELD,
    condition: ClauseCondition.Is,
    value: { value: kind },
  };
}

/**
 * The omnibar clause `category` for a tag filter.
 *
 * Tag clauses use `category: 'tag'` with `field` set to the tag key; `getTagsFromClauses` reads them by
 * this category (excluding the `'hidden tags'` display filter).
 */
const TAG_CATEGORY = 'tag';

/**
 * Extract a lowercased filename extension (including the leading dot) from a submission name.
 *
 * A name with no extension (no dot, or a leading-dot dotfile) yields `'(none)'`.
 *
 * @param name - The submission file name.
 * @returns The lowercased extension with its dot (e.g. `.exe`), or `'(none)'` when absent.
 */
function extractExtension(name: string): string {
  const dot = name.lastIndexOf('.');
  if (dot < 1) return '(none)';
  return name.substring(dot).toLowerCase();
}

/**
 * Read a sample's `FileType` tag value, if the tag is present.
 *
 * `Sample.tags` is shaped `{ [key]: { [value]: groups[] } }`, so the value(s) are the keys of the
 * `FileType` entry. When a sample carries multiple `FileType` values, the first (sorted for
 * determinism) is used so a sample contributes to exactly one type bucket.
 *
 * @param sample - The sample whose file-type tag to read.
 * @returns The chosen `FileType` value, or `null` when the tag is absent/empty.
 */
function fileTypeTagValue(sample: Sample): string | null {
  const tagMap = sample.tags?.[FILE_TYPE_TAG_KEY];
  if (!tagMap) return null;
  const values = Object.keys(tagMap);
  if (values.length === 0) return null;
  return values.sort()[0];
}

/**
 * Accumulate a labeled count into a running map, preserving first-seen order via the map iteration order.
 *
 * @param counts - The map from label to running count.
 * @param label - The bucket label to increment.
 */
function bump(counts: Map<string, number>, label: string): void {
  counts.set(label, (counts.get(label) ?? 0) + 1);
}

/**
 * Convert a label→count map into a sorted {@link BucketCount} list (descending by count).
 *
 * @param counts - The accumulated label→count map.
 * @param source - The provenance stamped on every bucket.
 * @param clickable - Whether the resulting bars filter the browser when clicked.
 * @returns The buckets, highest count first.
 */
function toBuckets(counts: Map<string, number>, source: BucketSource, clickable: boolean): BucketCount[] {
  return Array.from(counts.entries())
    .map(([label, value]) => ({ bucket: BUCKET_ALL, label, value, source, clickable }))
    .sort((a, b) => b.value - a.value);
}

/**
 * Derive the dashboard stats model from a graph's `data_map`.
 *
 * Walks every node once, discriminating on {@link TreeNodeKey}:
 *
 * - **Files** are grouped by their `FileType` tag (`source: Tag`, clickable). A file with no `FileType`
 *   tag falls back to a bucket per filename extension (`source: Extension`, non-clickable — there is no
 *   tag to filter on, so a clause would match nothing). A file with several submission names contributes
 *   to each distinct extension it carries; a tagged file never contributes an extension bucket.
 * - **Repos** collapse into a single raw-count bar (`source: Kind`, clickable), keyed by the repo kind
 *   so a click can whitelist repos.
 * - **Entities** yield one bar per `Entity.kind` (raw counts, `source: Kind`, clickable), falling back to
 *   the `Other` kind for a kind-less/edge node.
 * - **Tag** nodes are counted toward no series (they are not a stats series in v1).
 *
 * The result is a `Map<SeriesKey, BucketCount[]>`; a series with no items is omitted so an empty
 * `data_map` yields an empty map. The function is pure — it neither mutates its input nor reads context.
 *
 * @param dataMap - The graph's `data_map` (`{ [nodeId]: TreeNode }`).
 * @returns The derived {@link DashboardStats}.
 */
export function deriveStats(dataMap: Record<string, TreeNode>): DashboardStats {
  const stats: DashboardStats = new Map();
  // per-series accumulators; each maps a bucket label to its running count
  const fileTypeCounts = new Map<string, number>();
  const extensionCounts = new Map<string, number>();
  let repoCount = 0;
  const entityKindCounts = new Map<string, number>();
  for (const node of Object.values(dataMap)) {
    if (TreeNodeKey.Sample in node && node[TreeNodeKey.Sample]) {
      const sample = node[TreeNodeKey.Sample];
      const fileType = fileTypeTagValue(sample);
      if (fileType !== null) {
        // a classified file contributes exactly one clickable FileType bucket
        bump(fileTypeCounts, fileType);
      } else {
        // untagged files fall back to a (non-clickable) bucket per distinct submission extension
        const extensions = new Set<string>();
        for (const submission of sample.submissions ?? []) {
          if (submission.name) extensions.add(extractExtension(submission.name));
        }
        // a file with no named submissions still counts once under the "(none)" extension
        if (extensions.size === 0) extensions.add('(none)');
        for (const ext of extensions) bump(extensionCounts, ext);
      }
    } else if (TreeNodeKey.Repo in node && node[TreeNodeKey.Repo]) {
      repoCount++;
    } else if (TreeNodeKey.Entity in node && node[TreeNodeKey.Entity]) {
      const kind = node[TreeNodeKey.Entity].kind ?? Entities.Other;
      bump(entityKindCounts, String(kind));
    }
  }
  // Files: clickable FileType-tag buckets first, then non-clickable extension-fallback buckets
  const fileBuckets = [...toBuckets(fileTypeCounts, BucketSource.Tag, true), ...toBuckets(extensionCounts, BucketSource.Extension, false)];
  if (fileBuckets.length > 0) {
    stats.set(SeriesKey.Files, fileBuckets);
  }
  // Repos: a single raw-count, clickable (kind) bar
  if (repoCount > 0) {
    stats.set(SeriesKey.Repos, [
      { bucket: BUCKET_ALL, label: Entities.Repo, value: repoCount, source: BucketSource.Kind, clickable: true },
    ]);
  }
  // Entities: one clickable (kind) bar per entity kind
  const entityBuckets = toBuckets(entityKindCounts, BucketSource.Kind, true);
  if (entityBuckets.length > 0) {
    stats.set(SeriesKey.Entities, entityBuckets);
  }
  return stats;
}

/**
 * Collect the deduped sha256 of every file (`Sample`) node in a graph's `data_map`.
 *
 * Walks the same `Sample` nodes as {@link deriveStats}, reading each `.Sample.sha256`, and returns them
 * de-duplicated in first-seen order. Pure — it neither mutates its input nor reads context. Used by the
 * dashboard's Analysis Status panel to fan out reaction lookups over the dashboard's files.
 *
 * @param dataMap - The graph's `data_map` (`{ [nodeId]: TreeNode }`).
 * @returns The distinct file sha256s present in the graph, in first-seen order.
 */
export function collectSampleSha256s(dataMap: Record<string, TreeNode>): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const node of Object.values(dataMap)) {
    if (TreeNodeKey.Sample in node && node[TreeNodeKey.Sample]) {
      const sha256 = node[TreeNodeKey.Sample].sha256;
      if (sha256 && !seen.has(sha256)) {
        seen.add(sha256);
        out.push(sha256);
      }
    }
  }
  return out;
}

/**
 * Map a clicked stats bar to the omnibar {@link Clause} it should inject into the entity browser.
 *
 * The mapping follows the dashboard click contract:
 *
 * - **Kind buckets** (entity kinds and the repo series, `source: Kind`) inject an `Include` clause that
 *   whitelists that kind. With the browser's default `Show` fallback every kind renders, so narrowing
 *   requires an `Include` (whitelist) rather than a `Show`; the value is the bucket label (a raw kind).
 * - **Tag buckets** (`source: Tag`, i.e. `FileType`-tagged file bars) inject an `IsOneOf` tag clause
 *   matching the `FileType` key against the bucket's value; the dashboard merges repeated same-key clicks
 *   into one "any of" clause (`toggleTagValue`), which `matchesTags` uses to keep only nodes carrying it.
 * - **Extension buckets** (`source: Extension`) are non-clickable and return `null` — there is no tag to
 *   filter on, so any injected clause would match nothing.
 *
 * @param series - The series the clicked bar belongs to (unused for the mapping today, kept so the
 *   contract can diverge per series without a signature change).
 * @param bucket - The clicked bucket.
 * @returns The clause to inject, or `null` when the bucket is not clickable.
 */
export function barToClause(series: SeriesKey, bucket: BucketCount): Clause | null {
  switch (bucket.source) {
    case BucketSource.Kind:
      // whitelist this kind: field/category 'Include', value is the raw kind label
      return makeIncludeClause(bucket.label);
    case BucketSource.Tag:
      // match the FileType tag against this bucket's value as an is-one-of set so repeated same-key
      // clicks merge into a single "any of" clause (see toggleTagValue in tagFilter.ts)
      return {
        category: TAG_CATEGORY,
        field: FILE_TYPE_TAG_KEY,
        condition: ClauseCondition.IsOneOf,
        value: { values: [bucket.label] },
      };
    case BucketSource.Extension:
      // extension-fallback buckets have nothing to filter on
      return null;
  }
}
