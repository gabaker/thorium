import React from 'react';

// project imports
import { useEntityBrowser } from '../EntityBrowserContext';
import { SortMode } from '../types';
import { SelectChip, SortLabel, SortSelect } from './ControlsStyles';

/** Sort-mode dropdown choices, in the order they appear in the selector. */
const SORT_OPTIONS: readonly { value: SortMode; label: string }[] = [
  { value: SortMode.Flags, label: 'Flags' },
  { value: SortMode.Suspicion, label: 'Suspicion' },
  { value: SortMode.Confidence, label: 'Confidence' },
];

/**
 * Standalone sort controls for the entity browser, reading state from {@link useEntityBrowser}: a
 * dropdown selecting the primary flag-stat sort field (Flags / Suspicion / Confidence — the unselected two act
 * as descending tiebreakers) .
 * Rendered in the browser's own header row
 * (`BrowserHeader`) so it sits directly above the list, shared by the file-details tab and the dashboard.
 */
const EntitySortControls: React.FC = () => {
  const { sortMode, setSortMode } = useEntityBrowser();
  return (
    <SelectChip>
      <SortLabel htmlFor="entity-sort-mode">Sort:</SortLabel>
      <SortSelect
        id="entity-sort-mode"
        aria-label="Sort entities by"
        data-testid="entity-sort-mode"
        value={sortMode}
        onChange={(e) => setSortMode(e.target.value as SortMode)}
      >
        {SORT_OPTIONS.map((option) => (
          <option key={option.value} value={option.value}>
            {option.label}
          </option>
        ))}
      </SortSelect>
    </SelectChip>
  );
};

export default EntitySortControls;
