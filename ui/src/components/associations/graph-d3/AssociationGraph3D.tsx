import React, { useEffect, useReducer, useRef, useState } from 'react';
import { ErrorBoundary } from 'react-error-boundary';
import ForceGraph3D, { ForceGraph3DInstance } from '3d-force-graph';
import * as THREE from 'three';
import SpriteText from 'three-spritetext';
import styled from 'styled-components';

import RenderErrorAlert from '@components/shared/alerts/RenderErrorAlert';
import { getNodeColor, getEdgeColor, getNodeSvg, svgToTexture } from './styles';
import { GraphControlsToolbar, DisplayAction, GraphControls, NodeRenderMode, DagMode } from './controls';
import NodeInfo from '../graph/NodeInfo';
import EdgeInfo from '../graph/EdgeInfo';
import { processInitialGraphData } from './data';
import { useGraphData } from '../data';
import type { GraphNode, GraphLink, GraphData } from './types';

type LabelEntry = { sprite: THREE.Object3D; degree: number; isInitial: boolean; baseScale: THREE.Vector3 };

const buildNodeObject = (renderMode: NodeRenderMode, showLabels: boolean, labelMap?: Map<string, LabelEntry>) => {
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
      if (labelMap) {
        const obj = labelSprite as unknown as THREE.Object3D;
        labelMap.set(node.id, { sprite: obj, degree: node.degree, isInitial: node.visualState === 'initial', baseScale: obj.scale.clone() });
      }
    } else if (labelMap) {
      labelMap.delete(node.id);
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
  max-height: 90vh;
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
  inView: boolean;
}

const AssociationGraph3DInner: React.FC<AssociationGraphProps> = () => {
  const { graph, graphId, graphVersion, grow, growToDepth, growable } = useGraphData();

  const containerRef = useRef<HTMLDivElement>(null);
  const graphInstanceRef = useRef<ForceGraph3DInstance | null>(null);
  const graphRootRef = useRef<string[]>(['']);
  const graphDataRef = useRef<GraphData>({ nodes: [], links: [] });
  const labelSpritesRef = useRef<Map<string, LabelEntry>>(new Map());
  const animFrameRef = useRef<number>(0);
  const mountedVersionRef = useRef<number>(-1);

  const [nodeCount, setNodeCount] = useState(0);
  const [mounted, setMounted] = useState(false);

  const [controls, updateControls] = useReducer(controlsReducer, {
    filterChildless: false,
    depth: 1,
    showEdgeLabels: false,
    showNodeLabels: true,
    selectedElement: null,
    showNodeInfo: true,
    nodeRenderMode: 'icons' as NodeRenderMode,
    focusOnClick: true,
    edgeWidth: 1,
    edgeLength: 30,
    edgeLinkStrength: 0.5,
    edgeOpacity: 0.2,
    arrowLength: 3.5,
    directionalParticles: 0,
    particleSpeed: 0.01,
    nodeRelSize: 4,
    nodeOpacity: 0.75,
    enableNodeDrag: true,
    chargeStrength: -200,
    velocityDecay: 0.4,
    warmupTicks: 0,
    cooldownTime: 15000,
    dagMode: null as DagMode,
    dagLevelDistance: null as number | null,
    numDimensions: 3 as 2 | 3,
  });

  function controlsReducer(state: GraphControls, action: DisplayAction): GraphControls {
    const gi = graphInstanceRef.current;
    switch (action.type) {
      case 'showEdgeLabels': {
        if (gi) gi.linkLabel(action.state ? 'label' : () => '');
        return { ...state, showEdgeLabels: action.state };
      }
      case 'showNodeLabels': {
        if (gi) {
          labelSpritesRef.current.clear();
          gi.nodeThreeObject(buildNodeObject(state.nodeRenderMode, action.state, labelSpritesRef.current) as any);
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
      case 'focusOnClick':
        return { ...state, focusOnClick: action.state };
      case 'nodeRenderMode': {
        if (gi) {
          labelSpritesRef.current.clear();
          gi.nodeThreeObject(buildNodeObject(action.state, state.showNodeLabels, labelSpritesRef.current) as any);
          gi.nodeThreeObjectExtend(action.state === 'spheres');
          gi.refresh();
        }
        return { ...state, nodeRenderMode: action.state };
      }
      case 'edgeWidth': {
        if (gi) gi.linkWidth(action.state);
        return { ...state, edgeWidth: action.state };
      }
      case 'edgeLength': {
        if (gi) {
          const linkForce = gi.d3Force('link');
          if (linkForce && 'distance' in linkForce) (linkForce as any).distance(action.state);
          gi.d3ReheatSimulation();
        }
        return { ...state, edgeLength: action.state };
      }
      case 'edgeLinkStrength': {
        if (gi) {
          const linkForce = gi.d3Force('link');
          if (linkForce && 'strength' in linkForce) (linkForce as any).strength(action.state);
          gi.d3ReheatSimulation();
        }
        return { ...state, edgeLinkStrength: action.state };
      }
      case 'edgeOpacity': {
        if (gi) gi.linkOpacity(action.state);
        return { ...state, edgeOpacity: action.state };
      }
      case 'arrowLength': {
        if (gi) gi.linkDirectionalArrowLength(action.state);
        return { ...state, arrowLength: action.state };
      }
      case 'directionalParticles': {
        if (gi) gi.linkDirectionalParticles(action.state);
        return { ...state, directionalParticles: action.state };
      }
      case 'particleSpeed': {
        if (gi) gi.linkDirectionalParticleSpeed(action.state);
        return { ...state, particleSpeed: action.state };
      }
      case 'nodeRelSize': {
        if (gi) gi.nodeRelSize(action.state);
        return { ...state, nodeRelSize: action.state };
      }
      case 'nodeOpacity': {
        if (gi) gi.nodeOpacity(action.state);
        return { ...state, nodeOpacity: action.state };
      }
      case 'enableNodeDrag': {
        if (gi) gi.enableNodeDrag(action.state);
        return { ...state, enableNodeDrag: action.state };
      }
      case 'chargeStrength': {
        if (gi) {
          const charge = gi.d3Force('charge');
          if (charge && 'strength' in charge) (charge as any).strength(action.state);
          gi.d3ReheatSimulation();
        }
        return { ...state, chargeStrength: action.state };
      }
      case 'velocityDecay': {
        if (gi) {
          gi.d3VelocityDecay(action.state);
          gi.d3ReheatSimulation();
        }
        return { ...state, velocityDecay: action.state };
      }
      case 'warmupTicks': {
        if (gi) gi.warmupTicks(action.state);
        return { ...state, warmupTicks: action.state };
      }
      case 'cooldownTime': {
        if (gi) gi.cooldownTime(action.state);
        return { ...state, cooldownTime: action.state };
      }
      case 'dagMode': {
        if (gi) gi.dagMode(action.state as any);
        return { ...state, dagMode: action.state };
      }
      case 'dagLevelDistance': {
        if (gi) gi.dagLevelDistance(action.state as any);
        return { ...state, dagLevelDistance: action.state };
      }
      case 'numDimensions': {
        if (gi) gi.numDimensions(action.state);
        return { ...state, numDimensions: action.state };
      }
    }
  }

  const applyGrowthToInstance = (prevData: GraphData, newData: GraphData) => {
    const gi = graphInstanceRef.current;
    if (!gi) return;

    const existingNodeIds = new Set(prevData.nodes.map((n) => n.id));
    const existingEdgeKeys = new Set(
      prevData.links.map((l) => {
        const src = typeof l.source === 'object' ? (l.source as GraphNode).id : l.source;
        const tgt = typeof l.target === 'object' ? (l.target as GraphNode).id : l.target;
        return `${src}-${tgt}`;
      }),
    );

    const addedNodes = newData.nodes.filter((n) => !existingNodeIds.has(n.id));
    const addedLinks = newData.links.filter((l) => {
      const src = typeof l.source === 'object' ? (l.source as GraphNode).id : l.source;
      const tgt = typeof l.target === 'object' ? (l.target as GraphNode).id : l.target;
      return !existingEdgeKeys.has(`${src}-${tgt}`);
    });

    if (addedNodes.length === 0 && addedLinks.length === 0) return;

    const updatedData: GraphData = {
      nodes: [...prevData.nodes, ...addedNodes],
      links: [...prevData.links, ...addedLinks],
    };
    graphDataRef.current = updatedData;
    setNodeCount(updatedData.nodes.length);

    gi.graphData(updatedData);
    labelSpritesRef.current.clear();
    gi.nodeThreeObject(gi.nodeThreeObject());
    gi.refresh();
  };

  // React to graph changes from the shared context
  useEffect(() => {
    if (!graphId || graphVersion === 0) return;

    const newGraphData = processInitialGraphData(graph);

    // First load — store data and trigger mount
    if (!mounted) {
      if (graph.initial.length > 0) {
        graphRootRef.current = graph.initial;
      }
      graphDataRef.current = newGraphData;
      setNodeCount(newGraphData.nodes.length);
      setMounted(true);
      mountedVersionRef.current = graphVersion;
      return;
    }

    // Incremental update — diff and apply to existing ForceGraph3D instance
    if (graphVersion > mountedVersionRef.current) {
      const prevData = graphDataRef.current;
      applyGrowthToInstance(prevData, newGraphData);
      mountedVersionRef.current = graphVersion;
    }
  }, [graphId, graphVersion]);

  const handleNodeSelect = async (node: GraphNode) => {
    updateControls({
      type: 'selected',
      state: { kind: 'node', id: node.id, label: node.label },
    });

    if (!growable.has(node.id) || !graphId) return;
    await grow(node.id);
  };

  const handleEdgeSelect = (link: GraphLink) => {
    const src = typeof link.source === 'object' ? (link.source as GraphNode).id : link.source;
    const tgt = typeof link.target === 'object' ? (link.target as GraphNode).id : link.target;
    updateControls({
      type: 'selected',
      state: { kind: 'link', source: src, target: tgt, label: link.label },
    });
  };

  // Mount the ForceGraph3D instance once data is ready
  useEffect(() => {
    if (!containerRef.current || !mounted) return;

    if (graphInstanceRef.current) {
      graphInstanceRef.current._destructor();
      graphInstanceRef.current = null;
    }
    labelSpritesRef.current.clear();

    const fg = new ForceGraph3D(containerRef.current, { controlType: 'orbit' })
      .graphData(graphDataRef.current)
      .backgroundColor('rgba(0,0,0,0)')
      .width(containerRef.current.clientWidth)
      .height(containerRef.current.clientHeight || window.innerHeight * 0.9)
      .nodeVal((node: any) => (node as GraphNode).diameter)
      .nodeColor((node: any) => getNodeColor((node as GraphNode).nodeType, (node as GraphNode).visualState))
      .nodeLabel((node: any) => (node as GraphNode).label)
      .nodeThreeObject(buildNodeObject(controls.nodeRenderMode, controls.showNodeLabels, labelSpritesRef.current) as any)
      .nodeThreeObjectExtend(controls.nodeRenderMode === 'spheres')
      .nodeRelSize(controls.nodeRelSize)
      .nodeOpacity(controls.nodeOpacity)
      .linkDirectionalArrowLength(controls.arrowLength)
      .linkDirectionalArrowRelPos(1)
      .linkLabel(controls.showEdgeLabels ? 'label' : () => '')
      .linkColor(() => getEdgeColor())
      .linkWidth(controls.edgeWidth)
      .linkOpacity(controls.edgeOpacity)
      .linkCurvature((link: any) => ((link as GraphLink).bidirectional ? 0.2 : 0))
      .linkDirectionalParticles(controls.directionalParticles)
      .linkDirectionalParticleSpeed(controls.particleSpeed)
      .enableNodeDrag(controls.enableNodeDrag)
      .onNodeClick((node: any) => handleNodeSelect(node as GraphNode))
      .onLinkClick((link: any) => handleEdgeSelect(link as GraphLink))
      .numDimensions(controls.numDimensions)
      .warmupTicks(controls.warmupTicks)
      .cooldownTime(controls.cooldownTime)
      .d3VelocityDecay(controls.velocityDecay);

    if (controls.dagMode) {
      fg.dagMode(controls.dagMode as any);
      if (controls.dagLevelDistance !== null) {
        fg.dagLevelDistance(controls.dagLevelDistance as any);
      }
    }

    const chargeForce = fg.d3Force('charge');
    if (chargeForce && 'strength' in chargeForce) {
      (chargeForce as any).strength(controls.chargeStrength);
    }

    const linkForce = fg.d3Force('link');
    if (linkForce && 'distance' in linkForce) {
      (linkForce as any).distance(controls.edgeLength);
      (linkForce as any).strength(controls.edgeLinkStrength);
    }

    graphInstanceRef.current = fg;

    const orbitControls = fg.controls();
    if (orbitControls) (orbitControls as any).zoomToCursor = true;

    if (graphDataRef.current.nodes.length > 100) {
      fg.cooldownTicks(200);
    }

    const updateLabelScaling = () => {
      const gi = graphInstanceRef.current;
      if (gi && labelSpritesRef.current.size > 0) {
        const camPos = gi.cameraPosition();
        const target = (gi.controls() as any)?.target;
        if (target) {
          const dist = Math.hypot(camPos.x - target.x, camPos.y - target.y, camPos.z - target.z);

          let maxDegree = 1;
          labelSpritesRef.current.forEach((entry) => {
            if (entry.degree > maxDegree) maxDegree = entry.degree;
          });

          const distFactor = Math.max(1, dist / 300);
          const filterStart = 150;
          const filterEnd = 1000;
          const filterProgress = Math.min(1, Math.max(0, (dist - filterStart) / (filterEnd - filterStart)));
          const degreeThreshold = filterProgress * maxDegree * 0.6;

          labelSpritesRef.current.forEach((entry) => {
            if (entry.isInitial || entry.degree >= degreeThreshold) {
              entry.sprite.visible = true;
              const degreeBoost = entry.isInitial ? 1.5 : 1 + (entry.degree / maxDegree) * 0.5;
              const s = distFactor * degreeBoost;
              entry.sprite.scale.set(entry.baseScale.x * s, entry.baseScale.y * s, entry.baseScale.z);
            } else {
              entry.sprite.visible = false;
            }
          });
        }
      }
      animFrameRef.current = requestAnimationFrame(updateLabelScaling);
    };
    animFrameRef.current = requestAnimationFrame(updateLabelScaling);

    const fitTimeout = setTimeout(() => {
      if (graphInstanceRef.current) {
        graphInstanceRef.current.zoomToFit(1000, 50);
      }
    }, 1500);

    const container = containerRef.current;
    const handleDblClick = (event: MouseEvent) => {
      const gi = graphInstanceRef.current;
      if (!gi || !container) return;

      const rect = container.getBoundingClientRect();
      const mouse = new THREE.Vector2(
        ((event.clientX - rect.left) / rect.width) * 2 - 1,
        -((event.clientY - rect.top) / rect.height) * 2 + 1,
      );

      const camera = gi.cameraPosition();
      const raycaster = new THREE.Raycaster();
      raycaster.setFromCamera(mouse, gi.camera());

      const target = (gi.controls() as any).target;
      const dist = Math.hypot(camera.x - target.x, camera.y - target.y, camera.z - target.z);
      const dir = raycaster.ray.direction;
      const step = dist * 0.4;

      const newPos = { x: camera.x + dir.x * step, y: camera.y + dir.y * step, z: camera.z + dir.z * step };
      const lookDist = dist - step;
      const newLookAt = { x: newPos.x + dir.x * lookDist, y: newPos.y + dir.y * lookDist, z: newPos.z + dir.z * lookDist };

      gi.cameraPosition(newPos, newLookAt, 500);
    };
    container.addEventListener('dblclick', handleDblClick);

    const resizeObserver = new ResizeObserver((entries) => {
      const gi = graphInstanceRef.current;
      if (!gi) return;
      for (const entry of entries) {
        const { width, height } = entry.contentRect;
        if (width > 0 && height > 0) {
          gi.width(width).height(height);
        }
      }
    });
    resizeObserver.observe(container);

    return () => {
      cancelAnimationFrame(animFrameRef.current);
      clearTimeout(fitTimeout);
      container.removeEventListener('dblclick', handleDblClick);
      resizeObserver.disconnect();
      fg._destructor();
      graphInstanceRef.current = null;
    };
  }, [mounted]);

  // Grow frontier nodes when depth increases
  useEffect(() => {
    if (!graphId || controls.depth <= 1) return;
    let aborted = false;

    const doGrow = async () => {
      if (aborted) return;
      await growToDepth(controls.depth);
    };

    doGrow();
    return () => {
      aborted = true;
    };
  }, [controls.depth, graphId]);

  // Re-fetch when filterChildless changes
  const { reload } = useGraphData();
  useEffect(() => {
    if (!graphId) return;
    reload({ filterChildless: controls.filterChildless });
  }, [controls.filterChildless]);

  return (
    <GraphWindow>
      <GraphDiv ref={containerRef} />
      <GraphControlsToolbar
        graphId={graphId}
        controls={controls}
        updateControls={updateControls}
        graphInstance={graphInstanceRef.current}
        nodeCount={nodeCount}
      />
      <DataPreview>
        {controls.selectedElement?.kind === 'node' && controls.selectedElement.id !== '' && (
          <NodeInfo node={graph.data_map[controls.selectedElement.id]} />
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

export const AssociationGraph3D: React.FC<AssociationGraphProps> = ({ inView }) => {
  return (
    <ErrorBoundary fallback={<RenderErrorAlert page={false} />}>
      {inView && <AssociationGraph3DInner inView={inView} />}
    </ErrorBoundary>
  );
};

export default AssociationGraph3D;
