import { describe, expect, it } from 'vitest';

// project imports
import {
  banKindText,
  banMessage,
  countLabel,
  entityKindNoun,
  exclusionText,
  inclusionText,
  instanceStatusErrors,
  instanceStatusText,
  isInstanceReady,
  isRetryableReadStatus,
  isShortName,
  SHORT_NAME_LIMIT,
  portabilityLines,
  rootInstanceName,
  shortHash,
  sortedBans,
  sortedToolTags,
  summarizeDescription,
  toolInstanceName,
  partialDeleteMessage,
  toolVersionLabel,
  withArticle,
} from './display';
import { ModelBan } from '@models/modelconfigs';
import { ToolSetExclusionReason, ToolSetInclusion } from '@models/toolsets';
import { AgenticKinds } from '@models/agentic';

describe('tool version names', () => {
  it('reads semantic and custom labels', () => {
    expect(toolVersionLabel({ SemVer: '1.2.3' })).toBe('1.2.3');
    expect(toolVersionLabel({ Custom: 'nightly' })).toBe('nightly');
    expect(toolVersionLabel(null)).toBe('');
  });
  it('names instances the way the CLI does', () => {
    expect(toolInstanceName('strings', { SemVer: '1.0.0' })).toBe('strings@1.0.0');
    expect(rootInstanceName('triage', 3)).toBe('triage#r3');
  });
});

describe('instance status', () => {
  it('knows a Ready instance', () => {
    expect(isInstanceReady('Ready')).toBe(true);
    expect(isInstanceReady({ Invalid: { errors: [] } })).toBe(false);
  });
  it('describes the status as a line', () => {
    expect(instanceStatusText('Ready')).toBe('Ready');
    expect(instanceStatusText({ Invalid: { errors: ['no harness', 'dup'] } })).toBe('Invalid: no harness; dup');
    expect(instanceStatusText({ Invalid: { errors: [] } })).toBe('Invalid');
  });
  it('lists the reasons an instance is invalid', () => {
    expect(instanceStatusErrors('Ready')).toEqual([]);
    expect(instanceStatusErrors({ Invalid: { errors: ['no harness', 'dup'] } })).toEqual(['no harness', 'dup']);
    expect(instanceStatusErrors({ Invalid: { errors: [] } })).toEqual([]);
  });
});

describe('shortHash', () => {
  it('drops the scheme prefix before shortening', () => {
    expect(shortHash('v1:2297ee7d0123456789abcdef')).toBe('2297ee7d0123');
  });
  it('keeps a hash with no prefix', () => {
    expect(shortHash('abcdef', 4)).toBe('abcd');
  });
});

describe('inclusionText', () => {
  it('words each inclusion', () => {
    expect(inclusionText(ToolSetInclusion.ExplicitLatest)).toBe('explicit, follows latest');
    expect(inclusionText(ToolSetInclusion.ExplicitPinned)).toBe('explicit, pinned');
    expect(inclusionText(ToolSetInclusion.Selector)).toBe('matched by selector');
  });
});

describe('exclusionText', () => {
  it('words each reason', () => {
    const text = (reason: ToolSetExclusionReason, missing: string[] = []) => exclusionText({ reason, missing_groups: missing });
    expect(text(ToolSetExclusionReason.NotReadOnly)).toBe('declares effects, toolset is read only');
    expect(text(ToolSetExclusionReason.NotAgentic)).toBe('not an agentic tool');
    expect(text(ToolSetExclusionReason.NoLatest)).toBe('every version is deprecated');
    expect(text(ToolSetExclusionReason.NotTaggedInAllGroups, ['a', 'b'])).toBe('not tagged in a, b');
  });
});

describe('bans', () => {
  const older: ModelBan = { id: 'a', time_banned: '2026-01-01T00:00:00Z', ban_kind: { Generic: { msg: 'x' } } };
  const newer: ModelBan = { id: 'b', time_banned: '2026-02-01T00:00:00Z', ban_kind: {} };
  it('sorts newest first and tolerates no bans', () => {
    expect(sortedBans({ a: older, b: newer }).map((ban) => ban.id)).toEqual(['b', 'a']);
    expect(sortedBans(undefined)).toEqual([]);
  });
  it('names the ban kind', () => {
    expect(banKindText(older)).toBe('Generic');
    expect(banKindText(newer)).toBe('Ban');
  });
});

describe('summarizeDescription', () => {
  it('returns an empty string for no description', () => {
    expect(summarizeDescription(null)).toBe('');
    expect(summarizeDescription('')).toBe('');
  });

  it('keeps a plain description', () => {
    expect(summarizeDescription('Finds known vulnerabilities in software.')).toBe('Finds known vulnerabilities in software.');
  });

  it('skips leading rules and keeps only the first paragraph without markdown', () => {
    const text = '---\n\n`Balbuzard` is an older tool. See [the docs](https://example.test).\n\n---\n##### Usage\n\nRun it.\n';
    expect(summarizeDescription(text)).toBe('Balbuzard is an older tool. See the docs.');
  });

  it('skips a leading heading and strips html and entities', () => {
    expect(summarizeDescription('# Title\nSome **bold**&nbsp;text <b>here</b>')).toBe('Some bold text here');
  });

  it('cuts a long paragraph at a word boundary with an ellipsis', () => {
    const summary = summarizeDescription('word '.repeat(100), 50);
    expect(summary.endsWith('…')).toBe(true);
    expect(summary.length).toBeLessThanOrEqual(51);
    expect(
      summary
        .slice(0, -1)
        .split(' ')
        .every((word) => word === 'word'),
    ).toBe(true);
  });
});

describe('countLabel', () => {
  it('uses the singular only for one', () => {
    expect(countLabel(1, 'tool')).toBe('1 tool');
    expect(countLabel(0, 'tool')).toBe('0 tools');
    expect(countLabel(2, 'sub-agent')).toBe('2 sub-agents');
    expect(countLabel(1, 'toolset')).toBe('1 toolset');
  });
});

describe('ban messages', () => {
  const ban = (ban_kind: ModelBan['ban_kind']) => ({ id: 'b', time_banned: '2026-01-01T00:00:00Z', ban_kind });
  it('carries the message of the kinds that have one', () => {
    expect(banMessage(ban({ Generic: 'rate limited by hand' }))).toBe('Generic: rate limited by hand');
    expect(banMessage(ban({ CredentialRejected: { status: 401, message: 'bad key' } }))).toBe('CredentialRejected: 401 bad key');
    expect(banMessage(ban({ EndpointUnreachable: 'timeout' }))).toBe('EndpointUnreachable: timeout');
  });
  it('falls back to the kind name for the kinds without one', () => {
    expect(banMessage(ban('ModelRemoved'))).toBe('ModelRemoved');
    expect(banMessage(ban({}))).toBe('Ban');
  });
});

describe('entityKindNoun and withArticle', () => {
  it('names every entity kind in words rather than by its API name', () => {
    expect(entityKindNoun('WorkflowCatalog')).toBe('workflow');
    expect(entityKindNoun('ToolSetInstance')).toBe('toolset instance');
    for (const kind of Object.values(AgenticKinds)) expect(entityKindNoun(kind)).toMatch(/^[a-z ]+$/);
  });
  it('returns a kind it does not know as it is', () => {
    expect(entityKindNoun('Gadget')).toBe('Gadget');
  });
  it('uses "an" before a vowel and "a" before the rest', () => {
    expect(withArticle(entityKindNoun(AgenticKinds.Agent))).toBe('an agent');
    expect(withArticle(entityKindNoun(AgenticKinds.ToolInstance))).toBe('a tool version');
    expect(withArticle('Image')).toBe('an Image');
    expect(withArticle('Harness')).toBe('a Harness');
  });
});

describe('portabilityLines', () => {
  it('labels strict issues and Gemini rewrites on separate lines', () => {
    expect(
      portabilityLines({
        strict_compatible: false,
        strict_issues: ["`additionalProperties` at #/a isn't false"],
        gemini_compatible: true,
        gemini_rewrites: ['drop `additionalProperties` at #/a'],
      }),
    ).toEqual({
      strict: "Strict: not compatible (issues: `additionalProperties` at #/a isn't false)",
      gemini: 'Gemini: compatible (rewrites: drop `additionalProperties` at #/a)',
    });
  });
  it('names a family with no notes by its compatibility alone', () => {
    expect(portabilityLines({ strict_compatible: true, strict_issues: [], gemini_compatible: false, gemini_rewrites: [] })).toEqual({
      strict: 'Strict: compatible',
      gemini: 'Gemini: not compatible',
    });
  });
});

describe('sortedToolTags', () => {
  it('orders the known Tool tags and then the others alphabetically, dropping non-Tool tags', () => {
    const tags = { ToolSpecHash: 1, Other: 2, ToolKind: 3, ToolFunction: 4, ToolVersion: 5, ToolId: 6 };
    expect(sortedToolTags(tags).map(([key]) => key)).toEqual(['ToolId', 'ToolVersion', 'ToolKind', 'ToolSpecHash', 'ToolFunction']);
  });
  it('tolerates no tags', () => {
    expect(sortedToolTags({})).toEqual([]);
  });
});

describe('partialDeleteMessage', () => {
  it('leaves a refusal alone when nothing was deleted first', () => {
    expect(partialDeleteMessage('still referenced', [])).toBe('still referenced');
  });

  it('names the versions already deleted on a line before the refusal', () => {
    expect(partialDeleteMessage('still referenced trace: abc123', [{ SemVer: '1.1.0' }])).toBe(
      'The tool was not deleted, but version 1.1.0 was already deleted.\nstill referenced trace: abc123',
    );
    expect(partialDeleteMessage('still referenced', [{ SemVer: '1.1.0' }, { Custom: 'nightly' }])).toContain(
      'versions 1.1.0, nightly were already deleted',
    );
  });
});

describe('isShortName', () => {
  it('keeps a hyphenated name whole when it is within the limit', () => {
    expect(isShortName('demo-subagent')).toBe(true);
    expect(isShortName('x'.repeat(SHORT_NAME_LIMIT))).toBe(true);
  });

  it('lets a name over the limit break inside itself', () => {
    expect(isShortName('x'.repeat(SHORT_NAME_LIMIT + 1))).toBe(false);
    expect(isShortName('a-remarkably-long-hyphenated-lead-agent-name')).toBe(false);
  });
});

describe('isRetryableReadStatus', () => {
  it('treats a malformed id, a refusal, and a missing entity as permanent', () => {
    for (const status of [400, 401, 403, 404, 422]) expect(isRetryableReadStatus(status)).toBe(false);
  });
  it('treats no response, a timeout, a rate limit, and server errors as worth retrying', () => {
    for (const status of [0, 408, 429, 500, 502, 503]) expect(isRetryableReadStatus(status)).toBe(true);
  });
});
