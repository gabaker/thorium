import React, { useMemo } from 'react';
import { FaChevronDown, FaChevronUp, FaEyeSlash, FaXmark } from 'react-icons/fa6';

// spec: ./SPEC.md

// project imports
import { groupHiddenByType } from './hiddenGroups';
import {
  ClearHiddenButton,
  FiltersSection,
  FiltersToggleLabel,
  HiddenChip,
  HiddenTypeGroup,
  HiddenTypeLabel,
  OmnibarStrip,
  OmnibarStripSlot,
} from './styles';
import TagsTile from './TagsTile';
import { nodeTypeOf } from '@components/associations/browsing/EntityBrowser/browserHelpers';
import { useEntityBrowser } from '@components/associations/browsing/EntityBrowser/EntityBrowserContext';
import FlaggedOnlyToggle from '@components/associations/browsing/EntityBrowser/FlaggedOnlyToggle';
import { buildBrowserOmnibarOptions } from '@components/associations/browsing/EntityBrowser/omnibarOptions';
import { useGraphData } from '@components/associations/data/GraphDataContext';
import Collapsible, { TogglePosition } from '@components/shared/info/Collapsible';
import Omnibar from '@components/shared/inputs/omnibar/Omnibar';
import type { Clause } from '@components/shared/inputs/omnibar/ClauseTypes';
import { entityLabel } from '@models/entities';

/// The collapsed height (px) of the expandable filters section: a scrollable, bottom-faded window onto
/// the tags tile, tall enough to show a couple of tag-key tiles before the "filters" toggle expands it.
const FILTERS_COLLAPSED_MAX_PX = 200;

/// Render the filters-section toggle label: a sized chevron icon + "filters", flipping direction with the
/// collapsed state. The icon (not a unicode glyph) matches the label's size/baseline and stays centered.
function filtersToggleLabel(collapsed: boolean): React.ReactNode {
  return <FiltersToggleLabel>{collapsed ? <FaChevronDown size={12} /> : <FaChevronUp size={12} />}filters</FiltersToggleLabel>;
}

/// Props for {@link DashboardOmnibar}.
export interface DashboardOmnibarProps {
  /**
   * The shared clause state — the same value/setter handed to the surrounding {@link EntityBrowserProvider}
   * (URL-backed via `useOmnibarUrlState`). Kept as props (rather than read from context) so the dashboard's
   * stats-bar click handler and this strip mutate one authoritative clause list.
   */
  clauses: Clause[];
  /** Setter for the shared {@link clauses}. */
  setClauses: (next: Clause[]) => void;
}

/**
 * The dashboard's always-shown filter strip, sitting between the stats panel and the content tiles.
 *
 * Renders an {@link Omnibar} bound to the shared clause state, the hidden nodes as removable chips
 * **grouped by resource type** (each type headered, a chip-click unhides that node) with a single "Clear
 * all", and the {@link FlaggedOnlyToggle}. The omnibar's option lexicon (text/tag/group/Show/Hide/Exclude/Include/depth)
 * is built from the graph-derived `presentKinds`/`tagOptions`/`groupOptions` read from the surrounding
 * {@link EntityBrowserProvider}, so it offers exactly the browser's own vocabulary. The two toggle controls
 * also read that context, so this strip must be rendered inside the provider. Below the controls sits an
 * expandable "filters" section (a shared {@link Collapsible}) hosting the {@link TagsTile}, collapsed by
 * default: the capped area **scrolls** with a **static bottom fade** so the tags can be browsed without
 * expanding. Its toggle uses `TogglePosition.Adaptive`: while collapsed it sits **centered at the bottom**
 * (a "⌄ filters" expand affordance under the fade); once expanded it moves to the **top** (a "⌃ filters"
 * collapse control). The toggle appears once the tag content overflows the cap.
 *
 * @param clauses - The shared clause list (also given to the provider).
 * @param setClauses - Setter for the shared clause list.
 * @returns The omnibar strip.
 */
const DashboardOmnibar: React.FC<DashboardOmnibarProps> = ({ clauses, setClauses }) => {
  const { presentKinds, tagOptions, groupOptions, hiddenNodes, unhideNode, unhideAll, labelForNode } = useEntityBrowser();
  const { graph } = useGraphData();
  const dropdownOptions = useMemo(
    () => buildBrowserOmnibarOptions(presentKinds, tagOptions, groupOptions),
    [presentKinds, tagOptions, groupOptions],
  );
  // hidden ids are URL-backed, so surface them in the strip grouped by resource type: one removable chip
  // per hidden node (a click unhides it) under its type header, plus a single "Clear all" — a unified,
  // discoverable place to see and undo every hidden thing
  const hiddenGroups = useMemo(
    () => groupHiddenByType(Array.from(hiddenNodes), (id) => nodeTypeOf(id, graph), entityLabel),
    [hiddenNodes, graph],
  );
  return (
    <OmnibarStrip>
      <OmnibarStripSlot>
        <Omnibar clauses={clauses} setClauses={setClauses} dropdownOptions={dropdownOptions} placeholder="Filter entities…" />
      </OmnibarStripSlot>
      {hiddenGroups.map((group) => (
        <HiddenTypeGroup key={group.type}>
          <HiddenTypeLabel>{group.label}</HiddenTypeLabel>
          {group.ids.map((id) => {
            const label = labelForNode(id);
            return (
              <HiddenChip key={id} type="button" aria-label={`Unhide ${label}`} onClick={() => unhideNode(id)}>
                <FaEyeSlash size={12} aria-hidden />
                <span title={label}>{label}</span>
                <FaXmark size={12} aria-hidden />
              </HiddenChip>
            );
          })}
        </HiddenTypeGroup>
      ))}
      {hiddenGroups.length > 0 && (
        <ClearHiddenButton type="button" onClick={unhideAll}>
          Clear all
        </ClearHiddenButton>
      )}
      <FlaggedOnlyToggle />
      <FiltersSection>
        <Collapsible
          maxPx={FILTERS_COLLAPSED_MAX_PX}
          renderToggleLabel={filtersToggleLabel}
          togglePosition={TogglePosition.Adaptive}
          scrollWhenCollapsed
        >
          <TagsTile clauses={clauses} setClauses={setClauses} />
        </Collapsible>
      </FiltersSection>
    </OmnibarStrip>
  );
};

export default DashboardOmnibar;
