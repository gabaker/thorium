import type { NodeObject, LinkObject } from '3d-force-graph';

export type NodeType = 'file' | 'repo' | 'tag' | 'device' | 'vendor' | 'collection' | 'filesystem' | 'folder' | 'other';
export type VisualState = 'basic' | 'growable' | 'initial';

export interface GraphNode extends NodeObject {
  id: string;
  label: string;
  nodeType: NodeType;
  visualState: VisualState;
  score: number;
  diameter: number;
  degree: number;
}

export interface GraphLink extends LinkObject<GraphNode> {
  source: string;
  target: string;
  label: string;
  bidirectional: boolean;
}

export interface GraphData {
  nodes: GraphNode[];
  links: GraphLink[];
}
