import React, { useEffect, useReducer, useRef, useState } from 'react';
import { ErrorBoundary } from 'react-error-boundary';
import { Spinner, OverlayTrigger, Tooltip } from 'react-bootstrap';
import ForceGraph3D, { ForceGraph3DInstance } from '3d-force-graph';
import * as THREE from 'three';

import RenderErrorAlert from '@components/shared/alerts/RenderErrorAlert';
import { getNodeColor, getEdgeColor } from './styles';
import { FaTimes } from 'react-icons/fa';
import { FaFolderTree } from 'react-icons/fa6';
import { GraphControlsToolbar, NodeRenderMode, DagMode, createControlsReducer, buildNodeObject, buildEdgeLabelFactory, iconNodeVal } from './controls';
import type { LabelEntry } from './controls';
import { processInitialGraphData, getLinkEndpoints } from './data';
import { useGraphData } from '../data';
import type { GraphNode, GraphLink, GraphData } from './types';
import { applyGrowthToInstance } from './applyGrowth';
import DataPreviewPanel from './DataPreviewPanel';
import { AssociationTree } from '../browsing/AssociationTree';
import { GraphWindow, GraphDiv, LoadingOverlay, TreeOverlayToggle, TreeOverlayPanel, TreeOverlayHeader, MinimizeButton } from './AssociationGraph3D.styled';

interface AssociationGraphProps {
  inView: boolean;
}

interface FocusDistanceOpts {
  adjustDistance: boolean;
  distanceRatio: number;
}

const focusCameraOn = (
  gi: ForceGraph3DInstance,
  target: { x: number; y: number; z: number },
  distOpts: FocusDistanceOpts = { adjustDistance: false, distanceRatio: 1 },
  durationMs = 2000,
) => {
  const camPos = gi.cameraPosition();
  const orbitTarget = (gi.controls() as any)?.target;
  const currentDist = orbitTarget
    ? Math.hypot(
        camPos.x - orbitTarget.x,
        camPos.y - orbitTarget.y,
        camPos.z - orbitTarget.z,
      )
    : 150;

  const dist = distOpts.adjustDistance ? currentDist * distOpts.distanceRatio : currentDist;

  // Direction from target toward current camera — preserves viewing angle
  const dx = camPos.x - target.x;
  const dy = camPos.y - target.y;
  const dz = camPos.z - target.z;
  const dirLen = Math.hypot(dx, dy, dz);

  let ux: number, uy: number, uz: number;
  if (dirLen > 0.01) {
    ux = dx / dirLen;
    uy = dy / dirLen;
    uz = dz / dirLen;
  } else {
    ux = 0;
    uy = 0;
    uz = 1;
  }

  gi.cameraPosition(
    { x: target.x + ux * dist, y: target.y + uy * dist, z: target.z + uz * dist },
    target,
    durationMs,
  );

  if (distOpts.adjustDistance && distOpts.distanceRatio >= 2) {
    setTimeout(() => gi.zoomToFit(durationMs, 50), durationMs);
  }
};

const AssociationGraph3DInner: React.FC<AssociationGraphProps> = () => {
  const {
    graph, graphId, graphVersion, loading, grow, growToDepth,
    growable, reload, focusedNodeId, focusSource, setFocusedNode,
  } = useGraphData();

  const containerRef = useRef<HTMLDivElement>(null);
  const graphInstanceRef = useRef<ForceGraph3DInstance | null>(null);
  const graphDataRef = useRef<GraphData>({ nodes: [], links: [] });
  const labelSpritesRef = useRef<Map<string, LabelEntry>>(new Map());
  const edgeLabelSpritesRef = useRef<Map<string, LabelEntry>>(new Map());
  const animFrameRef = useRef<number>(0);
  const mountedVersionRef = useRef<number>(-1);
  const lastCamDistRef = useRef<number>(-1);

  const [nodeCount, setNodeCount] = useState(0);
  const [mounted, setMounted] = useState(false);
  const [previewMinimized, setPreviewMinimized] = useState(false);
  const [treeOverlayOpen, setTreeOverlayOpen] = useState(false);
  const handleNodeSelectRef = useRef<(node: GraphNode) => Promise<void>>(null as any);
  const handleEdgeSelectRef = useRef<(link: GraphLink) => void>(null as any);
  const focusSettingsRef = useRef({ focusOnClick: false, adjustDistance: false, distanceRatio: 1 });
  const labelScaleRef = useRef(1);
  const labelDensityRef = useRef(0.4);
  const labelMinSizeRef = useRef(1);
  const refitOnGrowRef = useRef(true);

  const controlsReducer = createControlsReducer(graphInstanceRef, labelSpritesRef, edgeLabelSpritesRef);

  const [controls, updateControls] = useReducer(controlsReducer, {
    filterChildless: false,
    depth: 1,
    showEdgeLabels: false,
    showNodeLabels: true,
    selectedElement: null,
    showNodeInfo: true,
    nodeRenderMode: 'icons' as NodeRenderMode,
    focusOnClick: true,
    adjustDistanceOnFocus: false,
    refitOnGrow: false,
    focusDistanceRatio: 1,
    labelScale: 1,
    edgeWidth: 1,
    edgeLength: 30,
    edgeLinkStrength: 0.5,
    edgeOpacity: 0.2,
    arrowLength: 3.5,
    directionalParticles: 1,
    particleSpeed: 0.006,
    nodeRelSize: 4,
    nodeOpacity: 0.75,
    enableNodeDrag: true,
    labelDensity: 0.4,
    labelMinSize: 1,
    chargeStrength: -200,
    velocityDecay: 0.4,
    warmupTicks: 0,
    cooldownTime: 15000,
    dagMode: null as DagMode,
    dagLevelDistance: null as number | null,
    numDimensions: 3 as 2 | 3,
  });

  focusSettingsRef.current = {
    focusOnClick: controls.focusOnClick,
    adjustDistance: controls.adjustDistanceOnFocus,
    distanceRatio: controls.focusDistanceRatio,
  };
  labelScaleRef.current = controls.labelScale;
  labelDensityRef.current = controls.labelDensity;
  labelMinSizeRef.current = controls.labelMinSize;
  refitOnGrowRef.current = controls.refitOnGrow;

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

      if (refitOnGrowRef.current && graphInstanceRef.current) {
        setTimeout(() => {
          graphInstanceRef.current?.zoomToFit(1000, 50);
        }, 800);
      }
    }
  }, [graphId, graphVersion]);

  const handleNodeSelect = async (node: GraphNode) => {
    updateControls({
      type: 'selected',
      state: { kind: 'node', id: node.id, label: node.label },
    });

    if (controls.focusOnClick) {
      setFocusedNode(node.id, 'graph');
      const gi = graphInstanceRef.current;
      if (gi && node.x !== undefined && node.y !== undefined) {
        focusCameraOn(gi, { x: node.x, y: node.y, z: node.z ?? 0 }, {
          adjustDistance: controls.adjustDistanceOnFocus,
          distanceRatio: controls.focusDistanceRatio,
        });
      }
    }

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

    if (controls.focusOnClick) {
      const gi = graphInstanceRef.current;
      if (gi) {
        const srcNode = graphDataRef.current.nodes.find((n) => n.id === source);
        const tgtNode = graphDataRef.current.nodes.find((n) => n.id === target);
        if (srcNode?.x !== undefined && tgtNode?.x !== undefined) {
          focusCameraOn(gi, {
            x: (srcNode.x + tgtNode.x) / 2,
            y: ((srcNode.y ?? 0) + (tgtNode.y ?? 0)) / 2,
            z: ((srcNode.z ?? 0) + (tgtNode.z ?? 0)) / 2,
          }, {
            adjustDistance: controls.adjustDistanceOnFocus,
            distanceRatio: controls.focusDistanceRatio,
          });
        }
      }
    }
  };
  handleEdgeSelectRef.current = handleEdgeSelect;

  // Mount the ForceGraph3D instance once data is ready
  useEffect(() => {
    if (!containerRef.current || !mounted) return;

    if (graphInstanceRef.current) {
      graphInstanceRef.current._destructor();
      graphInstanceRef.current = null;
    }
    labelSpritesRef.current.clear();
    edgeLabelSpritesRef.current.clear();

    const fg = new ForceGraph3D(containerRef.current, { controlType: 'orbit' })
      .graphData(graphDataRef.current)
      .backgroundColor('rgba(0,0,0,0)')
      .width(containerRef.current.clientWidth)
      .height(containerRef.current.clientHeight || window.innerHeight * 0.9)
      .nodeVal(controls.nodeRenderMode === 'icons'
        ? (iconNodeVal(controls.nodeRelSize) as any)
        : ((node: any) => (node as GraphNode).diameter))
      .nodeColor((node: any) => getNodeColor((node as GraphNode).nodeType, (node as GraphNode).visualState))
      .nodeLabel(() => '')
      .nodeThreeObject(buildNodeObject(controls.nodeRenderMode, controls.showNodeLabels, controls.nodeRelSize, controls.labelScale, labelSpritesRef.current) as any)
      .nodeThreeObjectExtend(controls.nodeRenderMode === 'spheres')
      .nodeRelSize(controls.nodeRelSize)
      .nodeOpacity(controls.nodeOpacity)
      .linkDirectionalArrowLength(controls.arrowLength)
      .linkDirectionalArrowRelPos(1)
      .linkThreeObjectExtend(controls.showEdgeLabels)
      .linkThreeObject(controls.showEdgeLabels
        ? ((link: any) => buildEdgeLabelFactory(controls.labelScale, edgeLabelSpritesRef.current)(link as GraphLink) as any)
        : (undefined as any))
      .linkColor(() => getEdgeColor())
      .linkWidth(controls.edgeWidth)
      .linkOpacity(controls.edgeOpacity)
      .linkCurvature((link: any) => ((link as GraphLink).bidirectional ? 0.2 : 0))
      .linkDirectionalParticles(controls.directionalParticles)
      .linkDirectionalParticleSpeed(controls.particleSpeed)
      .enableNodeDrag(controls.enableNodeDrag)
      .onNodeClick((node: any) => void handleNodeSelectRef.current?.(node as GraphNode))
      .onLinkClick((link: any) => handleEdgeSelectRef.current?.(link as GraphLink))
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
      const nodeLabels = labelSpritesRef.current;
      const edgeLabels = edgeLabelSpritesRef.current;
      const totalLabels = nodeLabels.size + edgeLabels.size;

      if (!gi || totalLabels === 0) {
        animFrameRef.current = requestAnimationFrame(updateLabelScaling);
        return;
      }

      const camPos = gi.cameraPosition();
      const controls = gi.controls() as any;
      const target = controls?.target;
      if (!target) {
        animFrameRef.current = requestAnimationFrame(updateLabelScaling);
        return;
      }

      // Keep orbit target in front of camera so zoom speed stays responsive.
      // After a focus animation the target can end up behind the camera when the
      // user scrolls toward a different cluster, collapsing the orbit radius and
      // making further zoom increments tiny.
      const cam = gi.camera();
      if (cam && controls) {
        const fwd = new THREE.Vector3(0, 0, -1).applyQuaternion(cam.quaternion);
        const toTarget = new THREE.Vector3(target.x - camPos.x, target.y - camPos.y, target.z - camPos.z);
        const proj = toTarget.dot(fwd);
        if (proj < 5) {
          const minDist = 50;
          target.set(camPos.x + fwd.x * minDist, camPos.y + fwd.y * minDist, camPos.z + fwd.z * minDist);
        }
      }

      const dist = Math.hypot(camPos.x - target.x, camPos.y - target.y, camPos.z - target.z);

      // Skip recalculation if camera hasn't moved significantly
      if (lastCamDistRef.current >= 0 && Math.abs(dist - lastCamDistRef.current) < lastCamDistRef.current * 0.005 + 0.1) {
        animFrameRef.current = requestAnimationFrame(updateLabelScaling);
        return;
      }
      lastCamDistRef.current = dist;

      let maxDegree = 1;
      nodeLabels.forEach((entry) => {
        if (entry.degree > maxDegree) maxDegree = entry.degree;
      });

      const scale = labelScaleRef.current;
      const distFactor = Math.max(1, dist / 300);
      const filterStart = 300 * scale;
      const filterEnd = 2000 * scale;
      const filterProgress = Math.min(1, Math.max(0, (dist - filterStart) / (filterEnd - filterStart)));
      const degreeThreshold = filterProgress * maxDegree * labelDensityRef.current;

      const minSize = labelMinSizeRef.current;
      const applyScaling = (entry: LabelEntry) => {
        if (entry.isInitial || entry.degree >= degreeThreshold) {
          entry.sprite.visible = true;
          const degreeBoost = entry.isInitial ? 1.5 : 1 + (entry.degree / maxDegree) * 0.5;
          const s = Math.max(minSize, distFactor * degreeBoost);
          entry.sprite.scale.set(entry.baseScale.x * s, entry.baseScale.y * s, entry.baseScale.z);
        } else {
          entry.sprite.visible = false;
        }
      };

      nodeLabels.forEach(applyScaling);
      edgeLabels.forEach((entry) => {
        entry.sprite.visible = true;
        const s = Math.max(minSize, distFactor);
        entry.sprite.scale.set(entry.baseScale.x * s, entry.baseScale.y * s, entry.baseScale.z);
      });

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

    const node = graphDataRef.current.nodes.find((n) => n.id === focusedNodeId);
    if (!node || node.x === undefined || node.y === undefined) return;

    updateControls({
      type: 'selected',
      state: { kind: 'node', id: node.id, label: node.label },
    });

    const gi = graphInstanceRef.current;
    if (gi) {
      const settings = focusSettingsRef.current;
      focusCameraOn(gi, { x: node.x, y: node.y, z: node.z ?? 0 }, {
        adjustDistance: settings.adjustDistance,
        distanceRatio: settings.distanceRatio,
      });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusedNodeId, focusSource]);

  return (
    <GraphWindow>
      <GraphDiv ref={containerRef} />
      {loading && (
        <LoadingOverlay>
          <Spinner animation="border" variant="secondary" />
        </LoadingOverlay>
      )}
      {treeOverlayOpen ? (
        <TreeOverlayPanel>
          <TreeOverlayHeader>
            <span>Browsing</span>
            <MinimizeButton onClick={() => setTreeOverlayOpen(false)}>
              <FaTimes size={12} />
            </MinimizeButton>
          </TreeOverlayHeader>
          <AssociationTree />
        </TreeOverlayPanel>
      ) : (
        <OverlayTrigger placement="right" overlay={<Tooltip>File Browser</Tooltip>}>
          <TreeOverlayToggle onClick={() => setTreeOverlayOpen(true)}>
            <FaFolderTree size={14} />
          </TreeOverlayToggle>
        </OverlayTrigger>
      )}
      <GraphControlsToolbar
        graphId={graphId}
        controls={controls}
        updateControls={updateControls}
        graphInstance={graphInstanceRef.current}
        nodeCount={nodeCount}
        loading={loading}
      />
      <DataPreviewPanel
        selectedElement={controls.selectedElement}
        nodeData={
          controls.selectedElement?.kind === 'node'
            ? graph.data_map[controls.selectedElement.id]
            : undefined
        }
        minimized={previewMinimized}
        onToggleMinimize={() => setPreviewMinimized((m) => !m)}
      />
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
