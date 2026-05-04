import React from 'react';
import { Dropdown, Form } from 'react-bootstrap';

import type { GraphSectionProps } from './types';
import { MenuList, MenuItem, MenuDropdown, PopoverBody, Divider } from './Toolbar.styled';

const GraphSection: React.FC<GraphSectionProps> = ({ controls, updateControls, graphInstance }) => (
  <PopoverBody>
    <MenuList $inset>
      <MenuDropdown>
        <Dropdown.Toggle variant="" size="sm" id="dimensions-dropdown">
          {controls.numDimensions}D
        </Dropdown.Toggle>
        <Dropdown.Menu>
          <Dropdown.Item onClick={() => updateControls({ type: 'numDimensions', state: 2 })}>2D</Dropdown.Item>
          <Dropdown.Item onClick={() => updateControls({ type: 'numDimensions', state: 3 })}>3D</Dropdown.Item>
        </Dropdown.Menu>
      </MenuDropdown>
      <MenuItem onClick={() => graphInstance?.zoomToFit(1000, 50)}>Fit All</MenuItem>
    </MenuList>

    <Divider />

    <Form.Check
      type="switch"
      id="form-focus-on-click"
      label="Focus on Click"
      checked={controls.focusOnClick}
      onChange={() => updateControls({ type: 'focusOnClick', state: !controls.focusOnClick })}
    />
  </PopoverBody>
);

export default GraphSection;
