import React from 'react';
import { Dropdown } from 'react-bootstrap';

import type { GraphSectionProps } from './types';
import { MenuList, MenuItem, MenuDropdown } from './Toolbar.styled';

const GraphSection: React.FC<GraphSectionProps> = ({ controls, updateControls, graphInstance }) => (
  <MenuList>
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
);

export default GraphSection;
