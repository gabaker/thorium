import React from 'react';
import { Button, Form, Dropdown, ButtonGroup } from 'react-bootstrap';
import styled from 'styled-components';
import type { ForceGraph3DInstance } from '3d-force-graph';

import { spacers } from '@styles';
import { exportJPEG, exportPNG } from './export';

export type NodeRenderMode = 'spheres' | 'icons';

export interface GraphControls {
  filterChildless: boolean;
  depth: number;
  showEdgeLabels: boolean;
  showNodeLabels: boolean;
  selectedElement: SelectedElement | null;
  showNodeInfo: boolean;
  nodeRenderMode: NodeRenderMode;
  focusOnClick: boolean;
  edgeWidth: number;
  edgeLength: number;
}

export type SelectedElement = { kind: 'node'; id: string; label: string } | { kind: 'link'; source: string; target: string; label: string };

export type DisplayAction =
  | { type: 'depth'; state: number }
  | { type: 'filterChildless' | 'showEdgeLabels' | 'showNodeLabels' | 'showNodeInfo' | 'focusOnClick'; state: boolean }
  | { type: 'selected'; state: SelectedElement | null }
  | { type: 'nodeRenderMode'; state: NodeRenderMode }
  | { type: 'edgeWidth' | 'edgeLength'; state: number };

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
  width: 200px;
  grid-auto-flow: column dense;
  align-items: center;
  justify-content: start;
  gap: ${spacers.one};
  margin-left: ${spacers.four};
`;

const ControlGroup = styled.div`
  display: grid;
  row-gap: ${spacers.one};
  justify-items: center;
  & > label {
    font-size: 0.9rem;
    margin: 0;
  }
`;

export const GraphControlsPanel: React.FC<GraphControlsProps> = ({ graphId, controls, updateControls, graphInstance }) => (
  <Controls>
    <ControlGroup>
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
      <Form.Label htmlFor="form-select-depth" className="mb-3">
        Depth
      </Form.Label>
    </ControlGroup>
    <Button
      variant=""
      className="secondary-btn mb-5"
      onClick={() => {
        graphInstance?.d3ReheatSimulation();
      }}
    >
      Reheat
    </Button>
    <Button
      variant=""
      className="secondary-btn mb-5"
      onClick={() => {
        graphInstance?.zoomToFit(1000, 50);
      }}
    >
      Fit All
    </Button>
    <Dropdown as={ButtonGroup} className="mb-5">
      <Dropdown.Toggle className="secondary-btn" variant="" id="download-dropdown">
        Download
      </Dropdown.Toggle>
      <Dropdown.Menu>
        <Dropdown.Item key="download_png_option" onClick={() => exportPNG(graphId, graphInstance)}>
          PNG
        </Dropdown.Item>
        <Dropdown.Item key="download_jpeg_option" onClick={() => exportJPEG(graphId, graphInstance)}>
          JPEG
        </Dropdown.Item>
      </Dropdown.Menu>
    </Dropdown>
    <ControlGroup>
      <Dropdown as={ButtonGroup} className="mb-5">
        <Dropdown.Toggle className="secondary-btn" variant="" id="node-style-dropdown">
          Nodes ({controls.nodeRenderMode})
        </Dropdown.Toggle>
        <Dropdown.Menu>
          <Dropdown.Item onClick={() => updateControls({ type: 'nodeRenderMode', state: 'spheres' })}>Spheres</Dropdown.Item>
          <Dropdown.Item onClick={() => updateControls({ type: 'nodeRenderMode', state: 'icons' })}>Icons</Dropdown.Item>
        </Dropdown.Menu>
      </Dropdown>
    </ControlGroup>
    <ControlGroup className="mb-4">
      <Form.Check
        type="switch"
        id="form-show-node"
        label="Node Labels"
        checked={controls.showNodeLabels}
        onChange={() =>
          updateControls({
            type: 'showNodeLabels',
            state: !controls.showNodeLabels,
          })
        }
      />
      <Form.Check
        type="switch"
        id="form-show-edge"
        label="Edge Labels"
        checked={controls.showEdgeLabels}
        onChange={() =>
          updateControls({
            type: 'showEdgeLabels',
            state: !controls.showEdgeLabels,
          })
        }
      />
      <Form.Check
        type="switch"
        id="form-focus-on-click"
        label="Focus on Click"
        checked={controls.focusOnClick}
        onChange={() =>
          updateControls({
            type: 'focusOnClick',
            state: !controls.focusOnClick,
          })
        }
      />
    </ControlGroup>
    <ControlGroup className="mb-4">
      <Form.Label htmlFor="form-edge-width" style={{ whiteSpace: 'nowrap' }}>
        Edge Width ({controls.edgeWidth})
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
        Edge Length ({controls.edgeLength})
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
    </ControlGroup>
  </Controls>
);

export default GraphControlsPanel;
