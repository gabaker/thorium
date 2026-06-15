import { describe, expect, it } from 'vitest';

// project imports
import { barToClause, collectSampleSha256s, deriveStats } from './deriveStats';
import { BucketSource, SeriesKey, type BucketCount } from './types';
import { ClauseCondition } from '@components/shared/inputs/omnibar/ClauseTypes';
import { Entities } from '@models/entities';
import type { Sample } from '@models/files';
import { TreeNode, TreeNodeKey } from '@models/trees';

/**
 * Build a minimal {@link Sample} node with the given tags and submission names.
 *
 * @param tags - The `FileType` (or other) tag map, shaped `{ key: { value: groups } }`.
 * @param names - Submission file names (drive the extension fallback).
 * @returns A `TreeNode` wrapping the sample.
 */
function fileNode(tags: Sample['tags'], names: (string | undefined)[] = []): TreeNode {
  const submissions = names.map((name, i) => ({
    id: `sub-${i}`,
    name,
    description: null,
    groups: ['corn'],
    submitter: 'farmer',
    uploaded: '2026-07-06T00:00:00Z',
    origin: { None: 'None' as const },
  }));
  const sample = {
    sha256: `sha-${Math.random()}`,
    sha1: '',
    md5: '',
    tags,
    submissions,
    comments: [],
  } as Sample;
  return { [TreeNodeKey.Sample]: sample };
}

/**
 * Build a minimal entity node of the given kind.
 *
 * @param kind - The entity kind string.
 * @returns A `TreeNode` wrapping an entity with only the fields `deriveStats` reads.
 */
function entityNode(kind: string): TreeNode {
  return { [TreeNodeKey.Entity]: { kind } as TreeNode[TreeNodeKey.Entity] };
}

/** A minimal repo node. */
function repoNode(): TreeNode {
  return { [TreeNodeKey.Repo]: {} as TreeNode[TreeNodeKey.Repo] };
}

/**
 * Assemble a `data_map` from a list of nodes with generated ids.
 *
 * @param nodes - The nodes to place in the map.
 * @returns A `{ [nodeId]: TreeNode }` map.
 */
function dataMap(nodes: TreeNode[]): Record<string, TreeNode> {
  const map: Record<string, TreeNode> = {};
  nodes.forEach((node, i) => {
    map[`node-${i}`] = node;
  });
  return map;
}

/** Find the single bucket with the given label in a series (throws if absent for a clear failure). */
function bucketByLabel(buckets: BucketCount[], label: string): BucketCount {
  const found = buckets.find((b) => b.label === label);
  if (!found) throw new Error(`no bucket labeled "${label}" in [${buckets.map((b) => b.label).join(', ')}]`);
  return found;
}

describe('deriveStats', () => {
  it('returns an empty map for an empty data_map', () => {
    expect(deriveStats({})).toEqual(new Map());
  });

  it('groups FileType-tagged files into clickable Tag buckets', () => {
    const stats = deriveStats(
      dataMap([
        fileNode({ FileType: { PE32: ['corn'] } }, ['a.exe']),
        fileNode({ FileType: { PE32: ['corn'] } }, ['b.dll']),
        fileNode({ FileType: { ELF: ['corn'] } }, ['c']),
      ]),
    );
    const files = stats.get(SeriesKey.Files)!;
    const pe = bucketByLabel(files, 'PE32');
    expect(pe.value).toBe(2);
    expect(pe.source).toBe(BucketSource.Tag);
    expect(pe.clickable).toBe(true);
    expect(pe.bucket).toBe('all');
    const elf = bucketByLabel(files, 'ELF');
    expect(elf.value).toBe(1);
    expect(elf.clickable).toBe(true);
    // buckets are sorted by descending count
    expect(files[0].label).toBe('PE32');
  });

  it('falls back to non-clickable extension buckets for untagged files', () => {
    const stats = deriveStats(dataMap([fileNode({}, ['first.exe']), fileNode({}, ['second.exe']), fileNode({}, ['notes.txt'])]));
    const files = stats.get(SeriesKey.Files)!;
    const exe = bucketByLabel(files, '.exe');
    expect(exe.value).toBe(2);
    expect(exe.source).toBe(BucketSource.Extension);
    expect(exe.clickable).toBe(false);
    const txt = bucketByLabel(files, '.txt');
    expect(txt.value).toBe(1);
    expect(txt.clickable).toBe(false);
  });

  it('counts a tagged file only in its Tag bucket (never the extension fallback)', () => {
    const stats = deriveStats(dataMap([fileNode({ FileType: { PE32: ['corn'] } }, ['tagged.exe'])]));
    const files = stats.get(SeriesKey.Files)!;
    // the .exe extension must not appear because the file is FileType-tagged
    expect(files.some((b) => b.source === BucketSource.Extension)).toBe(false);
    expect(files).toHaveLength(1);
    expect(files[0].label).toBe('PE32');
  });

  it('counts distinct extensions once per file across multiple submission names', () => {
    const stats = deriveStats(dataMap([fileNode({}, ['dup.exe', 'dup.exe', 'other.bin'])]));
    const files = stats.get(SeriesKey.Files)!;
    // a single file with two .exe submission names contributes 1 to .exe (deduped within the file)
    expect(bucketByLabel(files, '.exe').value).toBe(1);
    expect(bucketByLabel(files, '.bin').value).toBe(1);
  });

  it('buckets an untagged, nameless file under "(none)"', () => {
    const stats = deriveStats(dataMap([fileNode({}, [])]));
    const files = stats.get(SeriesKey.Files)!;
    const none = bucketByLabel(files, '(none)');
    expect(none.value).toBe(1);
    expect(none.clickable).toBe(false);
  });

  it('emits a single clickable raw-count bar for repos', () => {
    const stats = deriveStats(dataMap([repoNode(), repoNode(), repoNode()]));
    const repos = stats.get(SeriesKey.Repos)!;
    expect(repos).toHaveLength(1);
    expect(repos[0].value).toBe(3);
    expect(repos[0].label).toBe(Entities.Repo);
    expect(repos[0].source).toBe(BucketSource.Kind);
    expect(repos[0].clickable).toBe(true);
  });

  it('emits one clickable bar per entity kind', () => {
    const stats = deriveStats(dataMap([entityNode(Entities.Device), entityNode(Entities.Device), entityNode(Entities.Vendor)]));
    const entities = stats.get(SeriesKey.Entities)!;
    const device = bucketByLabel(entities, Entities.Device);
    expect(device.value).toBe(2);
    expect(device.source).toBe(BucketSource.Kind);
    expect(device.clickable).toBe(true);
    expect(bucketByLabel(entities, Entities.Vendor).value).toBe(1);
    // highest count first
    expect(entities[0].label).toBe(Entities.Device);
  });

  it('buckets a kind-less entity under Other', () => {
    const stats = deriveStats(dataMap([{ [TreeNodeKey.Entity]: {} as TreeNode[TreeNodeKey.Entity] }]));
    const entities = stats.get(SeriesKey.Entities)!;
    expect(bucketByLabel(entities, Entities.Other).value).toBe(1);
  });

  it('ignores Tag nodes and does not create a series for them', () => {
    const stats = deriveStats(dataMap([{ [TreeNodeKey.Tag]: { tags: { FileType: ['PE32'] } } }]));
    expect(stats.size).toBe(0);
  });

  it('derives all series together from a mixed data_map', () => {
    const stats = deriveStats(
      dataMap([fileNode({ FileType: { PE32: ['corn'] } }, ['a.exe']), fileNode({}, ['b.txt']), repoNode(), entityNode(Entities.Device)]),
    );
    expect(stats.get(SeriesKey.Files)).toHaveLength(2);
    expect(stats.get(SeriesKey.Repos)).toHaveLength(1);
    expect(stats.get(SeriesKey.Entities)).toHaveLength(1);
  });
});

/** Build a Sample node with an explicit sha256 (the shared `fileNode` randomizes it). */
function fileNodeWithSha(sha256: string): TreeNode {
  const sample: Sample = { sha256, sha1: '', md5: '', tags: {}, submissions: [], comments: [] };
  return { [TreeNodeKey.Sample]: sample };
}

describe('collectSampleSha256s', () => {
  it('returns an empty array for an empty data_map', () => {
    expect(collectSampleSha256s({})).toEqual([]);
  });

  it('collects the sha256 of every Sample node', () => {
    const result = collectSampleSha256s(dataMap([fileNodeWithSha('aaa'), fileNodeWithSha('bbb')]));
    expect(result).toEqual(['aaa', 'bbb']);
  });

  it('dedupes repeated sha256s, preserving first-seen order', () => {
    const result = collectSampleSha256s(dataMap([fileNodeWithSha('aaa'), fileNodeWithSha('bbb'), fileNodeWithSha('aaa')]));
    expect(result).toEqual(['aaa', 'bbb']);
  });

  it('ignores non-Sample nodes', () => {
    const result = collectSampleSha256s(dataMap([fileNodeWithSha('aaa'), repoNode(), entityNode(Entities.Device)]));
    expect(result).toEqual(['aaa']);
  });
});

describe('barToClause', () => {
  it('maps a Kind bucket to an Include clause whitelisting that kind', () => {
    const bucket: BucketCount = {
      bucket: 'all',
      label: Entities.Device,
      value: 3,
      source: BucketSource.Kind,
      clickable: true,
    };
    const clause = barToClause(SeriesKey.Entities, bucket);
    expect(clause).toEqual({
      category: 'Include',
      field: 'Include',
      condition: ClauseCondition.Is,
      value: { value: Entities.Device },
    });
  });

  it('maps the repo Kind bucket to an Include clause for repos', () => {
    const bucket: BucketCount = {
      bucket: 'all',
      label: Entities.Repo,
      value: 2,
      source: BucketSource.Kind,
      clickable: true,
    };
    const clause = barToClause(SeriesKey.Repos, bucket);
    expect(clause).toEqual({
      category: 'Include',
      field: 'Include',
      condition: ClauseCondition.Is,
      value: { value: Entities.Repo },
    });
  });

  it('maps a Tag bucket to an is-one-of FileType tag clause', () => {
    const bucket: BucketCount = {
      bucket: 'all',
      label: 'PE32',
      value: 5,
      source: BucketSource.Tag,
      clickable: true,
    };
    const clause = barToClause(SeriesKey.Files, bucket);
    expect(clause).toEqual({
      category: 'tag',
      field: 'FileType',
      condition: ClauseCondition.IsOneOf,
      value: { values: ['PE32'] },
    });
  });

  it('returns null for a non-clickable Extension bucket', () => {
    const bucket: BucketCount = {
      bucket: 'all',
      label: '.exe',
      value: 4,
      source: BucketSource.Extension,
      clickable: false,
    };
    expect(barToClause(SeriesKey.Files, bucket)).toBeNull();
  });
});
