// spec: ./EntityBrowser.spec.md
import React, { useMemo } from 'react';
import { FaFlag } from 'react-icons/fa6';

// project imports
import { useEntityBrowser } from './EntityBrowserContext';
import { OmnibarSlot, ToggleChip, ToolbarBar } from './EntityBrowser.styled';
import Omnibar from '@components/shared/inputs/omnibar/Omnibar';
import {
  addDepthOptions,
  addEntityLayerOptions,
  addGroupOptions,
  addTagOptions,
  addTextOptions,
  OmnibarOptionMap,
} from '@components/shared/inputs/omnibar/options';

// upper bound for the traversal-depth omnibar option
const MAX_DEPTH = 10;

/**
 * Omnibar-driven filter bar: text (name), tags, groups, the `Show`/`Hide`/`Exclude`/`Include` entity-layer
 * lexicon, and a traversal `depth`. Tag/group options come from the pulled graph (no extra request). A
 * separate "Flagged Only" toggle keeps the danger/Flag filter one click away.
 */
const BrowserToolbar: React.FC = () => {
  const { clauses, setClauses, flaggedOnly, setFlaggedOnly, presentKinds, tagOptions, groupOptions } = useEntityBrowser();

  const dropdownOptions = useMemo<OmnibarOptionMap>(() => {
    let opts: OmnibarOptionMap = {};
    opts = addTextOptions(opts);
    opts = addTagOptions(opts, tagOptions);
    opts = addGroupOptions(opts, groupOptions);
    opts = addEntityLayerOptions(opts, presentKinds);
    opts = addDepthOptions(opts, MAX_DEPTH);
    return opts;
  }, [tagOptions, groupOptions, presentKinds]);

  return (
    <ToolbarBar>
      <OmnibarSlot>
        <Omnibar clauses={clauses} setClauses={setClauses} dropdownOptions={dropdownOptions} placeholder="Filter entities…" />
      </OmnibarSlot>
      <ToggleChip
        type="button"
        $active={flaggedOnly}
        data-testid="entity-browser-flagged"
        aria-pressed={flaggedOnly}
        onClick={() => setFlaggedOnly(!flaggedOnly)}
      >
        <FaFlag size={12} /> Flagged Only
      </ToggleChip>
    </ToolbarBar>
  );
};

export default BrowserToolbar;
