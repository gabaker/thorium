import { describe, it, expect } from 'vitest';

// project imports
import { buildTagBrowseHref, filterIncludedTags, filterExcludedTags, getTagColorClass, getTagBadgeText } from './utilities';
import { paramsToClauses } from '@components/shared/inputs/omnibar/urlState';
import { Entities } from '@models/entities';
import type { Tags } from '@models/tags';

function makeTags(entries: Record<string, Record<string, string[]>>): Tags {
  return entries;
}

describe('buildTagBrowseHref', () => {
  it('builds a browse URL for a simple resource with the tag param', () => {
    expect(buildTagBrowseHref(Entities.File, 'family', 'emotet')).toBe('/files?tags%5Bfamily%5D=emotet');
    expect(buildTagBrowseHref(Entities.Repo, 'lang', 'rust')).toBe('/repos?tags%5Blang%5D=rust');
  });

  it('uses the correct multi-segment browse path (not `${resource}s`)', () => {
    // regression: WindowsProcess must map to /windows/processes, not /windowsprocesss
    const href = buildTagBrowseHref(Entities.WindowsProcess, 'name', 'svchost.exe');
    expect(href).toContain('/windows/processes?');
    expect(href).not.toContain('windowsprocess');
  });

  it('produces a URL that decodes back to a tag clause (round-trip through the omnibar)', () => {
    const href = buildTagBrowseHref(Entities.Device, 'vendor', 'Acme Corp')!;
    const params = new URLSearchParams(href.split('?')[1]);
    const clause = paramsToClauses(params).find((c) => c.category === 'tag');
    expect(clause?.field).toBe('vendor');
    expect(clause && !('values' in clause.value) ? clause.value.value : undefined).toBe('Acme Corp');
  });
});

describe('filterIncludedTags', () => {
  it('returns only tags whose keys appear in the include list (case-insensitive)', () => {
    const tags = makeTags({ TLP: { RED: ['RED'] }, FileType: { PE: ['PE'] }, MBC: { T1059: ['T1059'] } });
    const result = filterIncludedTags(tags, ['tlp', 'mbc']);
    expect(Object.keys(result)).toEqual(['TLP', 'MBC']);
  });

  it('returns empty object when no keys match', () => {
    const tags = makeTags({ TLP: { RED: ['RED'] } });
    expect(filterIncludedTags(tags, ['OTHER'])).toEqual({});
  });

  it('handles empty tags object', () => {
    expect(filterIncludedTags({}, ['TLP'])).toEqual({});
  });

  it('handles empty include list', () => {
    expect(filterIncludedTags(makeTags({ TLP: { RED: ['RED'] } }), [])).toEqual({});
  });
});

describe('filterExcludedTags', () => {
  it('removes tags whose keys appear in the exclude list (case-insensitive)', () => {
    const tags = makeTags({ TLP: { RED: ['RED'] }, FileType: { PE: ['PE'] }, MBC: { T1059: ['T1059'] } });
    const result = filterExcludedTags(tags, ['filetype']);
    expect(Object.keys(result)).toEqual(['TLP', 'MBC']);
  });

  it('returns all tags when no keys match the exclude list', () => {
    const tags = makeTags({ TLP: { RED: ['RED'] }, MBC: { T1059: ['T1059'] } });
    const result = filterExcludedTags(tags, ['OTHER']);
    expect(Object.keys(result)).toEqual(['TLP', 'MBC']);
  });

  it('returns empty object when all keys are excluded', () => {
    const tags = makeTags({ TLP: { RED: ['RED'] } });
    expect(filterExcludedTags(tags, ['TLP'])).toEqual({});
  });
});

describe('getTagColorClass', () => {
  it('returns tlp-red-btn for TLP RED', () => {
    expect(getTagColorClass('TLP', 'RED')).toBe('tlp-red-btn');
  });

  it('returns tlp-amber-btn for TLP AMBER', () => {
    expect(getTagColorClass('TLP', 'AMBER')).toBe('tlp-amber-btn');
  });

  it('returns tlp-amber-btn for TLP AMBER+STRICT', () => {
    expect(getTagColorClass('TLP', 'AMBER+STRICT')).toBe('tlp-amber-btn');
  });

  it('returns tlp-green-btn for TLP GREEN', () => {
    expect(getTagColorClass('TLP', 'GREEN')).toBe('tlp-green-btn');
  });

  it('returns tlp-clear-btn for TLP WHITE', () => {
    expect(getTagColorClass('TLP', 'WHITE')).toBe('tlp-clear-btn');
  });

  it('returns tlp-clear-btn for TLP CLEAR', () => {
    expect(getTagColorClass('TLP', 'CLEAR')).toBe('tlp-clear-btn');
  });

  it('is case-insensitive for TLP key', () => {
    expect(getTagColorClass('tlp', 'red')).toBe('tlp-red-btn');
  });

  it('returns general-tag for RESULTS key', () => {
    expect(getTagColorClass('RESULTS', 'anything')).toBe('general-tag');
  });

  it('returns attack-tag for ATT&CK key', () => {
    expect(getTagColorClass('ATT&CK', 'T1059')).toBe('attack-tag');
  });

  it('returns mbc-tag for MBC key', () => {
    expect(getTagColorClass('MBC', 'T1059')).toBe('mbc-tag');
  });

  it('returns info-tag for file info keys like FILETYPE', () => {
    expect(getTagColorClass('FileType', 'PE')).toBe('info-tag');
  });

  it('returns danger-tag for danger keys like CLAMAV', () => {
    expect(getTagColorClass('ClamAV', 'detected')).toBe('danger-tag');
  });

  it('returns other-tag for unknown keys', () => {
    expect(getTagColorClass('CustomTag', 'value')).toBe('other-tag');
  });
});

describe('getTagBadgeText', () => {
  it('returns value only for TLP non-condensed', () => {
    expect(getTagBadgeText('TLP', 'red', false)).toBe('RED');
  });

  it('returns TLP: VALUE for TLP condensed', () => {
    expect(getTagBadgeText('TLP', 'red', true)).toBe('TLP: RED');
  });

  it('returns value only for ATT&CK', () => {
    expect(getTagBadgeText('ATT&CK', 'T1059.001', false)).toBe('T1059.001');
  });

  it('returns value only for MBC', () => {
    expect(getTagBadgeText('MBC', 'T1059', true)).toBe('T1059');
  });

  it('returns key: value for RESULTS', () => {
    expect(getTagBadgeText('RESULTS', 'clean', false)).toBe('RESULTS: clean');
  });

  it('returns key: value for unknown keys', () => {
    expect(getTagBadgeText('Custom', 'data', false)).toBe('Custom: data');
  });
});
