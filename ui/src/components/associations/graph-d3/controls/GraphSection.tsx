import React from 'react';
import { Button, Form, Dropdown, ButtonGroup } from 'react-bootstrap';

import type { GraphSectionProps } from './types';
import { PopoverBody } from './Toolbar.styled';

const GraphSection: React.FC<GraphSectionProps> = ({ controls, updateControls, graphInstance }) => (
  <PopoverBody>
    <div style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
      <Form.Label htmlFor="form-select-depth" style={{ margin: 0, fontSize: '0.8rem', whiteSpace: 'nowrap' }}>
        Depth
      </Form.Label>
      <Form.Select
        id="form-select-depth"
        className="secondary-btn"
        size="sm"
        value={controls.depth}
        onChange={(e) => updateControls({ type: 'depth', state: parseInt(e.target.value, 10) })}
        style={{ width: '70px' }}
      >
        {Array.from({ length: 10 }, (_, i) => i + 1).map((d) => (
          <option key={d} value={d}>
            {d}
          </option>
        ))}
      </Form.Select>
    </div>

    <Dropdown as={ButtonGroup} style={{ width: '100%' }}>
      <Dropdown.Toggle className="secondary-btn" variant="" size="sm" id="dimensions-dropdown" style={{ width: '100%' }}>
        {controls.numDimensions}D
      </Dropdown.Toggle>
      <Dropdown.Menu>
        <Dropdown.Item onClick={() => updateControls({ type: 'numDimensions', state: 2 })}>2D</Dropdown.Item>
        <Dropdown.Item onClick={() => updateControls({ type: 'numDimensions', state: 3 })}>3D</Dropdown.Item>
      </Dropdown.Menu>
    </Dropdown>

    <div style={{ display: 'flex', gap: '6px' }}>
      <Button variant="" size="sm" className="secondary-btn" style={{ flex: 1 }} onClick={() => graphInstance?.d3ReheatSimulation()}>
        Reheat
      </Button>
      <Button variant="" size="sm" className="secondary-btn" style={{ flex: 1 }} onClick={() => graphInstance?.zoomToFit(1000, 50)}>
        Fit All
      </Button>
    </div>
  </PopoverBody>
);

export default GraphSection;
