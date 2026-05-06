import type { ForceGraph3DInstance } from '3d-force-graph';

import { getLinkEndpoints } from './data';
import type { GraphData } from './types';
import type { LabelEntry } from './controls/controlsReducer';

export const applyGrowthToInstance = (
  prevData: GraphData,
  newData: GraphData,
  graphInstanceRef: React.RefObject<ForceGraph3DInstance | null>,
  labelSpritesRef: React.RefObject<Map<string, LabelEntry>>,
  graphDataRef: React.RefObject<GraphData>,
  setNodeCount: (count: number) => void,
) => {
  const gi = graphInstanceRef.current;
  if (!gi) return;

  const existingEdgeKeys = new Set(
    prevData.links.map((l) => {
      const { source, target } = getLinkEndpoints(l);
      return `${source}-${target}`;
    }),
  );

  const addedLinks = newData.links.filter((l) => {
    const { source, target } = getLinkEndpoints(l);
    return !existingEdgeKeys.has(`${source}-${target}`);
  });

  const newNodeMap = new Map(newData.nodes.map((n) => [n.id, n]));
  const existingNodeIds = new Set(prevData.nodes.map((n) => n.id));
  const addedNodes = newData.nodes.filter((n) => !existingNodeIds.has(n.id));

  let stateChanged = false;
  const updatedExistingNodes = prevData.nodes.map((n) => {
    const updated = newNodeMap.get(n.id);
    if (updated && updated.visualState !== n.visualState) {
      stateChanged = true;
      return { ...n, visualState: updated.visualState };
    }
    return n;
  });

  if (addedNodes.length === 0 && addedLinks.length === 0 && !stateChanged) return;

  const updatedData: GraphData = {
    nodes: [...updatedExistingNodes, ...addedNodes],
    links: [...prevData.links, ...addedLinks],
  };
  graphDataRef.current = updatedData;
  setNodeCount(updatedData.nodes.length);

  gi.graphData(updatedData);

  if (stateChanged) {
    labelSpritesRef.current.clear();
    gi.nodeThreeObject(gi.nodeThreeObject());
    gi.refresh();
  }
};
