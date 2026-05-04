import * as THREE from 'three';
import SpriteText from 'three-spritetext';
import type { ForceGraph3DInstance } from '3d-force-graph';

import { getNodeColor, getNodeSvg, svgToTexture } from '../styles';
import type { GraphControls, DisplayAction, NodeRenderMode } from './types';
import type { GraphNode } from '../types';

export type LabelEntry = { sprite: THREE.Object3D; degree: number; isInitial: boolean; baseScale: THREE.Vector3 };

export const buildNodeObject = (
  renderMode: NodeRenderMode,
  showLabels: boolean,
  nodeRelSize: number,
  labelMap?: Map<string, LabelEntry>,
) => {
  const sizeFactor = nodeRelSize / 4;
  return (node: GraphNode): THREE.Object3D => {
    const group = new THREE.Group();

    if (renderMode === 'icons') {
      const svgString = getNodeSvg(node.nodeType, node.visualState);
      const texture = svgToTexture(svgString, 64);
      const spriteMaterial = new THREE.SpriteMaterial({ map: texture, transparent: true });
      const sprite = new THREE.Sprite(spriteMaterial);
      const scale = Math.max(6, node.diameter / 3) * sizeFactor;
      sprite.scale.set(scale, scale, 1);
      group.add(sprite);
    }

    if (showLabels) {
      const labelSprite = new SpriteText(node.label);
      labelSprite.color = getNodeColor(node.nodeType, node.visualState);
      labelSprite.textHeight = 3;
      (labelSprite as any).position.y = renderMode === 'icons' ? -(node.diameter / 5 + 4) * sizeFactor : -(node.diameter / 5 + 2);
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

export const createControlsReducer = (
  graphInstanceRef: React.RefObject<ForceGraph3DInstance | null>,
  labelSpritesRef: React.RefObject<Map<string, LabelEntry>>,
) => {
  return (state: GraphControls, action: DisplayAction): GraphControls => {
    const gi = graphInstanceRef.current;
    switch (action.type) {
      case 'showEdgeLabels': {
        if (gi) gi.linkLabel(action.state ? 'label' : () => '');
        return { ...state, showEdgeLabels: action.state };
      }
      case 'showNodeLabels': {
        if (gi) {
          labelSpritesRef.current.clear();
          gi.nodeThreeObject(buildNodeObject(state.nodeRenderMode, action.state, state.nodeRelSize, labelSpritesRef.current) as any);
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
          gi.nodeThreeObject(buildNodeObject(action.state, state.showNodeLabels, state.nodeRelSize, labelSpritesRef.current) as any);
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
        if (gi) {
          gi.nodeRelSize(action.state);
          if (state.nodeRenderMode === 'icons') {
            labelSpritesRef.current.clear();
            gi.nodeThreeObject(buildNodeObject(state.nodeRenderMode, state.showNodeLabels, action.state, labelSpritesRef.current) as any);
            gi.refresh();
          }
        }
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
  };
};
