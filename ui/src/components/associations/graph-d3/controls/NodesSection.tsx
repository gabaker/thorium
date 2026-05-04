import React from 'react';
import { Form, Dropdown, ButtonGroup } from 'react-bootstrap';

import type { SectionProps } from './types';
import { PopoverBody, Divider } from './Toolbar.styled';
import LabeledRange from './LabeledRange';

const NodesSection: React.FC<SectionProps> = ({ controls, updateControls }) => (
  <PopoverBody>
    <Dropdown as={ButtonGroup} style={{ width: '100%' }}>
      <Dropdown.Toggle className="secondary-btn" variant="" size="sm" id="node-style-dropdown" style={{ width: '100%' }}>
        Style: {controls.nodeRenderMode}
      </Dropdown.Toggle>
      <Dropdown.Menu>
        <Dropdown.Item onClick={() => updateControls({ type: 'nodeRenderMode', state: 'spheres' })}>Spheres</Dropdown.Item>
        <Dropdown.Item onClick={() => updateControls({ type: 'nodeRenderMode', state: 'icons' })}>Icons</Dropdown.Item>
      </Dropdown.Menu>
    </Dropdown>

    <LabeledRange
      id="form-node-size"
      label="Size"
      value={controls.nodeRelSize}
      min={1}
      max={20}
      step={1}
      onChange={(v) => updateControls({ type: 'nodeRelSize', state: v })}
    />
    <LabeledRange
      id="form-node-opacity"
      label="Opacity"
      value={controls.nodeOpacity}
      min={0.1}
      max={1.0}
      step={0.05}
      onChange={(v) => updateControls({ type: 'nodeOpacity', state: v })}
    />

    <Divider />

    <Form.Check
      type="switch"
      id="form-show-node-labels"
      label="Labels"
      checked={controls.showNodeLabels}
      onChange={() => updateControls({ type: 'showNodeLabels', state: !controls.showNodeLabels })}
    />
    <Form.Check
      type="switch"
      id="form-node-drag"
      label="Draggable"
      checked={controls.enableNodeDrag}
      onChange={() => updateControls({ type: 'enableNodeDrag', state: !controls.enableNodeDrag })}
    />
    <Form.Check
      type="switch"
      id="form-focus-on-click"
      label="Focus on Click"
      checked={controls.focusOnClick}
      onChange={() => updateControls({ type: 'focusOnClick', state: !controls.focusOnClick })}
    />
  </PopoverBody>
);

export default NodesSection;
