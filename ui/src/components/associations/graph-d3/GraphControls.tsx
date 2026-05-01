import React from 'react';
import { Button, Form, Dropdown, ButtonGroup } from 'react-bootstrap';
import styled from 'styled-components';
import type { ForceGraph3DInstance } from '3d-force-graph';

import { spacers } from '@styles';
import { exportJPEG, exportPNG } from './export';

export type NodeRenderMode = 'spheres' | 'icons';
export type DagMode = 'td' | 'bu' | 'lr' | 'rl' | 'zout' | 'zin' | 'radialout' | 'radialin' | null;

export interface GraphControls {
  filterChildless: boolean;
  depth: number;
  showEdgeLabels: boolean;
  showNodeLabels: boolean;
  selectedElement: SelectedElement | null;
  showNodeInfo: boolean;
  nodeRenderMode: NodeRenderMode;
  focusOnClick: boolean;
  // edges
  edgeWidth: number;
  edgeLength: number;
  edgeOpacity: number;
  arrowLength: number;
  directionalParticles: number;
  particleSpeed: number;
  // nodes
  nodeRelSize: number;
  nodeOpacity: number;
  enableNodeDrag: boolean;
  // forces
  chargeStrength: number;
  velocityDecay: number;
  warmupTicks: number;
  cooldownTime: number;
  // layout
  dagMode: DagMode;
  dagLevelDistance: number | null;
  numDimensions: 2 | 3;
}

export type SelectedElement = { kind: 'node'; id: string; label: string } | { kind: 'link'; source: string; target: string; label: string };

export type DisplayAction =
  | { type: 'depth'; state: number }
  | { type: 'filterChildless' | 'showEdgeLabels' | 'showNodeLabels' | 'showNodeInfo' | 'focusOnClick' | 'enableNodeDrag'; state: boolean }
  | { type: 'selected'; state: SelectedElement | null }
  | { type: 'nodeRenderMode'; state: NodeRenderMode }
  | { type: 'edgeWidth' | 'edgeLength' | 'edgeOpacity' | 'arrowLength' | 'directionalParticles' | 'particleSpeed'; state: number }
  | { type: 'nodeRelSize' | 'nodeOpacity'; state: number }
  | { type: 'chargeStrength' | 'velocityDecay' | 'warmupTicks' | 'cooldownTime'; state: number }
  | { type: 'dagMode'; state: DagMode }
  | { type: 'dagLevelDistance'; state: number | null }
  | { type: 'numDimensions'; state: 2 | 3 };

type GraphControlsProps = {
  graphId: string;
  controls: GraphControls;
  updateControls: React.ActionDispatch<[action: DisplayAction]>;
  graphInstance: ForceGraph3DInstance | null;
};

const Controls = styled.div`
  top: 0px;
  left: 0px;
  z-index: 400;
  display: grid;
  grid-auto-flow: column dense;
  align-items: start;
  justify-content: start;
  gap: ${spacers.two};
  margin-left: ${spacers.four};
`;

const ControlGroup = styled.div`
  display: grid;
  row-gap: ${spacers.one};
  justify-items: center;
  & > label {
    font-size: 0.85rem;
    margin: 0;
  }
`;

const SectionLabel = styled.div`
  font-size: 0.75rem;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  opacity: 0.6;
  margin-bottom: 2px;
`;

const DAG_MODE_LABELS: Record<string, string> = {
  none: 'None',
  td: 'Top-Down',
  bu: 'Bottom-Up',
  lr: 'Left-Right',
  rl: 'Right-Left',
  zout: 'Z-Out',
  zin: 'Z-In',
  radialout: 'Radial Out',
  radialin: 'Radial In',
};

export const GraphControlsPanel: React.FC<GraphControlsProps> = ({ graphId, controls, updateControls, graphInstance }) => (
  <Controls>
    {/* Graph section */}
    <ControlGroup>
      <SectionLabel>Graph</SectionLabel>
      <Form.Select
        id="form-select-depth"
        className="secondary-btn"
        value={controls.depth}
        onChange={(e) => updateControls({ type: 'depth', state: parseInt(e.target.value, 10) })}
        style={{ width: '75px' }}
      >
        {Array.from({ length: 10 }, (_, idx) => idx + 1).map((depth) => (
          <option key={depth} value={depth}>
            {depth}
          </option>
        ))}
      </Form.Select>
      <Form.Label htmlFor="form-select-depth">Depth</Form.Label>
      <Button variant="" className="secondary-btn" onClick={() => graphInstance?.d3ReheatSimulation()}>
        Reheat
      </Button>
      <Button variant="" className="secondary-btn" onClick={() => graphInstance?.zoomToFit(1000, 50)}>
        Fit All
      </Button>
      <Dropdown as={ButtonGroup}>
        <Dropdown.Toggle className="secondary-btn" variant="" id="download-dropdown">
          Download
        </Dropdown.Toggle>
        <Dropdown.Menu>
          <Dropdown.Item onClick={() => exportPNG(graphId, graphInstance)}>PNG</Dropdown.Item>
          <Dropdown.Item onClick={() => exportJPEG(graphId, graphInstance)}>JPEG</Dropdown.Item>
        </Dropdown.Menu>
      </Dropdown>
      <Dropdown as={ButtonGroup}>
        <Dropdown.Toggle className="secondary-btn" variant="" id="dimensions-dropdown">
          {controls.numDimensions}D
        </Dropdown.Toggle>
        <Dropdown.Menu>
          <Dropdown.Item onClick={() => updateControls({ type: 'numDimensions', state: 2 })}>2D</Dropdown.Item>
          <Dropdown.Item onClick={() => updateControls({ type: 'numDimensions', state: 3 })}>3D</Dropdown.Item>
        </Dropdown.Menu>
      </Dropdown>
    </ControlGroup>

    {/* Forces section */}
    <ControlGroup>
      <SectionLabel>Forces</SectionLabel>
      <Form.Label htmlFor="form-charge" style={{ whiteSpace: 'nowrap' }}>
        Charge ({controls.chargeStrength})
      </Form.Label>
      <Form.Range
        id="form-charge"
        min={-500}
        max={-10}
        step={10}
        value={controls.chargeStrength}
        onChange={(e) => updateControls({ type: 'chargeStrength', state: parseInt(e.target.value, 10) })}
        style={{ width: '100px' }}
      />
      <Form.Label htmlFor="form-velocity-decay" style={{ whiteSpace: 'nowrap' }}>
        Friction ({controls.velocityDecay})
      </Form.Label>
      <Form.Range
        id="form-velocity-decay"
        min={0.1}
        max={1.0}
        step={0.05}
        value={controls.velocityDecay}
        onChange={(e) => updateControls({ type: 'velocityDecay', state: parseFloat(e.target.value) })}
        style={{ width: '100px' }}
      />
      <Form.Label htmlFor="form-warmup" style={{ whiteSpace: 'nowrap' }}>
        Warmup ({controls.warmupTicks})
      </Form.Label>
      <Form.Range
        id="form-warmup"
        min={0}
        max={300}
        step={10}
        value={controls.warmupTicks}
        onChange={(e) => updateControls({ type: 'warmupTicks', state: parseInt(e.target.value, 10) })}
        style={{ width: '100px' }}
      />
      <Form.Label htmlFor="form-cooldown" style={{ whiteSpace: 'nowrap' }}>
        Cooldown ({(controls.cooldownTime / 1000).toFixed(0)}s)
      </Form.Label>
      <Form.Range
        id="form-cooldown"
        min={1000}
        max={30000}
        step={1000}
        value={controls.cooldownTime}
        onChange={(e) => updateControls({ type: 'cooldownTime', state: parseInt(e.target.value, 10) })}
        style={{ width: '100px' }}
      />
      <Dropdown as={ButtonGroup}>
        <Dropdown.Toggle className="secondary-btn" variant="" id="dag-mode-dropdown">
          DAG ({DAG_MODE_LABELS[controls.dagMode ?? 'none']})
        </Dropdown.Toggle>
        <Dropdown.Menu>
          {Object.entries(DAG_MODE_LABELS).map(([key, label]) => (
            <Dropdown.Item key={key} onClick={() => updateControls({ type: 'dagMode', state: key === 'none' ? null : (key as DagMode) })}>
              {label}
            </Dropdown.Item>
          ))}
        </Dropdown.Menu>
      </Dropdown>
      {controls.dagMode && (
        <>
          <Form.Label htmlFor="form-dag-distance" style={{ whiteSpace: 'nowrap' }}>
            DAG Spacing ({controls.dagLevelDistance ?? 'auto'})
          </Form.Label>
          <Form.Range
            id="form-dag-distance"
            min={10}
            max={300}
            step={10}
            value={controls.dagLevelDistance ?? 50}
            onChange={(e) => updateControls({ type: 'dagLevelDistance', state: parseInt(e.target.value, 10) })}
            style={{ width: '100px' }}
          />
        </>
      )}
    </ControlGroup>

    {/* Nodes section */}
    <ControlGroup>
      <SectionLabel>Nodes</SectionLabel>
      <Dropdown as={ButtonGroup}>
        <Dropdown.Toggle className="secondary-btn" variant="" id="node-style-dropdown">
          Style ({controls.nodeRenderMode})
        </Dropdown.Toggle>
        <Dropdown.Menu>
          <Dropdown.Item onClick={() => updateControls({ type: 'nodeRenderMode', state: 'spheres' })}>Spheres</Dropdown.Item>
          <Dropdown.Item onClick={() => updateControls({ type: 'nodeRenderMode', state: 'icons' })}>Icons</Dropdown.Item>
        </Dropdown.Menu>
      </Dropdown>
      <Form.Label htmlFor="form-node-size" style={{ whiteSpace: 'nowrap' }}>
        Size ({controls.nodeRelSize})
      </Form.Label>
      <Form.Range
        id="form-node-size"
        min={1}
        max={20}
        step={1}
        value={controls.nodeRelSize}
        onChange={(e) => updateControls({ type: 'nodeRelSize', state: parseInt(e.target.value, 10) })}
        style={{ width: '100px' }}
      />
      <Form.Label htmlFor="form-node-opacity" style={{ whiteSpace: 'nowrap' }}>
        Opacity ({controls.nodeOpacity})
      </Form.Label>
      <Form.Range
        id="form-node-opacity"
        min={0.1}
        max={1.0}
        step={0.05}
        value={controls.nodeOpacity}
        onChange={(e) => updateControls({ type: 'nodeOpacity', state: parseFloat(e.target.value) })}
        style={{ width: '100px' }}
      />
      <Form.Check
        type="switch"
        id="form-show-node"
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
    </ControlGroup>

    {/* Edges section */}
    <ControlGroup>
      <SectionLabel>Edges</SectionLabel>
      <Form.Label htmlFor="form-edge-width" style={{ whiteSpace: 'nowrap' }}>
        Width ({controls.edgeWidth})
      </Form.Label>
      <Form.Range
        id="form-edge-width"
        min={0.5}
        max={10}
        step={0.5}
        value={controls.edgeWidth}
        onChange={(e) => updateControls({ type: 'edgeWidth', state: parseFloat(e.target.value) })}
        style={{ width: '100px' }}
      />
      <Form.Label htmlFor="form-edge-length" style={{ whiteSpace: 'nowrap' }}>
        Length ({controls.edgeLength})
      </Form.Label>
      <Form.Range
        id="form-edge-length"
        min={10}
        max={200}
        step={10}
        value={controls.edgeLength}
        onChange={(e) => updateControls({ type: 'edgeLength', state: parseInt(e.target.value, 10) })}
        style={{ width: '100px' }}
      />
      <Form.Label htmlFor="form-edge-opacity" style={{ whiteSpace: 'nowrap' }}>
        Opacity ({controls.edgeOpacity})
      </Form.Label>
      <Form.Range
        id="form-edge-opacity"
        min={0.1}
        max={1.0}
        step={0.05}
        value={controls.edgeOpacity}
        onChange={(e) => updateControls({ type: 'edgeOpacity', state: parseFloat(e.target.value) })}
        style={{ width: '100px' }}
      />
      <Form.Label htmlFor="form-arrow-length" style={{ whiteSpace: 'nowrap' }}>
        Arrows ({controls.arrowLength})
      </Form.Label>
      <Form.Range
        id="form-arrow-length"
        min={0}
        max={15}
        step={0.5}
        value={controls.arrowLength}
        onChange={(e) => updateControls({ type: 'arrowLength', state: parseFloat(e.target.value) })}
        style={{ width: '100px' }}
      />
      <Form.Label htmlFor="form-particles" style={{ whiteSpace: 'nowrap' }}>
        Particles ({controls.directionalParticles})
      </Form.Label>
      <Form.Range
        id="form-particles"
        min={0}
        max={10}
        step={1}
        value={controls.directionalParticles}
        onChange={(e) => updateControls({ type: 'directionalParticles', state: parseInt(e.target.value, 10) })}
        style={{ width: '100px' }}
      />
      {controls.directionalParticles > 0 && (
        <>
          <Form.Label htmlFor="form-particle-speed" style={{ whiteSpace: 'nowrap' }}>
            Speed ({controls.particleSpeed})
          </Form.Label>
          <Form.Range
            id="form-particle-speed"
            min={0.001}
            max={0.05}
            step={0.001}
            value={controls.particleSpeed}
            onChange={(e) => updateControls({ type: 'particleSpeed', state: parseFloat(e.target.value) })}
            style={{ width: '100px' }}
          />
        </>
      )}
      <Form.Check
        type="switch"
        id="form-show-edge"
        label="Labels"
        checked={controls.showEdgeLabels}
        onChange={() => updateControls({ type: 'showEdgeLabels', state: !controls.showEdgeLabels })}
      />
    </ControlGroup>
  </Controls>
);

export default GraphControlsPanel;
