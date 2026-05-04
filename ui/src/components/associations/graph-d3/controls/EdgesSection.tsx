import React from 'react';
import { Form } from 'react-bootstrap';

import type { SectionProps } from './types';
import { PopoverBody, Divider } from './Toolbar.styled';
import LabeledRange from './LabeledRange';

const EdgesSection: React.FC<SectionProps> = ({ controls, updateControls }) => (
  <PopoverBody>
    <LabeledRange
      id="form-edge-width"
      label="Width"
      value={controls.edgeWidth}
      min={0.1}
      max={2}
      step={0.1}
      onChange={(v) => updateControls({ type: 'edgeWidth', state: v })}
    />
    <LabeledRange
      id="form-edge-length"
      label="Length"
      value={controls.edgeLength}
      min={10}
      max={200}
      step={10}
      onChange={(v) => updateControls({ type: 'edgeLength', state: v })}
    />
    <LabeledRange
      id="form-link-strength"
      label="Strength"
      value={controls.edgeLinkStrength}
      min={0.1}
      max={2.0}
      step={0.1}
      onChange={(v) => updateControls({ type: 'edgeLinkStrength', state: v })}
    />
    <LabeledRange
      id="form-edge-opacity"
      label="Opacity"
      value={controls.edgeOpacity}
      min={0.1}
      max={1.0}
      step={0.05}
      onChange={(v) => updateControls({ type: 'edgeOpacity', state: v })}
    />
    <LabeledRange
      id="form-arrow-length"
      label="Arrows"
      value={controls.arrowLength}
      min={0}
      max={15}
      step={0.5}
      onChange={(v) => updateControls({ type: 'arrowLength', state: v })}
    />

    <Divider />

    <LabeledRange
      id="form-particles"
      label="Particles"
      value={controls.directionalParticles}
      min={0}
      max={10}
      step={1}
      onChange={(v) => updateControls({ type: 'directionalParticles', state: v })}
    />
    {controls.directionalParticles > 0 && (
      <LabeledRange
        id="form-particle-speed"
        label="Speed"
        value={controls.particleSpeed}
        min={0.001}
        max={0.05}
        step={0.001}
        onChange={(v) => updateControls({ type: 'particleSpeed', state: v })}
      />
    )}

    <Divider />

    <Form.Check
      type="switch"
      id="form-show-edge-labels"
      label="Labels"
      checked={controls.showEdgeLabels}
      onChange={() => updateControls({ type: 'showEdgeLabels', state: !controls.showEdgeLabels })}
    />
  </PopoverBody>
);

export default EdgesSection;
