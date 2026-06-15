// spec: ./SPEC.md

import React from 'react';
import styled from 'styled-components';

// project imports
import { BROWSABLE_KINDS, BrowseMode, TAG_MODE } from './types';
import { OverlayTipRight } from '@components/shared/overlay/tips';
import { spacers } from '@styles';
import { entityLabel } from '@models/entities/entities';

/**
 * A themed native `<select>` for the resource-type picker.
 *
 * A native select (styled) rather than the creatable `SelectInput` because the choices are a fixed,
 * non-creatable enum — the user picks a type, they never type a new one — and the native control
 * gives free keyboard/accessibility behavior. Styling mirrors the `--thorium-*` conventions used by
 * the other themed inputs (e.g. `ScrollableSelect`).
 */
const TypeSelect = styled.select`
  min-width: 200px;
  padding: ${spacers.three} ${spacers.four};
  border: 1px solid var(--thorium-panel-border);
  border-radius: 8px;
  background: var(--thorium-secondary-panel-bg);
  color: var(--thorium-text);
  font-size: 0.9rem;
  cursor: pointer;
`;

/**
 * Props for {@link ResourceTypePicker}.
 */
interface ResourceTypePickerProps {
  /// The currently selected browse mode (an entity kind or Tag mode).
  mode: BrowseMode;
  /// Called with the new mode when the user picks a different type.
  onChange: (mode: BrowseMode) => void;
}

/**
 * The dropdown that chooses which resource type to browse (File, Repo, entity kinds) or Tag mode.
 *
 * Rendered directly above the filters omnibar so it reads as controlling what the omnibar/list below
 * shows. Selecting an entity kind drives the config-driven browse list; selecting Tag mode swaps the
 * list for a `TagSelect`-based key/value entry. The list of kinds comes from {@link BROWSABLE_KINDS} so
 * the order is stable.
 *
 * @param props - See {@link ResourceTypePickerProps}.
 * @returns The resource-type picker control.
 */
const ResourceTypePicker: React.FC<ResourceTypePickerProps> = ({ mode, onChange }) => {
  return (
    <OverlayTipRight tip="Choose what to browse and add to the dashboard. Pick Tag to seed by a tag key/value instead of a specific item.">
      <TypeSelect aria-label="Resource type to browse" value={mode} onChange={(event) => onChange(event.target.value as BrowseMode)}>
        {BROWSABLE_KINDS.map((kind) => (
          <option key={kind} value={kind}>
            {entityLabel(kind)}
          </option>
        ))}
        <option value={TAG_MODE}>Tag</option>
      </TypeSelect>
    </OverlayTipRight>
  );
};

export default ResourceTypePicker;
