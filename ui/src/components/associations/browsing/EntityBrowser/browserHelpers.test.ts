import { describe, it, expect } from 'vitest';

// project imports
import { buildTreeIndex, childIdsOf } from '../treeHelpers';
import {
  collectGroupOptions,
  computeFlaggedNodes,
  effectiveChildren,
  filterTree,
  findFileNodeHash,
  getDepthFromClauses,
  getEntityLayerConfigFromClauses,
  groupByKind,
  nodeGroups,
  resolvePolicy,
  resolveRoots,
} from './browserHelpers';
import { FilterCriteria, LayerPolicy, TraversalConfig } from './types';
import { Clause, ClauseCondition } from '@components/shared/inputs/omnibar/ClauseTypes';
import { AssociationKind } from '@models/associations';
import { Entities } from '@models/entities';
import { type BranchNode, Direction, type Graph, NodeType, type TreeNode, TreeNodeKey } from '@models/trees';

/** A traversal config with sensible defaults for tests (Show everything, no depth bound). */
function mkCfg(overrides?: Partial<TraversalConfig>): TraversalConfig {
  return {
    clausePolicies: {},
    includeSet: null,
    defaultPolicies: {},
    fallback: LayerPolicy.Show,
    maxDepth: null,
    distances: new Map(),
    ...overrides,
  };
}

/** A filter criteria with no active filters by default. */
function mkCriteria(overrides?: Partial<FilterCriteria>): FilterCriteria {
  return { text: '', tags: {}, groups: [], flaggedOnly: false, flaggedNodes: new Set(), ...overrides };
}

/** Build a layer clause (`Show`/`Hide`/`Exclude`/`Include` is/is-one-of <types>). */
function layerClause(category: string, values: string[]): Clause {
  if (values.length === 1) {
    return { field: category, category, condition: ClauseCondition.Is, value: { value: values[0] } };
  }
  return { field: category, category, condition: ClauseCondition.IsOneOf, value: { values } };
}

const FILE_SHA = 'a'.repeat(64);
const FILE_HASH = 'file-hash';

// --- fixture builders ---

function sampleNode(sha256: string, name: string): TreeNode {
  return { [TreeNodeKey.Sample]: { sha256, submissions: [{ name }], tags: {} } } as unknown as TreeNode;
}

function entityNode(id: string, name: string, kind: Entities, tags: Record<string, Record<string, unknown>> = {}): TreeNode {
  return { [TreeNodeKey.Entity]: { id, name, kind, tags, description: null } } as unknown as TreeNode;
}

function tagNode(tags: Record<string, string[]>): TreeNode {
  return { [TreeNodeKey.Tag]: { tags } };
}

function assocBranch(node: string, kind: AssociationKind, direction: Direction, hash = `${node}-${kind}`): BranchNode {
  return { relationship: { Association: { kind, direction } }, node, direction, relationship_hash: hash } as BranchNode;
}

function tagBranch(node: string, direction = Direction.To): BranchNode {
  return { relationship: { Tags: 'Tags' }, node, direction, relationship_hash: `${node}-tags` };
}

function mkGraph(opts: {
  dataMap: Record<string, TreeNode>;
  branches: Record<string, BranchNode[]>;
  initial?: string[];
  growable?: string[];
}): Graph {
  return {
    id: 'tree-1',
    initial: opts.initial ?? [FILE_HASH],
    growable: opts.growable ?? [],
    data_map: opts.dataMap,
    branches: opts.branches,
  };
}

describe('findFileNodeHash', () => {
  it('finds the node id for a file sha256', () => {
    const graph = mkGraph({ dataMap: { [FILE_HASH]: sampleNode(FILE_SHA, 'f.bin') }, branches: {} });
    expect(findFileNodeHash(graph, FILE_SHA)).toBe(FILE_HASH);
  });
  it('returns undefined when absent', () => {
    const graph = mkGraph({ dataMap: { [FILE_HASH]: sampleNode(FILE_SHA, 'f.bin') }, branches: {} });
    expect(findFileNodeHash(graph, 'b'.repeat(64))).toBeUndefined();
  });
});

describe('buildTreeIndex (edge-carrying)', () => {
  it('carries relationship, direction and a label per edge', () => {
    const graph = mkGraph({
      dataMap: { [FILE_HASH]: sampleNode(FILE_SHA, 'f.bin'), e1: entityNode('id1', 'Dev', Entities.Device) },
      branches: { [FILE_HASH]: [assocBranch('e1', AssociationKind.FirmwareFor, Direction.To)] },
    });
    const idx = buildTreeIndex(graph);
    const edges = idx.childrenOf.get(FILE_HASH)!;
    expect(edges).toHaveLength(1);
    expect(edges[0].id).toBe('e1');
    expect(edges[0].direction).toBe(Direction.To);
    expect(edges[0].relationship.Association?.kind).toBe(AssociationKind.FirmwareFor);
    expect(edges[0].label).toContain('Association');
  });

  it('collapses the reverse-pair a directed edge stores on both endpoints', () => {
    // same relationship_hash stored as To on the file and From on the entity -> one edge under the file
    const graph = mkGraph({
      dataMap: { [FILE_HASH]: sampleNode(FILE_SHA, 'f.bin'), e1: entityNode('id1', 'Dev', Entities.Device) },
      branches: {
        [FILE_HASH]: [assocBranch('e1', AssociationKind.FirmwareFor, Direction.To, 'H')],
        e1: [assocBranch(FILE_HASH, AssociationKind.FirmwareFor, Direction.From, 'H')],
      },
    });
    const idx = buildTreeIndex(graph);
    expect(childIdsOf(idx, FILE_HASH)).toEqual(['e1']);
  });

  it('names the container for a "…In" association reached from the container side (To)', () => {
    // file --FileSystemIn--> filesystem : file is the source/container, stored To on the file
    const graph = mkGraph({
      dataMap: { [FILE_HASH]: sampleNode(FILE_SHA, 'f.bin'), fs: entityNode('idfs', 'dump.img', Entities.FileSystem) },
      branches: { [FILE_HASH]: [assocBranch('fs', AssociationKind.FileSystemIn, Direction.To)] },
    });
    const idx = buildTreeIndex(graph);
    const edge = idx.childrenOf.get(FILE_HASH)!.find((e) => e.id === 'fs')!;
    expect(edge.containerLabel).toBe('f.bin File');
  });

  it('names the container for a "…In" association reached from the contained side (From)', () => {
    // file --FileIn--> folder stored From on the file : the folder (source/container) becomes the parent
    const graph = mkGraph({
      dataMap: { [FILE_HASH]: sampleNode(FILE_SHA, 'f.bin'), folder: entityNode('idfold', 'somefolder', Entities.Folder) },
      branches: { [FILE_HASH]: [assocBranch('folder', AssociationKind.FileIn, Direction.From)] },
    });
    const idx = buildTreeIndex(graph);
    const edge = idx.childrenOf.get('folder')!.find((e) => e.id === FILE_HASH)!;
    expect(edge.containerLabel).toContain('somefolder');
  });

  it('does not add a container label for non-containment kinds (e.g. BasedIn)', () => {
    const graph = mkGraph({
      dataMap: { [FILE_HASH]: sampleNode(FILE_SHA, 'f.bin'), c: entityNode('idc', 'Elbonia', Entities.Vendor) },
      branches: { [FILE_HASH]: [assocBranch('c', AssociationKind.BasedIn, Direction.To)] },
    });
    const idx = buildTreeIndex(graph);
    const edge = idx.childrenOf.get(FILE_HASH)!.find((e) => e.id === 'c')!;
    expect(edge.containerLabel).toBeUndefined();
  });

  it('yields mutual parent/child entries for a bidirectional edge', () => {
    const graph = mkGraph({
      dataMap: { a: entityNode('ida', 'A', Entities.Device), b: entityNode('idb', 'B', Entities.Device) },
      branches: {
        a: [assocBranch('b', AssociationKind.AssociatedWith, Direction.Bidirectional, 'H')],
        b: [assocBranch('a', AssociationKind.AssociatedWith, Direction.Bidirectional, 'H')],
      },
      initial: ['a'],
    });
    const idx = buildTreeIndex(graph);
    expect(childIdsOf(idx, 'a')).toEqual(['b']);
    expect(childIdsOf(idx, 'b')).toEqual(['a']);
  });
});

describe('effectiveChildren', () => {
  it('returns direct children under the default Show policy, regardless of direction', () => {
    const graph = mkGraph({
      dataMap: {
        [FILE_HASH]: sampleNode(FILE_SHA, 'f.bin'),
        to: entityNode('t', 'Zeta', Entities.Device),
        from: entityNode('f', 'Alpha', Entities.Device),
        bi: entityNode('b', 'Mu', Entities.Device),
      },
      branches: {
        [FILE_HASH]: [
          assocBranch('to', AssociationKind.FirmwareFor, Direction.To),
          assocBranch('bi', AssociationKind.AssociatedWith, Direction.Bidirectional),
        ],
        from: [assocBranch(FILE_HASH, AssociationKind.DevelopedBy, Direction.From)],
      },
    });
    const idx = buildTreeIndex(graph);
    const kids = effectiveChildren(FILE_HASH, idx, graph, mkCfg(), new Set([FILE_HASH]));
    expect(kids.map((c) => c.edge.id).sort()).toEqual(['bi', 'from', 'to']);
  });

  it('prunes Skip layers entirely', () => {
    const graph = mkGraph({
      dataMap: {
        [FILE_HASH]: sampleNode(FILE_SHA, 'f.bin'),
        tag: tagNode({ foo: ['bar'] }),
        e1: entityNode('id1', 'Dev', Entities.Device),
      },
      branches: { [FILE_HASH]: [tagBranch('tag'), assocBranch('e1', AssociationKind.FirmwareFor, Direction.To)] },
    });
    const idx = buildTreeIndex(graph);
    const cfg = mkCfg({ clausePolicies: { [NodeType.Tag]: LayerPolicy.Skip } });
    const kids = effectiveChildren(FILE_HASH, idx, graph, cfg, new Set([FILE_HASH]));
    expect(kids.map((c) => c.edge.id)).toEqual(['e1']);
  });

  it('grafts descendants of a PassThrough layer with a breadcrumb', () => {
    // file -> process tree (PassThrough) -> process (Show)
    const graph = mkGraph({
      dataMap: {
        [FILE_HASH]: sampleNode(FILE_SHA, 'f.bin'),
        tree: entityNode('idtree', 'ProcTree', Entities.WindowsProcessTree),
        proc: entityNode('idproc', 'svchost.exe', Entities.WindowsProcess),
      },
      branches: {
        [FILE_HASH]: [assocBranch('tree', AssociationKind.ProcessTreeIn, Direction.To)],
        tree: [assocBranch('proc', AssociationKind.ChildProcess, Direction.To)],
      },
    });
    const idx = buildTreeIndex(graph);
    const kids = effectiveChildren(
      FILE_HASH,
      idx,
      graph,
      mkCfg({ clausePolicies: { [Entities.WindowsProcessTree]: LayerPolicy.PassThrough } }),
      new Set([FILE_HASH]),
    );
    expect(kids).toHaveLength(1);
    expect(kids[0].edge.id).toBe('proc');
    expect(kids[0].breadcrumb).toEqual(['ProcTree']);
  });

  it('guards cycles per path but still renders DAG re-convergence', () => {
    // root -> x -> z ; root -> y -> z (z reachable via two paths); and a cycle x -> root
    const graph = mkGraph({
      dataMap: {
        root: entityNode('idr', 'Root', Entities.Device),
        x: entityNode('idx', 'X', Entities.Device),
        y: entityNode('idy', 'Y', Entities.Device),
        z: entityNode('idz', 'Z', Entities.Device),
      },
      branches: {
        root: [
          assocBranch('x', AssociationKind.AssociatedWith, Direction.To),
          assocBranch('y', AssociationKind.AssociatedWith, Direction.To),
        ],
        x: [
          assocBranch('z', AssociationKind.AssociatedWith, Direction.To),
          assocBranch('root', AssociationKind.AssociatedWith, Direction.To),
        ],
        y: [assocBranch('z', AssociationKind.AssociatedWith, Direction.To)],
      },
      initial: ['root'],
    });
    const idx = buildTreeIndex(graph);
    // z renders under both x and y
    expect(effectiveChildren('x', idx, graph, mkCfg(), new Set(['root', 'x'])).map((c) => c.edge.id)).toContain('z');
    expect(effectiveChildren('y', idx, graph, mkCfg(), new Set(['root', 'y'])).map((c) => c.edge.id)).toEqual(['z']);
    // cycle back to root is guarded
    expect(effectiveChildren('x', idx, graph, mkCfg(), new Set(['root', 'x'])).map((c) => c.edge.id)).not.toContain('root');
  });

  it('prunes nodes beyond the depth bound', () => {
    const graph = mkGraph({
      dataMap: {
        [FILE_HASH]: sampleNode(FILE_SHA, 'f.bin'),
        near: entityNode('n', 'Near', Entities.Device),
        far: entityNode('f', 'Far', Entities.Device),
      },
      branches: {
        [FILE_HASH]: [assocBranch('near', AssociationKind.AssociatedWith, Direction.To)],
        near: [assocBranch('far', AssociationKind.AssociatedWith, Direction.To)],
      },
    });
    const idx = buildTreeIndex(graph);
    const distances = new Map([
      [FILE_HASH, 0],
      ['near', 1],
      ['far', 2],
    ]);
    // maxDepth 1 keeps `near` but prunes `far`
    const cfg = mkCfg({ maxDepth: 1, distances });
    expect(effectiveChildren('near', idx, graph, cfg, new Set([FILE_HASH, 'near'])).map((c) => c.edge.id)).toEqual([]);
    expect(effectiveChildren(FILE_HASH, idx, graph, cfg, new Set([FILE_HASH])).map((c) => c.edge.id)).toEqual(['near']);
  });
});

describe('resolvePolicy', () => {
  it('applies precedence: explicit clause > include(Show) > default > include-others(PassThrough) > fallback', () => {
    const cfg = mkCfg({
      clausePolicies: { [Entities.Device]: LayerPolicy.Skip },
      includeSet: new Set<NodeType>([Entities.SigmaRule]),
      defaultPolicies: { [NodeType.Tag]: LayerPolicy.Skip },
    });
    expect(resolvePolicy(Entities.Device, cfg)).toBe(LayerPolicy.Skip); // explicit wins
    expect(resolvePolicy(Entities.SigmaRule, cfg)).toBe(LayerPolicy.Show); // in whitelist
    expect(resolvePolicy(NodeType.Tag, cfg)).toBe(LayerPolicy.Skip); // default respected over include-others
    expect(resolvePolicy(Entities.Vendor, cfg)).toBe(LayerPolicy.PassThrough); // whitelist present, not listed
  });

  it('falls back when no include set is present', () => {
    expect(resolvePolicy(Entities.Vendor, mkCfg({ fallback: LayerPolicy.Show }))).toBe(LayerPolicy.Show);
  });
});

describe('clause extractors', () => {
  it('maps Show/Hide/Exclude/Include clauses to policies + includeSet', () => {
    const clauses = [
      layerClause('Show', [Entities.Device]),
      layerClause('Hide', [Entities.WindowsProcessTree]),
      layerClause('Exclude', [NodeType.Tag]),
      layerClause('Include', [Entities.SigmaRule, Entities.NetworkConnection]),
    ];
    const { policies, includeSet } = getEntityLayerConfigFromClauses(clauses);
    expect(policies[Entities.Device]).toBe(LayerPolicy.Show);
    expect(policies[Entities.WindowsProcessTree]).toBe(LayerPolicy.PassThrough);
    expect(includeSet?.has(Entities.SigmaRule)).toBe(true);
    expect(includeSet?.has(Entities.NetworkConnection)).toBe(true);
  });

  it('reads the traversal depth (last valid positive integer)', () => {
    expect(getDepthFromClauses([layerClause('depth', ['3'])], 1)).toBe(3);
    expect(getDepthFromClauses([], 1)).toBe(1);
  });
});

describe('computeFlaggedNodes', () => {
  it('flags Flag nodes, their ancestors, and danger-tagged nodes', () => {
    const graph = mkGraph({
      dataMap: {
        [FILE_HASH]: sampleNode(FILE_SHA, 'f.bin'),
        sig: entityNode('s', 'Rule', Entities.SigmaRule),
        flag: entityNode('fl', 'Suspicious', Entities.Flag),
        packed: entityNode('p', 'Packed', Entities.Device, { PACKED: { true: [] } }),
        clean: entityNode('c', 'Clean', Entities.Device),
      },
      branches: {
        [FILE_HASH]: [
          assocBranch('sig', AssociationKind.SigmaRuleHit, Direction.To),
          assocBranch('packed', AssociationKind.AssociatedWith, Direction.To),
          assocBranch('clean', AssociationKind.AssociatedWith, Direction.To),
        ],
        sig: [assocBranch('flag', AssociationKind.AssociatedWith, Direction.To)],
      },
    });
    const idx = buildTreeIndex(graph);
    const flagged = computeFlaggedNodes(graph, idx);
    expect(flagged.has('flag')).toBe(true); // the Flag itself
    expect(flagged.has('sig')).toBe(true); // ancestor of the Flag
    expect(flagged.has(FILE_HASH)).toBe(true); // ancestor of the Flag (any hop)
    expect(flagged.has('packed')).toBe(true); // danger-tagged
    expect(flagged.has('clean')).toBe(false); // neither
  });
});

describe('nodeGroups / collectGroupOptions', () => {
  it('reads entity groups and collects the graph-wide set', () => {
    const graph = mkGraph({
      dataMap: {
        [FILE_HASH]: sampleNode(FILE_SHA, 'f.bin'),
        d: {
          [TreeNodeKey.Entity]: { id: 'd', name: 'Dev', kind: Entities.Device, tags: {}, groups: ['alpha', 'beta'] },
        } as unknown as TreeNode,
      },
      branches: { [FILE_HASH]: [assocBranch('d', AssociationKind.FirmwareFor, Direction.To)] },
    });
    expect(nodeGroups(graph.data_map.d)).toEqual(['alpha', 'beta']);
    expect(collectGroupOptions(graph)).toEqual(['alpha', 'beta']);
  });
});

describe('groupByKind', () => {
  it('groups children by node type in first-appearance order', () => {
    const graph = mkGraph({
      dataMap: {
        [FILE_HASH]: sampleNode(FILE_SHA, 'f.bin'),
        d1: entityNode('d1', 'Dev1', Entities.Device),
        s1: entityNode('s1', 'Sig1', Entities.SigmaRule),
        d2: entityNode('d2', 'Dev2', Entities.Device),
      },
      branches: {
        [FILE_HASH]: [
          assocBranch('d1', AssociationKind.FirmwareFor, Direction.To),
          assocBranch('s1', AssociationKind.SigmaRuleHit, Direction.To),
          assocBranch('d2', AssociationKind.FirmwareFor, Direction.To),
        ],
      },
    });
    const idx = buildTreeIndex(graph);
    const kids = effectiveChildren(FILE_HASH, idx, graph, mkCfg(), new Set([FILE_HASH]));
    const groups = groupByKind(kids, graph);
    expect(groups.map((g) => g.nodeType)).toEqual([Entities.Device, Entities.SigmaRule]);
    expect(groups[0].children.map((c) => c.edge.id)).toEqual(['d1', 'd2']);
  });
});

describe('resolveRoots', () => {
  it('resolves a sha256 to the file node', () => {
    const graph = mkGraph({ dataMap: { [FILE_HASH]: sampleNode(FILE_SHA, 'f.bin') }, branches: {} });
    expect(resolveRoots(graph, { kind: 'sha256', sha256: FILE_SHA }).map((r) => r.id)).toEqual([FILE_HASH]);
  });
  it('passes explicit node roots through', () => {
    const graph = mkGraph({ dataMap: {}, branches: {} });
    expect(resolveRoots(graph, { kind: 'nodes', roots: [{ id: 'x', label: 'X' }] })).toEqual([{ id: 'x', label: 'X' }]);
  });
  it('ascends initial seeds to their tree root', () => {
    // root -> mid -> leaf; seed is the leaf
    const graph = mkGraph({
      dataMap: {
        root: entityNode('idr', 'Root', Entities.Device),
        mid: entityNode('idm', 'Mid', Entities.Device),
        leaf: entityNode('idl', 'Leaf', Entities.Device),
      },
      branches: {
        root: [assocBranch('mid', AssociationKind.AssociatedWith, Direction.To)],
        mid: [assocBranch('leaf', AssociationKind.AssociatedWith, Direction.To)],
      },
      initial: ['leaf'],
    });
    expect(resolveRoots(graph, { kind: 'initial' }).map((r) => r.id)).toEqual(['root']);
  });
});

describe('filterTree', () => {
  const buildFilterGraph = () =>
    mkGraph({
      dataMap: {
        [FILE_HASH]: sampleNode(FILE_SHA, 'root.bin'),
        keep: entityNode('k', 'NeedleEntity', Entities.Device),
        drop: entityNode('d', 'Unrelated', Entities.Device),
        danger: entityNode('g', 'Packed', Entities.Device, { PACKED: { true: [] } }),
      },
      branches: {
        [FILE_HASH]: [
          assocBranch('keep', AssociationKind.FirmwareFor, Direction.To),
          assocBranch('drop', AssociationKind.FirmwareFor, Direction.To),
          assocBranch('danger', AssociationKind.FirmwareFor, Direction.To),
        ],
      },
    });

  it('keeps text matches and their ancestors, drops the rest', () => {
    const graph = buildFilterGraph();
    const idx = buildTreeIndex(graph);
    const visible = filterTree([FILE_HASH], idx, graph, mkCriteria({ text: 'needle' }), mkCfg());
    expect(visible.has('keep')).toBe(true);
    expect(visible.has(FILE_HASH)).toBe(true); // ancestor kept
    expect(visible.has('drop')).toBe(false);
  });

  it('filters to flagged (danger-tagged) nodes', () => {
    const graph = buildFilterGraph();
    const idx = buildTreeIndex(graph);
    const flaggedNodes = computeFlaggedNodes(graph, idx);
    const visible = filterTree([FILE_HASH], idx, graph, mkCriteria({ flaggedOnly: true, flaggedNodes }), mkCfg());
    expect(visible.has('danger')).toBe(true);
    expect(visible.has('keep')).toBe(false);
    expect(visible.has(FILE_HASH)).toBe(true);
  });
});
