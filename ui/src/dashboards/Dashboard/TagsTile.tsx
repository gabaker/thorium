import React, { useMemo } from 'react';
import { FaChevronDown, FaChevronUp } from 'react-icons/fa6';

// spec: ./SPEC.md

// project imports
import { visibleNodes } from './nodes';
import { ALWAYS_HIDDEN_TAG_KEYS, collectTagCounts, type TagValueCount } from './tagCounts';
import { EmptyTags, TagChip, TagChipCount, TagGroup, TagGroupKey, TagGroupValues, TagValuesToggle } from './styles';
import { toggleTagValue } from './tagFilter';
import { useEntityBrowser } from '@components/associations/browsing/EntityBrowser/EntityBrowserContext';
import { useGraphData } from '@components/associations/data/GraphDataContext';
import Collapsible, { TogglePosition } from '@components/shared/info/Collapsible';
import { BalancedColumns } from '@components/shared/layout/BalancedColumns';
import { OverlayTipTop } from '@components/shared/overlay/tips';
import { ClauseCondition, ClauseIsMulti, type Clause } from '@components/shared/inputs/omnibar/ClauseTypes';
import { getHiddenTagsFromClauses } from '@components/shared/inputs/omnibar/utils';
import { spacers } from '@styles';

/// Props for {@link TagsTile}.
export interface TagsTileProps {
  /**
   * The shared clause state — the same value/setter handed to the surrounding `EntityBrowserProvider`
   * (URL-backed). Kept as props (rather than read from context) so the tile, the omnibar strip, and the
   * stats-bar clicks all mutate one authoritative clause list.
   */
  clauses: Clause[];
  /** Setter for the shared {@link clauses}. */
  setClauses: (next: Clause[]) => void;
}

/// The tag keys that are always hidden even when no `hidden tags` clause is present.
const DEFAULT_HIDDEN_KEYS = ['Results', 'Parent', 'submitter'];

/// The minimum width (px) of a per-key tag tile; `BalancedColumns` derives the column count from this so
/// wide screens pack more (narrower) columns while each tile stays only as tall as its content.
const TAG_TILE_MIN_WIDTH = 200;

/// The collapsed height cap (px) of a tag tile's value chips — about 3–4 chip rows. Taller tiles clip the
/// extra chips behind the shared bottom fade and reveal a caret to expand; short tiles show no caret.
const TAG_TILE_VALUES_MAX_PX = 120;

/// Render the caret-only toggle for a tag tile's value collapse: a down chevron while collapsed (expand)
/// and an up chevron while expanded (collapse). The visible glyph is icon-only, so the accessible name is
/// carried by the wrapper's `aria-label` while the icon is `aria-hidden`.
function tagValuesToggleLabel(collapsed: boolean): React.ReactNode {
  return (
    <TagValuesToggle aria-label={collapsed ? 'Show all values' : 'Show fewer values'}>
      {collapsed ? <FaChevronDown size={12} aria-hidden /> : <FaChevronUp size={12} aria-hidden />}
    </TagValuesToggle>
  );
}

/**
 * Precompute the set of active (already-filtered) tag values per key from the clause list.
 *
 * Building this once per clause change is O(clauses) total, versus the previous per-chip
 * `isTagValueActive` scan that was O(clauses) *per rendered chip* (and called twice per chip). Lookups
 * against the returned map are then O(1) while rendering.
 *
 * @param clauses - The current clause list.
 * @returns A map from tag key to the set of its active `IsOneOf` values.
 */
function activeTagValues(clauses: Clause[]): Map<string, Set<string>> {
  const active = new Map<string, Set<string>>();
  for (const clause of clauses) {
    if (clause.category === 'tag' && clause.condition === ClauseCondition.IsOneOf && ClauseIsMulti(clause)) {
      let values = active.get(clause.field);
      if (values === undefined) {
        values = new Set<string>();
        active.set(clause.field, values);
      }
      for (const value of clause.value.values) values.add(value);
    }
  }
  return active;
}

/**
 * The dashboard's tags tile: every tag present in the current (visible) node set, grouped by key, each
 * value a clickable chip showing its count.
 *
 * Counts over the entity browser's `visibleSet` when a filter is active (so the tile *downselects* — a
 * value that no longer appears in the filtered view drops out) and over the whole `data_map` otherwise.
 * The display-hidden tag keys (`getHiddenTagsFromClauses`, defaulting to `Results`/`Parent`/`submitter`)
 * are excluded, as are the always-hidden high-cardinality keys ({@link ALWAYS_HIDDEN_TAG_KEYS}) which are
 * dropped unconditionally. Clicking a chip toggles its value into the key's single is-one-of filter via
 * {@link toggleTagValue}; already-filtered values render active and sort **first** within their tile so they
 * stay visible when the tile is collapsed. Each tile's value chips are wrapped in a shared {@link Collapsible}
 * (default collapsed): a tile taller than {@link TAG_TILE_VALUES_MAX_PX} clips its extra chips behind the
 * static bottom fade and reveals a caret-only toggle to expand; short tiles show no caret. The tally is
 * memoized on `[graphVersion, visibleSet, hiddenKeys]` so it recomputes only when the graph, the visible set,
 * or the hidden-key exclusion set change; the active-value lookup is precomputed once per clause change.
 *
 * @param clauses - The shared clause list (also given to the provider).
 * @param setClauses - Setter for the shared clause list.
 * @returns The tags tile.
 */
const TagsTile: React.FC<TagsTileProps> = ({ clauses, setClauses }) => {
  const { graph, graphVersion } = useGraphData();
  const { visibleSet } = useEntityBrowser();

  // the set of tag keys to exclude from the tally: the omnibar-derived hidden keys (or the default
  // fallback) plus the always-hidden high-cardinality keys. Derived in its own memo (on `clauses`) so the
  // count walk below recomputes only when the *hidden keys* change, not on every unrelated clause edit.
  const hiddenKeys = useMemo(() => {
    const fromClauses = getHiddenTagsFromClauses(clauses);
    return [...(fromClauses.length > 0 ? fromClauses : DEFAULT_HIDDEN_KEYS), ...ALWAYS_HIDDEN_TAG_KEYS];
  }, [clauses]);

  const tagCounts = useMemo(() => {
    // node set = only the visible ids when a filter is active, otherwise every node in the graph
    const nodes = visibleNodes(graph.data_map, visibleSet);
    return collectTagCounts(nodes, hiddenKeys);
    // graphVersion is bumped whenever data_map changes (graph itself is a stable ref); visibleSet drives
    // the downselect; hiddenKeys drives the exclusion set
  }, [graph, graphVersion, visibleSet, hiddenKeys]);

  // active (already-filtered) values per key, computed once per clause change for O(1) chip lookups
  const activeValues = useMemo(() => activeTagValues(clauses), [clauses]);

  const keys = Array.from(tagCounts.keys()).sort((a, b) => a.localeCompare(b));

  if (keys.length === 0) {
    return <EmptyTags>No tags</EmptyTags>;
  }

  return (
    <BalancedColumns
      columnWidth={TAG_TILE_MIN_WIDTH}
      gap={spacers.two}
      items={keys.map((key) => {
        // order active (already-filtered) values first — each partition keeping its descending-count order —
        // so applied filters stay visible in the collapsed preview under the fade. Active-ness depends on
        // `clauses`, so it's partitioned here in the render rather than baked into `collectTagCounts`.
        const values = tagCounts.get(key) ?? [];
        const activeForKey = activeValues.get(key);
        const active: TagValueCount[] = [];
        const inactive: TagValueCount[] = [];
        for (const entry of values) {
          (activeForKey?.has(entry.value) ? active : inactive).push(entry);
        }
        const ordered = [...active, ...inactive];
        return (
          <TagGroup key={key}>
            <TagGroupKey>{key}</TagGroupKey>
            <Collapsible maxPx={TAG_TILE_VALUES_MAX_PX} renderToggleLabel={tagValuesToggleLabel} togglePosition={TogglePosition.Adaptive}>
              <TagGroupValues>
                {ordered.map(({ value, count }) => {
                  const isActive = activeForKey?.has(value) ?? false;
                  // tell the user a click filters the dashboard by this tag (and that an active chip removes it)
                  const tip = isActive ? `Remove the ${key}: ${value} filter` : `Filter the dashboard to items tagged ${key}: ${value}`;
                  return (
                    <OverlayTipTop key={value} tip={tip}>
                      <TagChip
                        type="button"
                        $active={isActive}
                        aria-pressed={isActive}
                        onClick={() => setClauses(toggleTagValue(clauses, key, value))}
                      >
                        {value}
                        <TagChipCount>({count})</TagChipCount>
                      </TagChip>
                    </OverlayTipTop>
                  );
                })}
              </TagGroupValues>
            </Collapsible>
          </TagGroup>
        );
      })}
    />
  );
};

export default TagsTile;
