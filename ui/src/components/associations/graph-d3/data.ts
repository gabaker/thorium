import { Graph, BranchNode, Direction } from '@models/trees';
import { Entities } from '@models/entities/entities';
import { formatSubmissionNames, formatTagNames, getEdgeLabel } from '../utilities';
import { getNodeSize, scoreNode } from './scaling';
import type { GraphNode, GraphLink, GraphData, NodeType, VisualState } from './types';

export const classifyNode = (nodeId: string, graph: Graph): { nodeType: NodeType; visualState: VisualState; label: string } => {
  const nodeData = graph.data_map[nodeId];
  const growable = graph.growable.map((n) => n.toString());
  const initial = graph.initial.map((n) => n.toString());
  const isGrowable = growable.includes(nodeId);
  const isInitial = initial.includes(nodeId);
  const visualState: VisualState = isGrowable ? 'growable' : isInitial ? 'initial' : 'basic';

  if ('Sample' in nodeData) {
    let label = formatSubmissionNames(nodeData.Sample?.submissions ?? []);
    if (label.length > 30) {
      label = label.substring(0, 15) + '...' + label.substring(label.length - 15);
    }
    return { nodeType: 'file', visualState, label };
  } else if ('Repo' in nodeData) {
    return { nodeType: 'repo', visualState, label: nodeData.Repo?.url ?? '' };
  } else if ('Tag' in nodeData) {
    return { nodeType: 'tag', visualState, label: formatTagNames(nodeData.Tag?.tags ?? {}, true) };
  } else if (nodeData.Entity?.kind && Object.keys(Entities).includes(nodeData.Entity.kind)) {
    const nodeType = nodeData.Entity.kind.toLowerCase() as NodeType;
    return { nodeType, visualState, label: nodeData.Entity.name };
  }
  return { nodeType: 'other', visualState, label: 'Unknown' };
};

export const buildGraphNode = (nodeId: string, graph: Graph, nodeCount: number): GraphNode => {
  const { nodeType, visualState, label } = classifyNode(nodeId, graph);
  const score = scoreNode(graph.data_map[nodeId]);
  return {
    id: nodeId,
    label,
    nodeType,
    visualState,
    score,
    diameter: getNodeSize(score, nodeCount),
  };
};

export const processInitialGraphData = (graph: Graph): GraphData => {
  const seenNodes = new Set<string>();
  const seenEdges = new Set<string>();
  const nodes: GraphNode[] = [];
  const links: GraphLink[] = [];
  const nodeCount = Object.keys(graph.data_map).length;

  const addNode = (nodeId: string) => {
    if (seenNodes.has(nodeId)) return;
    seenNodes.add(nodeId);
    nodes.push(buildGraphNode(nodeId, graph, nodeCount));
  };

  const addEdge = (source: string, target: BranchNode) => {
    const targetNode = target.direction === Direction.To ? target.node.toString() : source;
    const sourceNode = target.direction === Direction.To ? source : target.node.toString();
    const edgeKey = `${sourceNode}-${targetNode}-${target.relationship_hash}`;
    if (seenEdges.has(edgeKey) && target.direction !== Direction.Bidirectional) return;
    seenEdges.add(edgeKey);
    links.push({
      source: sourceNode,
      target: targetNode,
      label: getEdgeLabel(targetNode, sourceNode, target, graph),
      bidirectional: target.direction === Direction.Bidirectional,
    });
  };

  graph.initial.forEach((initialNode) => addNode(initialNode.toString()));

  Object.keys(graph.branches).forEach((nodeKey) => {
    addNode(nodeKey.toString());
    graph.branches[nodeKey].forEach((descendant: BranchNode) => {
      addNode(descendant.node.toString());
      addEdge(nodeKey, descendant);
    });
  });

  return { nodes, links };
};
