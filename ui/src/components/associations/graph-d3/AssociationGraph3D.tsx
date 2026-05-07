import React, { useEffect, useReducer, useRef, useState } from 'react';
import { ErrorBoundary } from 'react-error-boundary';
import { Spinner, OverlayTrigger, Tooltip } from 'react-bootstrap';
import ForceGraph3D, { ForceGraph3DInstance } from '3d-force-graph';
import * as THREE from 'three';

import RenderErrorAlert from '@components/shared/alerts/RenderErrorAlert';
import { getNodeColor, getEdgeColor } from './styles';
import { FaFolderTree } from 'react-icons/fa6';
import { GoSidebarCollapse } from 'react-icons/go';
import {
  GraphControlsToolbar,
  NodeRenderMode,
  DagMode,
  createControlsReducer,
  buildNodeObject,
  buildEdgeLabelFactory,
  iconNodeVal,
} from './controls';
import type { LabelEntry } from './controls';
import { processInitialGraphData, getLinkEndpoints } from './data';
import { useGraphData } from '../data';
import type { GraphNode, GraphLink, GraphData } from './types';
import { applyGrowthToInstance } from './applyGrowth';
import DataPreviewPanel from './DataPreviewPanel';
import { AssociationTree } from '../browsing/AssociationTree';
import {
  GraphWindow,
  GraphDiv,
  LoadingOverlay,
  TreeOverlayToggle,
  TreeOverlayPanel,
  TreeOverlayHeader,
  MinimizeButton,
  HoverTooltip,
} from './AssociationGraph3D.styled';

interface AssociationGraphProps {
  inView: boolean;
}

interface FocusDistanceOpts {
  adjustDistance: boolean;
  distanceRatio: number;
}

const MIN_FOCUS_DISTANCE = 120;
const MIN_ORBIT_RADIUS = 40;
const ORBIT_LERP_FACTOR = 0.15;
const ZOOM_SPEED = 1.5;
const LABEL_BASE_DISTANCE = 300;
const NODE_FILTER_START_FACTOR = 100;
const NODE_FILTER_END_FACTOR = 800;
const EDGE_FILTER_START_FACTOR = 80;
const EDGE_FILTER_END_FACTOR = 600;
const CAM_DIST_THRESHOLD_RATIO = 0.005;
const CAM_DIST_THRESHOLD_MIN = 0.1;
const INITIAL_FIT_DELAY_MS = 1500;
const LARGE_GRAPH_THRESHOLD = 100;
const LARGE_GRAPH_COOLDOWN_TICKS = 200;

const focusCameraOn = (
  gi: ForceGraph3DInstance,
  target: { x: number; y: number; z: number },
  distOpts: FocusDistanceOpts = { adjustDistance: false, distanceRatio: 1 },
  durationMs = 2000,
) => {
  const camPos = gi.cameraPosition();
  const orbitTarget = (gi.controls() as any)?.target;
  const currentDist = orbitTarget ? Math.hypot(camPos.x - orbitTarget.x, camPos.y - orbitTarget.y, camPos.z - orbitTarget.z) : 150;

  const dist = Math.max(MIN_FOCUS_DISTANCE, distOpts.adjustDistance ? currentDist * distOpts.distanceRatio : currentDist);

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

  gi.cameraPosition({ x: target.x + ux * dist, y: target.y + uy * dist, z: target.z + uz * dist }, target, durationMs);

  if (distOpts.adjustDistance && distOpts.distanceRatio >= 2) {
    setTimeout(() => gi.zoomToFit(durationMs, 50), durationMs);
  }
};

const AssociationGraph3DInner: React.FC = () => {
  const { graph, graphId, graphVersion, loading, grow, growToDepth, growable, reload, focusedNodeId, focusSource, setFocusedNode } =
    useGraphData();

  const containerRef = useRef<HTMLDivElement>(null);
  const graphInstanceRef = useRef<ForceGraph3DInstance | null>(null);
  const graphDataRef = useRef<GraphData>({ nodes: [], links: [] });
  const labelSpritesRef = useRef<Map<string, LabelEntry>>(new Map());
  const edgeLabelSpritesRef = useRef<Map<string, LabelEntry>>(new Map());
  const animFrameRef = useRef<number>(0);
  const mountedVersionRef = useRef<number>(-1);
  const lastCamDistRef = useRef<number>(-1);

  const tooltipRef = useRef<HTMLDivElement>(null);
  const gridRef = useRef<THREE.Group | null>(null);

  const [nodeCount, setNodeCount] = useState(0);
  const [mounted, setMounted] = useState(false);
  const [previewMinimized, setPreviewMinimized] = useState(false);
  const [treeOverlayOpen, setTreeOverlayOpen] = useState(false);
  const handleNodeSelectRef = useRef<((node: GraphNode) => Promise<void>) | null>(null);
  const handleEdgeSelectRef = useRef<((link: GraphLink) => void) | null>(null);
  const focusSettingsRef = useRef({ focusOnClick: false, adjustDistance: false, distanceRatio: 1 });
  const nodeLabelScaleRef = useRef(1);
  const edgeLabelScaleRef = useRef(1);
  const nodeLabelDensityRef = useRef(0.7);
  const edgeLabelDensityRef = useRef(0.7);
  const nodeLabelMinSizeRef = useRef(1);
  const edgeLabelMinSizeRef = useRef(1);
  const refitOnGrowRef = useRef(true);

  const controlsReducerRef = useRef(createControlsReducer(graphInstanceRef, labelSpritesRef, edgeLabelSpritesRef, lastCamDistRef));
  const controlsReducer = controlsReducerRef.current;

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
    nodeLabelScale: 1,
    edgeLabelScale: 1,
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
    nodeLabelDensity: 0.7,
    nodeLabelMinSize: 1,
    edgeLabelDensity: 0.7,
    edgeLabelMinSize: 1,
    chargeStrength: -200,
    velocityDecay: 0.4,
    warmupTicks: 0,
    cooldownTime: 15000,
    dagMode: null as DagMode,
    dagLevelDistance: null as number | null,
    numDimensions: 3 as 2 | 3,
    showGrid: false,
  });

  focusSettingsRef.current = {
    focusOnClick: controls.focusOnClick,
    adjustDistance: controls.adjustDistanceOnFocus,
    distanceRatio: controls.focusDistanceRatio,
  };
  nodeLabelScaleRef.current = controls.nodeLabelScale;
  edgeLabelScaleRef.current = controls.edgeLabelScale;
  nodeLabelDensityRef.current = controls.nodeLabelDensity;
  edgeLabelDensityRef.current = controls.edgeLabelDensity;
  nodeLabelMinSizeRef.current = controls.nodeLabelMinSize;
  edgeLabelMinSizeRef.current = controls.edgeLabelMinSize;
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
      lastCamDistRef.current = -1;

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

    const settings = focusSettingsRef.current;
    if (settings.focusOnClick) {
      setFocusedNode(node.id, 'graph');
      const gi = graphInstanceRef.current;
      if (gi && node.x !== undefined && node.y !== undefined) {
        focusCameraOn(
          gi,
          { x: node.x, y: node.y, z: node.z ?? 0 },
          {
            adjustDistance: settings.adjustDistance,
            distanceRatio: settings.distanceRatio,
          },
        );
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

    const settings = focusSettingsRef.current;
    if (settings.focusOnClick) {
      const gi = graphInstanceRef.current;
      if (gi) {
        const srcNode = graphDataRef.current.nodes.find((n) => n.id === source);
        const tgtNode = graphDataRef.current.nodes.find((n) => n.id === target);
        if (srcNode?.x !== undefined && tgtNode?.x !== undefined) {
          focusCameraOn(
            gi,
            {
              x: (srcNode.x + tgtNode.x) / 2,
              y: ((srcNode.y ?? 0) + (tgtNode.y ?? 0)) / 2,
              z: ((srcNode.z ?? 0) + (tgtNode.z ?? 0)) / 2,
            },
            {
              adjustDistance: settings.adjustDistance,
              distanceRatio: settings.distanceRatio,
            },
          );
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
      .nodeVal(
        controls.nodeRenderMode === 'icons' ? (iconNodeVal(controls.nodeRelSize) as any) : (node: any) => (node as GraphNode).diameter,
      )
      .nodeColor((node: any) => getNodeColor((node as GraphNode).nodeType, (node as GraphNode).visualState))
      .nodeLabel(() => '')
      .nodeThreeObject(
        buildNodeObject(
          controls.nodeRenderMode,
          controls.showNodeLabels,
          controls.nodeRelSize,
          controls.nodeLabelScale,
          labelSpritesRef.current,
          controls.nodeOpacity,
        ) as any,
      )
      .nodeThreeObjectExtend(controls.nodeRenderMode === 'spheres')
      .nodeRelSize(controls.nodeRelSize)
      .nodeOpacity(controls.nodeOpacity)
      .linkDirectionalArrowLength(controls.arrowLength)
      .linkDirectionalArrowRelPos(1)
      .linkThreeObjectExtend(controls.showEdgeLabels)
      .linkThreeObject(
        controls.showEdgeLabels
          ? (link: any) => buildEdgeLabelFactory(controls.edgeLabelScale, edgeLabelSpritesRef.current)(link as GraphLink) as any
          : (undefined as any),
      )
      .linkPositionUpdate(
        controls.showEdgeLabels
          ? (sprite: any, { start, end }: any) => {
              if (!sprite) return false;
              sprite.position.set((start.x + end.x) / 2, (start.y + end.y) / 2, (start.z + end.z) / 2);
              return false;
            }
          : (null as any),
      )
      .linkColor(() => getEdgeColor())
      .linkWidth(controls.edgeWidth)
      .linkOpacity(controls.edgeOpacity)
      .linkCurvature((link: any) => ((link as GraphLink).bidirectional ? 0.2 : 0))
      .linkDirectionalParticles(controls.directionalParticles)
      .linkDirectionalParticleSpeed(controls.particleSpeed)
      .enableNodeDrag(controls.enableNodeDrag)
      .onNodeClick((node: any) => void handleNodeSelectRef.current?.(node as GraphNode))
      .onLinkClick((link: any) => handleEdgeSelectRef.current?.(link as GraphLink))
      .onNodeHover((node: any) => {
        const tip = tooltipRef.current;
        if (!tip) return;
        if (node) {
          const gn = node as GraphNode;
          tip.textContent = gn.label;
          if (gn.x !== undefined && gn.y !== undefined && graphInstanceRef.current) {
            const coords = graphInstanceRef.current.graph2ScreenCoords(gn.x, gn.y, gn.z ?? 0);
            tip.style.left = `${coords.x + 15}px`;
            tip.style.top = `${coords.y + 15}px`;
          }
          tip.style.display = 'block';
        } else {
          tip.style.display = 'none';
        }
      })
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

    const orbitControls = fg.controls() as any;
    const freezeNodes = () => {
      const data = fg.graphData() as GraphData;
      for (const n of data.nodes) {
        if (n.x !== undefined) {
          (n as any).fx = n.x;
          (n as any).fy = n.y;
          (n as any).fz = n.z;
        }
      }
    };
    const unfreezeNodes = () => {
      const data = fg.graphData() as GraphData;
      for (const n of data.nodes) {
        (n as any).fx = undefined;
        (n as any).fy = undefined;
        (n as any).fz = undefined;
      }
    };
    if (orbitControls) {
      orbitControls.zoomToCursor = true;
      orbitControls.zoomSpeed = ZOOM_SPEED;
      orbitControls.addEventListener('start', freezeNodes);
      orbitControls.addEventListener('end', unfreezeNodes);
    }

    const gridGroup = new THREE.Group();
    const xzGrid = new THREE.GridHelper(2000, 40);
    (xzGrid.material as THREE.Material).opacity = 0.15;
    (xzGrid.material as THREE.Material).transparent = true;
    gridGroup.add(xzGrid);

    const xyGrid = new THREE.GridHelper(2000, 40);
    (xyGrid.material as THREE.Material).opacity = 0.1;
    (xyGrid.material as THREE.Material).transparent = true;
    xyGrid.rotation.x = Math.PI / 2;
    gridGroup.add(xyGrid);

    gridGroup.visible = controls.showGrid;
    fg.scene().add(gridGroup);
    gridRef.current = gridGroup;

    const enforceMinOrbitRadius = () => {
      const gi = graphInstanceRef.current;
      if (!gi) return;
      const ctrl = gi.controls() as any;
      if (!ctrl?.target) return;
      const cam = gi.camera();
      const target = ctrl.target as THREE.Vector3;
      const radius = cam.position.distanceTo(target);
      if (radius < MIN_ORBIT_RADIUS) {
        const fwd = new THREE.Vector3(0, 0, -1).applyQuaternion(cam.quaternion);
        target.set(
          cam.position.x + fwd.x * MIN_ORBIT_RADIUS,
          cam.position.y + fwd.y * MIN_ORBIT_RADIUS,
          cam.position.z + fwd.z * MIN_ORBIT_RADIUS,
        );
      }
    };
    containerRef.current!.addEventListener('wheel', enforceMinOrbitRadius, { capture: true, passive: true });

    if (graphDataRef.current.nodes.length > LARGE_GRAPH_THRESHOLD) {
      fg.cooldownTicks(LARGE_GRAPH_COOLDOWN_TICKS);
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

      const dist = Math.hypot(camPos.x - target.x, camPos.y - target.y, camPos.z - target.z);

      // Skip recalculation if camera hasn't moved significantly
      const camThreshold = lastCamDistRef.current * CAM_DIST_THRESHOLD_RATIO + CAM_DIST_THRESHOLD_MIN;
      if (lastCamDistRef.current >= 0 && Math.abs(dist - lastCamDistRef.current) < camThreshold) {
        animFrameRef.current = requestAnimationFrame(updateLabelScaling);
        return;
      }
      lastCamDistRef.current = dist;

      let maxDegree = 1;
      nodeLabels.forEach((entry) => {
        if (entry.degree > maxDegree) maxDegree = entry.degree;
      });

      const camVec = new THREE.Vector3(camPos.x, camPos.y, camPos.z);

      const computeVisibility = (
        itemDist: number,
        degree: number,
        isInitial: boolean,
        filterStart: number,
        filterEnd: number,
        density: number,
      ) => {
        if (density >= 1) return true;
        const progress = Math.min(1, Math.max(0, (itemDist - filterStart) / (filterEnd - filterStart)));
        const threshold = progress * maxDegree * (1 - density);
        return isInitial || degree >= threshold;
      };

      const nodeScale = nodeLabelScaleRef.current;
      const nodeFilterStart = NODE_FILTER_START_FACTOR * nodeScale;
      const nodeFilterEnd = NODE_FILTER_END_FACTOR * nodeScale;
      const nodeDensity = nodeLabelDensityRef.current;
      const nodeMinSize = nodeLabelMinSizeRef.current;

      nodeLabels.forEach((entry) => {
        const parent = entry.sprite.parent;
        const nodeDist = parent ? camVec.distanceTo(parent.position) : dist;
        if (computeVisibility(nodeDist, entry.degree, entry.isInitial, nodeFilterStart, nodeFilterEnd, nodeDensity)) {
          entry.sprite.visible = true;
          const nodeDistFactor = Math.max(1, nodeDist / LABEL_BASE_DISTANCE);
          const degreeBoost = entry.isInitial ? 1.5 : 1 + (entry.degree / maxDegree) * 0.5;
          const s = Math.max(nodeMinSize, nodeDistFactor * degreeBoost);
          entry.sprite.scale.set(entry.baseScale.x * s, entry.baseScale.y * s, entry.baseScale.z);
        } else {
          entry.sprite.visible = false;
        }
      });

      const edgeScale = edgeLabelScaleRef.current;
      const edgeFilterStart = EDGE_FILTER_START_FACTOR * edgeScale;
      const edgeFilterEnd = EDGE_FILTER_END_FACTOR * edgeScale;
      const edgeDensity = edgeLabelDensityRef.current;
      const edgeMinSize = edgeLabelMinSizeRef.current;
      const worldPos = new THREE.Vector3();

      edgeLabels.forEach((entry) => {
        entry.sprite.getWorldPosition(worldPos);
        const edgeDist = camVec.distanceTo(worldPos);
        if (computeVisibility(edgeDist, entry.degree, entry.isInitial, edgeFilterStart, edgeFilterEnd, edgeDensity)) {
          entry.sprite.visible = true;
          const edgeDistFactor = Math.max(1, edgeDist / LABEL_BASE_DISTANCE);
          const s = Math.max(edgeMinSize, edgeDistFactor);
          entry.sprite.scale.set(entry.baseScale.x * s, entry.baseScale.y * s, entry.baseScale.z);
        } else {
          entry.sprite.visible = false;
        }
      });

      animFrameRef.current = requestAnimationFrame(updateLabelScaling);
    };
    animFrameRef.current = requestAnimationFrame(updateLabelScaling);

    const fitTimeout = setTimeout(() => {
      if (graphInstanceRef.current) {
        graphInstanceRef.current.zoomToFit(1000, 50);
      }
    }, INITIAL_FIT_DELAY_MS);

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
      if (orbitControls) {
        orbitControls.removeEventListener('start', freezeNodes);
        orbitControls.removeEventListener('end', unfreezeNodes);
      }
      container.removeEventListener('wheel', enforceMinOrbitRadius, { capture: true });
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

  useEffect(() => {
    if (gridRef.current) gridRef.current.visible = controls.showGrid;
  }, [controls.showGrid]);

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
      focusCameraOn(
        gi,
        { x: node.x, y: node.y, z: node.z ?? 0 },
        {
          adjustDistance: settings.adjustDistance,
          distanceRatio: settings.distanceRatio,
        },
      );
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusedNodeId, focusSource]);

  return (
    <GraphWindow>
      <GraphDiv ref={containerRef} />
      <HoverTooltip ref={tooltipRef} />
      {loading && (
        <LoadingOverlay>
          <Spinner animation="border" variant="secondary" />
        </LoadingOverlay>
      )}
      {treeOverlayOpen ? (
        <TreeOverlayPanel>
          <TreeOverlayHeader>
            <MinimizeButton onClick={() => setTreeOverlayOpen(false)}>
              <GoSidebarCollapse size={14} />
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
        nodeData={controls.selectedElement?.kind === 'node' ? graph.data_map[controls.selectedElement.id] : undefined}
        minimized={previewMinimized}
        onToggleMinimize={() => setPreviewMinimized((m) => !m)}
      />
    </GraphWindow>
  );
};

export const AssociationGraph3D: React.FC<AssociationGraphProps> = ({ inView }) => {
  return (
    <ErrorBoundary fallback={<RenderErrorAlert page={false} />}>{inView && <AssociationGraph3DInner />}</ErrorBoundary>
  );
};

export default AssociationGraph3D;
