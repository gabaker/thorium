import React, { useEffect, useReducer, useRef, useState } from 'react';
import { ErrorBoundary } from 'react-error-boundary';
import ForceGraph3D, { ForceGraph3DInstance } from '3d-force-graph';
import * as THREE from 'three';
import SpriteText from 'three-spritetext';
import styled from 'styled-components';

import { Seed, Graph, BranchNode, Direction, BlankGraph } from '@models/trees';
import RenderErrorAlert from '@components/shared/alerts/RenderErrorAlert';
import { getInitialTree, growTree } from '@thorpi/trees';
import { getEdgeLabel } from '../utilities';
import { getNodeColor, getEdgeColor, getNodeSvg, svgToTexture } from './styles';
import { GraphControlsPanel, DisplayAction, GraphControls, SelectedElement, NodeRenderMode } from './GraphControls';
import NodeInfo from '../graph/NodeInfo';
import EdgeInfo from '../graph/EdgeInfo';
import { buildGraphNode, processInitialGraphData } from './data';
import type { GraphNode, GraphLink, GraphData } from './types';

const buildNodeObject = (renderMode: NodeRenderMode, showLabels: boolean) => {
  return (node: GraphNode): THREE.Object3D => {
    const group = new THREE.Group();

    if (renderMode === 'icons') {
      const svgString = getNodeSvg(node.nodeType, node.visualState);
      const texture = svgToTexture(svgString, 64);
      const spriteMaterial = new THREE.SpriteMaterial({ map: texture, transparent: true });
      const sprite = new THREE.Sprite(spriteMaterial);
      const scale = Math.max(6, node.diameter / 3);
      sprite.scale.set(scale, scale, 1);
      group.add(sprite);
    }

    if (showLabels) {
      const labelSprite = new SpriteText(node.label);
      labelSprite.color = getNodeColor(node.nodeType, node.visualState);
      labelSprite.textHeight = 3;
      (labelSprite as any).position.y = renderMode === 'icons' ? -(node.diameter / 5 + 4) : -(node.diameter / 5 + 2);
      // @ts-ignore — depthWrite exists on SpriteMaterial
      labelSprite.material.depthWrite = false;
      group.add(labelSprite);
    }

    return group;
  };
};

const GraphWindow = styled.div`
  position: relative;
  background-color: var(--thorium-panel-bg);
`;

const GraphDiv = styled.div`
  z-index: 200;
  overflow: visible;
  min-height: 90vh;
  max-height: 90vw;
  padding-top: 1rem;
`;

const DataPreview = styled.div`
  position: absolute;
  z-index: 300;
  top: 0px;
  right: 0px;
  background-color: rgba(255, 255, 255, 0.01) !important;
  padding: 10px;
  max-width: 40vw;
  max-height: 30vh;
  overflow-y: auto;
  overflow-x: auto;
`;

interface AssociationGraphProps {
  initial: Seed;
  inView: boolean;
}

const AssociationGraph3DInner: React.FC<AssociationGraphProps> = ({ initial, inView }) => {
  const containerRef = useRef<HTMLDivElement>(null);
  const graphInstanceRef = useRef<ForceGraph3DInstance | null>(null);
  const graphRootRef = useRef<string[]>(['']);
  const graphRef = useRef<Graph>(BlankGraph);
  const graphDataRef = useRef<GraphData>({ nodes: [], links: [] });

  const [nodeCount, setNodeCount] = useState(0);
  const [graphId, setGraphId] = useState<string>('');

  const [controls, updateControls] = useReducer(controlsReducer, {
    filterChildless: false,
    depth: 1,
    showEdgeLabels: false,
    showNodeLabels: true,
    selectedElement: null,
    showNodeInfo: true,
    nodeRenderMode: 'spheres' as NodeRenderMode,
  });

  function controlsReducer(state: GraphControls, action: DisplayAction): GraphControls {
    switch (action.type) {
      case 'showEdgeLabels': {
        const gi = graphInstanceRef.current;
        if (gi) gi.linkLabel(action.state ? (link: any) => (link as GraphLink).label : '');
        return { ...state, showEdgeLabels: action.state };
      }
      case 'showNodeLabels': {
        const gi = graphInstanceRef.current;
        if (gi) {
          gi.nodeThreeObject(buildNodeObject(state.nodeRenderMode, action.state) as any);
          gi.nodeThreeObjectExtend(state.nodeRenderMode === 'spheres');
          gi.refresh();
        }
        return { ...state, showNodeLabels: action.state };
      }
      case 'showNodeInfo':
        return { ...state, showNodeInfo: action.state };
      case 'selected':
        return { ...state, selectedElement: action.state };
      case 'depth':
        return { ...state, depth: action.state };
      case 'filterChildless':
        return { ...state, filterChildless: action.state };
      case 'nodeRenderMode': {
        const gi = graphInstanceRef.current;
        if (gi) {
          gi.nodeThreeObject(buildNodeObject(action.state, state.showNodeLabels) as any);
          gi.nodeThreeObjectExtend(action.state === 'spheres');
          gi.refresh();
        }
        return { ...state, nodeRenderMode: action.state };
      }
    }
  }

  const handleNodeSelect = async (node: GraphNode) => {
    updateControls({
      type: 'selected',
      state: { kind: 'node', id: node.id, label: node.label },
    });

    if (graphInstanceRef.current && node.x !== undefined && node.y !== undefined && node.z !== undefined) {
      const distance = 150;
      const distRatio = 1 + distance / Math.hypot(node.x, node.y, node.z || 1);
      graphInstanceRef.current.cameraPosition(
        { x: node.x * distRatio, y: node.y * distRatio, z: node.z * distRatio },
        { x: node.x, y: node.y, z: node.z },
        1000,
      );
    }

    const growable = graphRef.current.growable.map((n) => n.toString());
    if (!growable.includes(node.id) || !graphId) return;

    const data = await growTree(graphId, [node.id], console.log);
    if (!data) return;

    const existingData = graphDataRef.current;
    const existingNodeIds = new Set(existingData.nodes.map((n) => n.id));
    const existingEdgeKeys = new Set(
      existingData.links.map((l) => {
        const src = typeof l.source === 'object' ? (l.source as GraphNode).id : l.source;
        const tgt = typeof l.target === 'object' ? (l.target as GraphNode).id : l.target;
        return `${src}-${tgt}`;
      }),
    );

    const newNodes: GraphNode[] = [];
    const newLinks: GraphLink[] = [];

    if (data.data_map) {
      Object.keys(data.data_map).forEach((newNodeId) => {
        graphRef.current.data_map[newNodeId] = data.data_map[newNodeId];
        if (!existingNodeIds.has(newNodeId)) {
          const totalCount = Object.keys(graphRef.current.data_map).length;
          newNodes.push(buildGraphNode(newNodeId, graphRef.current, totalCount));
          existingNodeIds.add(newNodeId);
        }
      });
    }

    if (data.branches) {
      Object.keys(data.branches).forEach((branch) => {
        graphRef.current.branches[branch] = data.branches[branch];
        data.branches[branch].forEach((target: BranchNode) => {
          const targetNode = target.direction === Direction.To ? target.node.toString() : branch;
          const sourceNode = target.direction === Direction.To ? branch : target.node.toString();
          const edgeKey = `${sourceNode}-${targetNode}`;
          if (!existingEdgeKeys.has(edgeKey) || target.direction === Direction.Bidirectional) {
            newLinks.push({
              source: sourceNode,
              target: targetNode,
              label: getEdgeLabel(targetNode, sourceNode, target, graphRef.current),
              bidirectional: target.direction === Direction.Bidirectional,
            });
            existingEdgeKeys.add(edgeKey);
          }
        });
      });
    }

    if (data.growable) {
      const newGrowable = [...data.growable, ...growable.filter((g) => g !== node.id)];
      graphRef.current.growable = newGrowable;
    } else {
      graphRef.current.growable = growable.filter((g) => g !== node.id);
    }

    if (!graphRef.current.growable.includes(node.id)) {
      const existingNode = existingData.nodes.find((n) => n.id === node.id);
      if (existingNode && existingNode.visualState === 'growable') {
        existingNode.visualState = 'basic';
      }
    }

    const updatedData: GraphData = {
      nodes: [...existingData.nodes, ...newNodes],
      links: [...existingData.links, ...newLinks],
    };
    graphDataRef.current = updatedData;
    setNodeCount(updatedData.nodes.length);

    if (graphInstanceRef.current) {
      graphInstanceRef.current.graphData(updatedData);
      graphInstanceRef.current.nodeThreeObject(graphInstanceRef.current.nodeThreeObject());
      graphInstanceRef.current.refresh();
    }
  };

  const handleEdgeSelect = (link: GraphLink) => {
    const src = typeof link.source === 'object' ? (link.source as GraphNode).id : link.source;
    const tgt = typeof link.target === 'object' ? (link.target as GraphNode).id : link.target;
    updateControls({
      type: 'selected',
      state: { kind: 'link', source: src, target: tgt, label: link.label },
    });
  };

  useEffect(() => {
    updateControls({ type: 'selected', state: null });

    getInitialTree(initial, controls.filterChildless, controls.depth, console.log).then((data) => {
      if (data) {
        setGraphId(data.id);
        if (data.initial.length > 0) {
          graphRootRef.current = data.initial;
        }
        graphRef.current = data;
        const graphData = processInitialGraphData(data);
        graphDataRef.current = graphData;
        setNodeCount(graphData.nodes.length);
      }
    });
  }, [controls.filterChildless, controls.depth, initial]);

  useEffect(() => {
    if (!containerRef.current || !graphId) return;

    if (graphInstanceRef.current) {
      graphInstanceRef.current._destructor();
      graphInstanceRef.current = null;
    }

    const graph = new ForceGraph3D(containerRef.current)
      .graphData(graphDataRef.current)
      .backgroundColor('rgba(0,0,0,0)')
      .width(containerRef.current.clientWidth)
      .height(containerRef.current.clientHeight || window.innerHeight * 0.9)
      .nodeVal((node: any) => (node as GraphNode).diameter)
      .nodeColor((node: any) => getNodeColor((node as GraphNode).nodeType, (node as GraphNode).visualState))
      .nodeLabel((node: any) => (node as GraphNode).label)
      .nodeThreeObject(buildNodeObject(controls.nodeRenderMode, controls.showNodeLabels) as any)
      .nodeThreeObjectExtend(controls.nodeRenderMode === 'spheres')
      .linkDirectionalArrowLength(3.5)
      .linkDirectionalArrowRelPos(1)
      .linkLabel(controls.showEdgeLabels ? (link: any) => (link as GraphLink).label : '')
      .linkColor(() => getEdgeColor())
      .linkWidth(1)
      .linkCurvature((link: any) => ((link as GraphLink).bidirectional ? 0.2 : 0))
      .onNodeClick((node: any) => handleNodeSelect(node as GraphNode))
      .onLinkClick((link: any) => handleEdgeSelect(link as GraphLink));

    const chargeForce = graph.d3Force('charge');
    if (chargeForce && 'strength' in chargeForce) {
      (chargeForce as any).strength(-200);
    }

    graphInstanceRef.current = graph;

    if (graphDataRef.current.nodes.length > 100) {
      graph.cooldownTicks(200);
    }

    return () => {
      graph._destructor();
      graphInstanceRef.current = null;
    };
  }, [graphId]);

  useEffect(() => {
    if (inView && graphInstanceRef.current && containerRef.current) {
      graphInstanceRef.current
        .width(containerRef.current.clientWidth)
        .height(containerRef.current.clientHeight || window.innerHeight * 0.9);
    }
  }, [inView]);

  return (
    <GraphWindow>
      <GraphDiv ref={containerRef} />
      <div className="ps-4 ms-2">Node Count: {nodeCount}</div>
      <GraphControlsPanel graphId={graphId} controls={controls} updateControls={updateControls} graphInstance={graphInstanceRef.current} />
      <DataPreview>
        {controls.selectedElement?.kind === 'node' && controls.selectedElement.id !== '' && (
          <NodeInfo node={graphRef.current.data_map[controls.selectedElement.id]} />
        )}
        {controls.selectedElement?.kind === 'link' && (
          <EdgeInfo
            edge={{
              data: {
                source: controls.selectedElement.source,
                target: controls.selectedElement.target,
                label: controls.selectedElement.label,
              },
            }}
          />
        )}
      </DataPreview>
    </GraphWindow>
  );
};

export const AssociationGraph3D: React.FC<AssociationGraphProps> = ({ initial, inView }) => {
  return (
    <ErrorBoundary fallback={<RenderErrorAlert page={false} />}>
      {inView && <AssociationGraph3DInner inView={inView} initial={initial} />}
    </ErrorBoundary>
  );
};

export default AssociationGraph3D;
