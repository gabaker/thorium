import { describe, it, expect } from 'vitest';

// project imports
import { buildTreeIndex, childIdsOf, defaultBidirectional, TreeOrientation } from '../treeHelpers';
import {
  collectGroupOptions,
  computeFlaggedNodes,
  effectiveChildren,
  filterTree,
  findFileNodeHash,
  focusBreadcrumb,
  getDepthFromClauses,
  getDisplayTags,
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

  it('drops a hidden leaf child', () => {
    const graph = mkGraph({
      dataMap: {
        [FILE_HASH]: sampleNode(FILE_SHA, 'f.bin'),
        keep: entityNode('k', 'Keep', Entities.Device),
        gone: entityNode('g', 'Gone', Entities.Device),
      },
      branches: {
        [FILE_HASH]: [
          assocBranch('keep', AssociationKind.AssociatedWith, Direction.To),
          assocBranch('gone', AssociationKind.AssociatedWith, Direction.To),
        ],
      },
    });
    const idx = buildTreeIndex(graph);
    const cfg = mkCfg({ hiddenNodes: new Set(['gone']) });
    expect(effectiveChildren(FILE_HASH, idx, graph, cfg, new Set([FILE_HASH])).map((c) => c.edge.id)).toEqual(['keep']);
  });

  it('hiding a mid-tree node hides its entire subtree', () => {
    // file -> mid -> deep ; hiding `mid` must also drop `deep`
    const graph = mkGraph({
      dataMap: {
        [FILE_HASH]: sampleNode(FILE_SHA, 'f.bin'),
        mid: entityNode('m', 'Mid', Entities.Device),
        deep: entityNode('d', 'Deep', Entities.Device),
      },
      branches: {
        [FILE_HASH]: [assocBranch('mid', AssociationKind.AssociatedWith, Direction.To)],
        mid: [assocBranch('deep', AssociationKind.AssociatedWith, Direction.To)],
      },
    });
    const idx = buildTreeIndex(graph);
    const cfg = mkCfg({ hiddenNodes: new Set(['mid']) });
    // mid is gone from the file's children, and nothing grafts `deep` up in its place
    expect(effectiveChildren(FILE_HASH, idx, graph, cfg, new Set([FILE_HASH])).map((c) => c.edge.id)).toEqual([]);
  });

  it('hiding a pass-through node suppresses the descendants it would otherwise graft up', () => {
    // file -> tree (PassThrough) -> proc (Show). Hiding the pass-through node drops the whole branch, so
    // `proc` is NOT grafted onto the file (hidden check runs before the policy check).
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
    const cfg = mkCfg({
      clausePolicies: { [Entities.WindowsProcessTree]: LayerPolicy.PassThrough },
      hiddenNodes: new Set(['tree']),
    });
    expect(effectiveChildren(FILE_HASH, idx, graph, cfg, new Set([FILE_HASH])).map((c) => c.edge.id)).toEqual([]);
  });

  it('hides a DAG-duplicate node under every parent (hiding is by node id)', () => {
    // root -> x -> z ; root -> y -> z. Hiding `z` removes it under both x and y.
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
        x: [assocBranch('z', AssociationKind.AssociatedWith, Direction.To)],
        y: [assocBranch('z', AssociationKind.AssociatedWith, Direction.To)],
      },
      initial: ['root'],
    });
    const idx = buildTreeIndex(graph);
    const cfg = mkCfg({ hiddenNodes: new Set(['z']) });
    expect(effectiveChildren('x', idx, graph, cfg, new Set(['root', 'x'])).map((c) => c.edge.id)).toEqual([]);
    expect(effectiveChildren('y', idx, graph, cfg, new Set(['root', 'y'])).map((c) => c.edge.id)).toEqual([]);
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

describe('effectiveChildren — bidirectional relationship display', () => {
  /** A bidirectional-aware config (the entity browser's default view policy). */
  const bidiCfg = (overrides?: Partial<TraversalConfig>) =>
    mkCfg({ orientation: TreeOrientation.Down, bidirectional: defaultBidirectional, ...overrides });

  /** SigmaRule --SigmaRuleHit--> Flag --AssociatedWith--> WindowsProcess, all stored `To` from the source. */
  const sigmaChain = () =>
    mkGraph({
      dataMap: {
        sig: entityNode('s', 'Rule', Entities.SigmaRule),
        flag: entityNode('fl', 'Suspicious', Entities.Flag),
        proc: entityNode('p', 'powershell.exe', Entities.WindowsProcess),
      },
      branches: {
        sig: [assocBranch('flag', AssociationKind.SigmaRuleHit, Direction.To)],
        flag: [assocBranch('proc', AssociationKind.AssociatedWith, Direction.To)],
      },
      initial: ['proc'],
    });

  it('surfaces a WindowsProcess → Flag → SigmaRule via reverse edges (relationship kinds)', () => {
    const graph = sigmaChain();
    const idx = buildTreeIndex(graph);
    // from the process, its Flag is reachable as a reversed child
    const procKids = effectiveChildren('proc', idx, graph, bidiCfg(), new Set(['proc']));
    expect(procKids.map((c) => c.edge.id)).toEqual(['flag']);
    expect(procKids[0].viaReversed).toBe(true);
    expect(procKids[0].reverseDepth).toBe(1);
    // from that Flag (arrived via reverse at depth 1), the SigmaRule is the next reversed child
    const flagKids = effectiveChildren('flag', idx, graph, bidiCfg(), new Set(['proc', 'flag']), 1, true);
    expect(flagKids.map((c) => c.edge.id)).toEqual(['sig']);
    expect(flagKids[0].reverseDepth).toBe(2);
  });

  it('a reverse-reached SigmaRule does not fan back out to its other forward children', () => {
    // sig hits two flags; arriving at sig via reverse from flagA must NOT re-list flagB
    const graph = mkGraph({
      dataMap: {
        sig: entityNode('s', 'Rule', Entities.SigmaRule),
        flagA: entityNode('fa', 'FlagA', Entities.Flag),
        flagB: entityNode('fb', 'FlagB', Entities.Flag),
      },
      branches: {
        sig: [
          assocBranch('flagA', AssociationKind.SigmaRuleHit, Direction.To),
          assocBranch('flagB', AssociationKind.SigmaRuleHit, Direction.To),
        ],
      },
      initial: ['flagA'],
    });
    const idx = buildTreeIndex(graph);
    // sig arrived via reverse (depth 2): forward flags suppressed
    expect(effectiveChildren('sig', idx, graph, bidiCfg(), new Set(['flagA', 'sig']), 2, true).map((c) => c.edge.id)).toEqual([]);
  });

  it('keeps containment (Folder → File) directional — a File never lists its Folder as a child', () => {
    // folder --FileIn--> file (structural). Even under the bidirectional cfg the file has no reverse child.
    const graph = mkGraph({
      dataMap: { folder: entityNode('fold', 'dir', Entities.Folder), file: entityNode('fl', 'f.bin', Entities.File) },
      branches: { folder: [assocBranch('file', AssociationKind.FileIn, Direction.To)] },
      initial: ['file'],
    });
    const idx = buildTreeIndex(graph);
    expect(effectiveChildren('file', idx, graph, bidiCfg(), new Set(['file'])).map((c) => c.edge.id)).toEqual([]);
    // the folder still shows the file as a forward child
    expect(effectiveChildren('folder', idx, graph, bidiCfg(), new Set(['folder'])).map((c) => c.edge.id)).toEqual(['file']);
  });

  it('no longer reverse-surfaces a FirmwareFor edge (now structural under the whitelist)', () => {
    // device --FirmwareFor--> file: FirmwareFor left the whitelist, so from the file the device is NOT a child
    const graph = mkGraph({
      dataMap: { device: entityNode('d', 'Router', Entities.Device), file: entityNode('fl', 'fw.bin', Entities.File) },
      branches: { device: [assocBranch('file', AssociationKind.FirmwareFor, Direction.To)] },
      initial: ['file'],
    });
    const idx = buildTreeIndex(graph);
    expect(effectiveChildren('file', idx, graph, bidiCfg(), new Set(['file'])).map((c) => c.edge.id)).toEqual([]);
    // forward direction is unaffected — the device still lists the file
    expect(effectiveChildren('device', idx, graph, bidiCfg(), new Set(['device'])).map((c) => c.edge.id)).toEqual(['file']);
  });

  it('surfaces the real flag chain: WindowsProcess → Flag (FlagFor) → SigmaRule (CreatedBy)', () => {
    // the actual backend shape: SigmaRule -CreatedBy-> Flag -FlagFor-> WindowsProcess (both stored To from source)
    const graph = mkGraph({
      dataMap: {
        sig: entityNode('s', 'Rule', Entities.SigmaRule),
        flag: entityNode('fl', 'Suspicious', Entities.Flag),
        proc: entityNode('p', 'powershell.exe', Entities.WindowsProcess),
      },
      branches: {
        sig: [assocBranch('flag', AssociationKind.CreatedBy, Direction.To)],
        flag: [assocBranch('proc', AssociationKind.FlagFor, Direction.To)],
      },
      initial: ['proc'],
    });
    const idx = buildTreeIndex(graph);
    // from the process, the Flag surfaces as a reversed child (FlagFor is reverse-eligible)
    const procKids = effectiveChildren('proc', idx, graph, bidiCfg(), new Set(['proc']));
    expect(procKids.map((c) => c.edge.id)).toEqual(['flag']);
    expect(procKids[0].viaReversed).toBe(true);
    // from that Flag (reverseDepth 1), the SigmaRule surfaces via the reversed CreatedBy edge
    const flagKids = effectiveChildren('flag', idx, graph, bidiCfg(), new Set(['proc', 'flag']), 1, true);
    expect(flagKids.map((c) => c.edge.id)).toEqual(['sig']);
  });
});

describe('filterTree — reverse-reachable matches', () => {
  it('keeps a reverse-reachable SigmaRule (and the process ancestor) when it matches text', () => {
    const graph = mkGraph({
      dataMap: {
        proc: entityNode('p', 'powershell.exe', Entities.WindowsProcess),
        flag: entityNode('fl', 'Suspicious', Entities.Flag),
        sig: entityNode('s', 'NeedleRule', Entities.SigmaRule),
      },
      branches: {
        sig: [assocBranch('flag', AssociationKind.SigmaRuleHit, Direction.To)],
        flag: [assocBranch('proc', AssociationKind.AssociatedWith, Direction.To)],
      },
      initial: ['proc'],
    });
    const idx = buildTreeIndex(graph);
    const cfg = mkCfg({ orientation: TreeOrientation.Down, bidirectional: defaultBidirectional });
    const visible = filterTree(['proc'], idx, graph, mkCriteria({ text: 'needle' }), cfg);
    // the sigma rule (reverse-reached from the process) matches and its reverse-ancestors stay visible
    expect(visible.has('sig')).toBe(true);
    expect(visible.has('flag')).toBe(true);
    expect(visible.has('proc')).toBe(true);
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

  it('excludes a hidden matching node (and its subtree) from the visible set', () => {
    // `keep` matches "needle" but is hidden, so filterTree — which walks via effectiveChildren — never
    // reaches it and it is not kept.
    const graph = buildFilterGraph();
    const idx = buildTreeIndex(graph);
    const cfg = mkCfg({ hiddenNodes: new Set(['keep']) });
    const visible = filterTree([FILE_HASH], idx, graph, mkCriteria({ text: 'needle' }), cfg);
    expect(visible.has('keep')).toBe(false);
  });
});

describe('resolveRoots — hidden roots (dropped by the body consumer)', () => {
  it('still resolves a root that is hidden (the EntityBrowserBody filters hidden roots out at render)', () => {
    // resolveRoots itself is hidden-agnostic — hidden roots are removed where roots are consumed, not here.
    const graph = mkGraph({ dataMap: { [FILE_HASH]: sampleNode(FILE_SHA, 'f.bin') }, branches: {} });
    const roots = resolveRoots(graph, { kind: 'sha256', sha256: FILE_SHA });
    expect(roots.map((r) => r.id)).toEqual([FILE_HASH]);
    // the consumer drops hidden roots with a plain filter
    const hiddenNodes = new Set([FILE_HASH]);
    expect(roots.filter((r) => !hiddenNodes.has(r.id))).toEqual([]);
  });
});

describe('getDisplayTags', () => {
  it('flattens key/value pairs and drops suppressed keys', () => {
    const node = entityNode('e1', 'thing', Entities.Flag, {
      FileType: { PE32: [] },
      submitter: { alice: [] },
      Parent: { 'abc…': [] },
      FolderAllSha256: { deadbeef: [] },
    });
    const { shown, overflow } = getDisplayTags(node);
    // only FileType survives; submitter/Parent/FolderAllSha256 are suppressed
    expect(shown).toEqual([{ key: 'FileType', value: 'PE32' }]);
    expect(overflow).toBe(0);
  });

  it('orders pairs by key then value regardless of insertion order', () => {
    const node = entityNode('e1', 'thing', Entities.Flag, {
      Zeta: { b: [], a: [] },
      Alpha: { two: [] },
    });
    const { shown } = getDisplayTags(node);
    expect(shown).toEqual([
      { key: 'Alpha', value: 'two' },
      { key: 'Zeta', value: 'a' },
      { key: 'Zeta', value: 'b' },
    ]);
  });

  it('caps at the limit and reports the overflow count + labels', () => {
    const node = entityNode('e1', 'thing', Entities.Flag, {
      K: { v1: [], v2: [], v3: [], v4: [] },
    });
    const { shown, overflow, overflowLabels } = getDisplayTags(node, 2);
    expect(shown).toEqual([
      { key: 'K', value: 'v1' },
      { key: 'K', value: 'v2' },
    ]);
    expect(overflow).toBe(2);
    expect(overflowLabels).toEqual(['K: v3', 'K: v4']);
  });

  it('returns an empty set for a node with no (non-suppressed) tags', () => {
    const node = entityNode('e1', 'thing', Entities.Flag, { Results: { r: [] } });
    expect(getDisplayTags(node)).toEqual({ shown: [], overflow: 0, overflowLabels: [] });
  });
});

describe('focusBreadcrumb', () => {
  // root --(To)--> mid --(To)--> leaf : parentsOf(leaf)=[mid], parentsOf(mid)=[root]
  function chainGraph(): Graph {
    return mkGraph({
      dataMap: {
        root: entityNode('idr', 'Root', Entities.Device),
        mid: entityNode('idm', 'Mid', Entities.Folder),
        leaf: entityNode('idl', 'Leaf', Entities.Flag),
      },
      branches: {
        root: [assocBranch('mid', AssociationKind.ChildProcess, Direction.To)],
        mid: [assocBranch('leaf', AssociationKind.ChildProcess, Direction.To)],
      },
      initial: ['root'],
    });
  }

  it('returns the ancestor chain top→down including the focus root, with labels', () => {
    const graph = chainGraph();
    const idx = buildTreeIndex(graph);
    const crumbs = focusBreadcrumb(graph, idx, 'leaf');
    expect(crumbs.map((c) => c.id)).toEqual(['root', 'mid', 'leaf']);
    expect(crumbs.map((c) => c.label)).toEqual(['Root', 'Mid', 'Leaf']);
  });

  it('returns just the focus root when it has no parent', () => {
    const graph = chainGraph();
    const idx = buildTreeIndex(graph);
    expect(focusBreadcrumb(graph, idx, 'root').map((c) => c.id)).toEqual(['root']);
  });
});
