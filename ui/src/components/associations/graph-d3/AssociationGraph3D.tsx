import React, { useEffect, useReducer, useRef, useState } from 'react';
import { ErrorBoundary } from 'react-error-boundary';
import { Spinner } from 'react-bootstrap';
import ForceGraph3D, { ForceGraph3DInstance } from '3d-force-graph';
import * as THREE from 'three';

import RenderErrorAlert from '@components/shared/alerts/RenderErrorAlert';
import { getNodeColor, getEdgeColor } from './styles';
import { GraphControlsToolbar, NodeRenderMode, DagMode, createControlsReducer, buildNodeObject } from './controls';
import type { LabelEntry } from './controls';
import NodeInfo from '../graph/NodeInfo';
import EdgeInfo from '../graph/EdgeInfo';
import { processInitialGraphData, getLinkEndpoints } from './data';
import { useGraphData } from '../data';
import type { GraphNode, GraphLink, GraphData } from './types';
import { applyGrowthToInstance } from './applyGrowth';
import { GraphWindow, GraphDiv, LoadingOverlay, DataPreview } from './AssociationGraph3D.styled';

interface AssociationGraphProps {
  inView: boolean;
}

const AssociationGraph3DInner: React.FC<AssociationGraphProps> = () => {
  const {
    graph, graphId, graphVersion, loading, grow, growToDepth,
    growable, reload, focusedNodeId, focusSource, setFocusedNode,
  } = useGraphData();

  const containerRef = useRef<HTMLDivElement>(null);
  const graphInstanceRef = useRef<ForceGraph3DInstance | null>(null);
  const graphDataRef = useRef<GraphData>({ nodes: [], links: [] });
  const labelSpritesRef = useRef<Map<string, LabelEntry>>(new Map());
  const animFrameRef = useRef<number>(0);
  const mountedVersionRef = useRef<number>(-1);

  const [nodeCount, setNodeCount] = useState(0);
  const [mounted, setMounted] = useState(false);
  const handleNodeSelectRef = useRef<(node: GraphNode) => Promise<void>>(null as any);

  const controlsReducer = createControlsReducer(graphInstanceRef, labelSpritesRef);

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

  // React to graph changes from the shared context
  useEffect(() => {
    if (!graphId || graphVersion === 0) return;

    const newGraphData = processInitialGraphData(graph);

    if (!mounted) {
      graphDataRef.current = newGraphData;
      setNodeCount(newGraphData.nodes.length);
      setMounted(true);
      mountedVersionRef.current = graphVersion;
      return;
    }

    if (graphVersion > mountedVersionRef.current) {
      const prevData = graphDataRef.current;
      applyGrowthToInstance(prevData, newGraphData, graphInstanceRef, labelSpritesRef, graphDataRef, setNodeCount);
      mountedVersionRef.current = graphVersion;
    }
  }, [graphId, graphVersion]);

  const handleNodeSelect = async (node: GraphNode) => {
    updateControls({
      type: 'selected',
      state: { kind: 'node', id: node.id, label: node.label },
    });
    setFocusedNode(node.id, 'graph');

    if (!growable.has(node.id) || !graphId) return;
    await grow(node.id);
  };
  handleNodeSelectRef.current = handleNodeSelect;

  const handleEdgeSelect = (link: GraphLink) => {
    const { source, target } = getLinkEndpoints(link);
    updateControls({
      type: 'selected',
      state: { kind: 'link', source, target, label: link.label },
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
      .onNodeClick((node: any) => void handleNodeSelectRef.current?.(node as GraphNode))
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

    void doGrow();
    return () => {
      aborted = true;
    };
  }, [controls.depth, graphId]);

  // Re-fetch when filterChildless changes
  useEffect(() => {
    if (!graphId) return;
    void reload({ filterChildless: controls.filterChildless });
  }, [controls.filterChildless]);

  // Animate camera to focused node when focus originates from tree
  useEffect(() => {
    if (!focusedNodeId || focusSource !== 'tree') return;
    const gi = graphInstanceRef.current;
    if (!gi) return;

    const node = graphDataRef.current.nodes.find((n) => n.id === focusedNodeId);
    if (!node || node.x === undefined || node.y === undefined) return;

    const distance = 150;
    const nodeHypot = Math.hypot(node.x, node.y, node.z ?? 0);
    const distRatio = nodeHypot > 0 ? 1 + distance / nodeHypot : 1;

    gi.cameraPosition(
      { x: node.x * distRatio, y: node.y * distRatio, z: (node.z ?? 0) * distRatio },
      { x: node.x, y: node.y, z: node.z ?? 0 },
      1000,
    );

    updateControls({
      type: 'selected',
      state: { kind: 'node', id: node.id, label: node.label },
    });
  }, [focusedNodeId, focusSource]);

  return (
    <GraphWindow>
      <GraphDiv ref={containerRef} />
      {loading && (
        <LoadingOverlay>
          <Spinner animation="border" variant="secondary" />
        </LoadingOverlay>
      )}
      <GraphControlsToolbar
        graphId={graphId}
        controls={controls}
        updateControls={updateControls}
        graphInstance={graphInstanceRef.current}
        nodeCount={nodeCount}
        loading={loading}
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
