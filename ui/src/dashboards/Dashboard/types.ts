// spec: ./SPEC.md

// project imports
import type { Seed } from '@models/trees';

/**
 * A stats series on the dashboard — one logical grouping of buckets rendered as a bar cluster.
 *
 * The stats layer is shaped as `Map<SeriesKey, BucketCount[]>` so a future time dimension can add
 * buckets per series without changing the series set. In v1 each series emits a single
 * `bucket: 'all'` count.
 */
export enum SeriesKey {
  /// File (sample) counts, bucketed by `FileType` tag with a filename-extension fallback.
  Files = 'files',
  /// Repository counts (single raw-count bucket in v1).
  Repos = 'repos',
  /// Entity counts, bucketed by entity kind (one bucket per `Entity.kind`).
  Entities = 'entities',
}

/**
 * The provenance of a stats bucket, which determines whether — and how — clicking its bar filters
 * the entity browser.
 *
 * The concrete clause a click injects is decided by the stats-click layer (a later phase); this
 * enum lets `deriveStats` tag each bucket with enough intent for that mapping and for
 * accessibility labels, without the stats layer importing omnibar clause builders.
 */
export enum BucketSource {
  /// Bucket keyed by an entity kind or the repo series — clicking injects an `Include` clause that
  /// whitelists that kind (narrows the browser).
  Kind = 'kind',
  /// Bucket keyed by a `FileType` tag value — clicking injects a tag clause matching that tag.
  Tag = 'tag',
  /// Bucket derived from a filename-extension fallback — there is no tag to filter on, so the bar
  /// is non-clickable.
  Extension = 'extension',
}

/**
 * A single bar in a stats series: a labeled count with click semantics and optional color.
 *
 * `clickable` is the single source of truth the UI reads to decide whether to render the bar as an
 * interactive `<button>`; `source` carries the reason so the click handler can build the right
 * clause. Extension-fallback buckets are always `clickable: false`.
 */
export interface BucketCount {
  /// The time bucket this count belongs to; `'all'` in v1 (temporal-ready).
  bucket: string;
  /// The human-readable label shown on/under the bar (e.g. a file type, entity kind, or extension).
  label: string;
  /// The count of items in this bucket.
  value: number;
  /// The bucket's provenance, used to build the click clause and the a11y label.
  source: BucketSource;
  /// Whether clicking the bar filters the browser. Always `false` for extension-fallback buckets.
  clickable: boolean;
  /// Optional bar color (CSS color / `--thorium-*` var); falls back to a series default when unset.
  color?: string;
}

/**
 * The full derived stats model consumed by the stats tile: each series mapped to its buckets.
 *
 * Keyed by {@link SeriesKey} so the tile can render series in a stable, known order and a future
 * temporal view only changes the `BucketCount[]` length per series.
 */
export type DashboardStats = Map<SeriesKey, BucketCount[]>;

/**
 * The decoded result of the dashboard's URL seed params: the {@link Seed} to build the graph from
 * plus the validated crawl depth.
 *
 * Returned by `decodeSeedParams` and accepted by `encodeSeedParams` so the URL ⇄ graph boundary is
 * a single round-trippable value.
 */
export interface DashboardSeedParams {
  /// The seed handed to `getInitialTree` / `GraphDataProvider`.
  seed: Seed;
  /// The validated, clamped crawl depth (0..=10, default 2).
  depth: number;
}
