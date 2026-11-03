import { describe, expect, it } from 'vitest';

// project imports
import {
  banCount,
  camelWords,
  capabilityNames,
  catalogBans,
  catalogIds,
  credentialRef,
  idSegments,
  isLongNameChunk,
  isSqueezableNameChunk,
  kindWords,
  isLongLimit,
  limitsParts,
  limitsText,
  nameChunks,
  messageParts,
  parameterEntries,
  parameterNameChunks,
  parametersText,
  providerTarget,
  splitLongNameChunk,
  taggedDetails,
  variantText,
} from './format';
import { ModelBan, ModelCredential } from '@models/modelconfigs';

describe('model formatting', () => {
  it('splits a CamelCase variant name into its words', () => {
    expect(camelWords('OpenAiCompatible')).toEqual(['Open', 'Ai', 'Compatible']);
    expect(camelWords('Succeeded')).toEqual(['Succeeded']);
    expect(camelWords('AwsAssumeRole').join('')).toBe('AwsAssumeRole');
  });
  it('splits ids only after separators', () => {
    expect(idSegments('us.anthropic.claude-sonnet-4-5-20250929-v1:0')).toEqual([
      'us.',
      'anthropic.',
      'claude-',
      'sonnet-',
      '4-',
      '5-',
      '20250929-',
      'v1:0',
    ]);
    expect(idSegments('uiloop-extra-model')).toEqual(['uiloop-', 'extra-', 'model']);
    expect(idSegments('models/gemini_pro@001')).toEqual(['models/', 'gemini_', 'pro@001']);
    // a short tail stays with the piece before it, so a long id never strands its last character after a colon
    expect(idSegments('anthropic.claude-sonnet-4-5-20250929-v1:0')).toEqual([
      'anthropic.',
      'claude-',
      'sonnet-',
      '4-',
      '5-',
      '20250929-',
      'v1:0',
    ]);
    expect(idSegments('a-b')).toEqual(['a-b']);
    expect(idSegments('98f4255836b1')).toEqual(['98f4255836b1']);
    expect(idSegments('')).toEqual([]);
    expect(idSegments('a-b.c').join('')).toBe('a-b.c');
  });
  it('names variants', () => {
    expect(variantText('Idle')).toBe('Idle');
    expect(variantText({ Failed: 'x' })).toBe('Failed');
    expect(variantText({ state: 'Succeeded', error: null })).toBe('Succeeded');
    expect(variantText(null)).toBe('');
  });
  it('counts bans', () => {
    expect(banCount({ a: {} as never, b: {} as never })).toBe(2);
    expect(banCount(undefined)).toBe(0);
  });
  it('lists catalog ids', () => {
    expect(catalogIds([{ model_id: 'a' }, { model_id: 'b' }])).toEqual(['a', 'b']);
    expect(catalogIds(undefined)).toEqual([]);
  });
  it('lists the bans on catalog models with their model ids', () => {
    const ban = { id: 'b1' } as ModelBan;
    expect(catalogBans([{ model_id: 'a', bans: { b1: ban } }, { model_id: 'b' }, { model_id: 'c', bans: {} }])).toEqual([
      { modelId: 'a', ban },
    ]);
    expect(catalogBans(undefined)).toEqual([]);
  });
  it('shows a provider by its address', () => {
    expect(providerTarget({ OpenAiCompatible: { base_url: 'http://Host/m/v1', extra_headers: {}, key_header: null } })).toEqual([
      'http://Host/m/v1',
    ]);
    expect(providerTarget({ Vertex: { project_id: 'p', location: 'Global', endpoint_url: 'https://e' } })).toEqual(['https://e']);
    expect(providerTarget({ Bedrock: { region: 'us-east-1', endpoint_url: null } })).toEqual(['region: us-east-1']);
    expect(providerTarget('Weird')).toEqual([]);
  });
  it('lists the set fields of a tagged value', () => {
    expect(taggedDetails('ApiKey')).toEqual({ variant: 'ApiKey', fields: [] });
    expect(
      taggedDetails({
        AwsAssumeRole: { role_arn: 'arn:aws:iam::1:role/r', external_id: null, duration_seconds: 900, source: { AccessKey: 'id' } },
      }),
    ).toEqual({
      variant: 'AwsAssumeRole',
      fields: ['role_arn: arn:aws:iam::1:role/r', 'duration_seconds: 900', 'source: {"AccessKey":"id"}'],
    });
  });
  it('describes limits, capabilities, and parameters', () => {
    expect(limitsText({ context_window: 128000, max_output_tokens: 8192 })).toBe('context 128000 · output 8192');
    expect(limitsText({ context_window: null, max_output_tokens: 8 })).toBe('output 8');
    expect(limitsText(null)).toBe('');
    expect(limitsParts({ context_window: 128000, max_output_tokens: 8192 })).toEqual(['context 128000', 'output 8192']);
    expect(limitsParts(undefined)).toEqual([]);
    // a pair as long as a 128000 context window fits a tight column; a 1M window does not
    expect(isLongLimit('context 128000')).toBe(false);
    expect(isLongLimit('output 128000')).toBe(false);
    expect(isLongLimit('context 1000000')).toBe(true);
    expect(capabilityNames({ tool_calling: true, vision: false, thinking: true })).toEqual(['tool_calling', 'thinking']);
    expect(parametersText({ temperature: 0.2, top_p: null, stop_sequences: [], max_output_tokens: 512 })).toBe(
      'temperature 0.2, max_output_tokens 512',
    );
    expect(parametersText(undefined)).toBe('');
    expect(parameterEntries({ temperature: 0.2, top_p: null, stop_sequences: [], flag: true })).toEqual([
      { name: 'temperature', value: '0.2' },
      { name: 'flag', value: 'true' },
    ]);
    expect(parameterEntries(null)).toEqual([]);
  });
  it("keeps a long chunk's trailing separator with the letter before it", () => {
    expect(splitLongNameChunk('observability-')).toEqual(['observabilit', 'y-']);
    expect(splitLongNameChunk('observability_')).toEqual(['observabilit', 'y_']);
    expect(splitLongNameChunk('a-')).toEqual(['', 'a-']);
    for (const chunk of ['observability-', 'observability.', 'longerthanten-']) {
      const [body, tail] = splitLongNameChunk(chunk);
      expect(body + tail).toBe(chunk);
      expect(/^[-_.]/.test(tail)).toBe(false);
    }
  });
  it('splits names into chunks without stranding a short tail', () => {
    expect(nameChunks('jup-mock-a')).toEqual(['jup-mock-a']);
    expect(nameChunks('jup-mock-key')).toEqual(['jup-', 'mock-key']);
    expect(nameChunks('uiloop-profile-banned')).toEqual(['uiloop-', 'profile-', 'banned']);
    expect(nameChunks('jup-mock-endpoint-a')).toEqual(['jup-mock-', 'endpoint-a']);
    expect(nameChunks('uiloop-profile-x-a')).toEqual(['uiloop-', 'profile-', 'x-a']);
    expect(nameChunks('plain')).toEqual(['plain']);
    // short hyphen pieces do not pile up into one wide chunk
    expect(nameChunks('uiloop-profile-banned-by-ratelimit')).toEqual(['uiloop-', 'profile-', 'banned-', 'by-', 'ratelimit']);
    expect(nameChunks('triage-agent-fast-tier')).toEqual(['triage-', 'agent-', 'fast-tier']);
    expect(nameChunks('profile-for-triage-agent-a')).toEqual(['profile-', 'for-', 'triage-', 'agent-a']);
    // a piece of one or two characters still joins its neighbour
    expect(nameChunks('a-b-ratelimit-prod')).toEqual(['a-b-', 'ratelimit-', 'prod']);
    expect(nameChunks('')).toEqual([]);
    // a short tail does not merge into a chunk when the result would be wider than the merge cap
    expect(nameChunks('observability-key')).toEqual(['observability-', 'key']);
    expect(nameChunks('observability-endpoint-a')).toEqual(['observability-', 'endpoint-a']);
    // no chunk of these names is wider than the cap, so no column is made wider than it
    for (const name of ['triage-agent-fast-tier', 'uiloop-profile-banned-by-ratelimit', 'profile-for-triage-agent-a']) {
      expect(Math.max(...nameChunks(name).map((chunk) => chunk.length))).toBeLessThanOrEqual(10);
    }
    // a short tail never makes a chunk wider than the cap: it takes the last piece of the chunk before it instead
    expect(nameChunks('triage-agent-fast-tier-v2')).toEqual(['triage-', 'agent-', 'fast-', 'tier-v2']);
    expect(nameChunks('claude-reasoning-v2')).toEqual(['claude-', 'reasoning-', 'v2']);
    expect(nameChunks('sonnet-reasoning-eu')).toEqual(['sonnet-', 'reasoning-', 'eu']);
    expect(nameChunks('haiku-triage-fast-eu')).toEqual(['haiku-', 'triage-', 'fast-eu']);
    // a lone character still joins a chunk that is a single wide piece
    expect(nameChunks('sonnet-reasoning-a')).toEqual(['sonnet-', 'reasoning-a']);
    for (const name of ['triage-agent-fast-tier-v2', 'claude-reasoning-v2', 'sonnet-reasoning-eu', 'haiku-triage-fast-eu']) {
      expect(Math.max(...nameChunks(name).map((chunk) => chunk.length))).toBeLessThanOrEqual(10);
    }
    // joined, the chunks always give the name back
    for (const name of ['openai-gpt-4o-mini-b', 'triage-agent-fast-tier-v2', 'sonnet-reasoning-a']) {
      expect(nameChunks(name).join('')).toBe(name);
    }
  });
  it('keeps an ordinary word whole and lets only a chunk past the wide-viewport cap break at every width', () => {
    // a word of up to fourteen characters, with or without its separator, fits a column, so it is not split
    for (const chunk of ['observability-', 'summarization', 'production-', 'uiloop-', 'high-effort', 'endpoint']) {
      expect(isLongNameChunk(chunk)).toBe(false);
      expect(isSqueezableNameChunk(chunk)).toBe(false);
    }
    // a chunk breaks at every width only once it passes the wide-viewport cap of 24 characters
    expect(isLongNameChunk('a'.repeat(24))).toBe(false);
    expect(isLongNameChunk('a'.repeat(25))).toBe(true);
    expect(isLongNameChunk(`${'a'.repeat(23)}-`)).toBe(false);
    expect(isLongNameChunk(`${'a'.repeat(24)}-`)).toBe(true);
  });
  it('lets an ordinary long word break only on a narrow viewport', () => {
    // a chunk of 15 to 24 characters stays whole where there is room and breaks as a last resort where there is not
    for (const chunk of ['classification-', 'classification-eu', 'internationalization', 'productionclaude', `${'a'.repeat(23)}-`]) {
      expect(isSqueezableNameChunk(chunk)).toBe(true);
      expect(isLongNameChunk(chunk)).toBe(false);
    }
    // a chunk of fourteen characters or fewer never breaks, and a very long chunk breaks at every width instead
    for (const chunk of ['uiloop-', 'abcdefghij', `${'a'.repeat(13)}-`, `${'a'.repeat(24)}-`]) {
      expect(isSqueezableNameChunk(chunk)).toBe(false);
    }
  });
  it('splits an overlong name chunk so its separator stays with the letter before it and a short tail stays whole', () => {
    expect(splitLongNameChunk(`${'a'.repeat(30)}-`)).toEqual(['a'.repeat(29), 'a-']);
    expect(splitLongNameChunk('word--')).toEqual(['wor', 'd--']);
    expect(splitLongNameChunk('observability-b')).toEqual(['observabilit', 'y-b']);
    expect(splitLongNameChunk('multiregion-')).toEqual(['multiregio', 'n-']);
    expect(splitLongNameChunk('compatible-')).toEqual(['compatibl', 'e-']);
    expect(splitLongNameChunk('summarization-a')).toEqual(['summarizatio', 'n-a']);
    // a chunk without a separator, or with a long piece after its last one, keeps its last few characters together
    expect(splitLongNameChunk('summarizationabc')).toEqual(['summarizatio', 'nabc']);
    expect(splitLongNameChunk('productionclaude')).toEqual(['productioncl', 'aude']);
    expect(splitLongNameChunk('ab')).toEqual(['', 'ab']);
    expect(splitLongNameChunk('')).toEqual(['', '']);
    // joined, the parts always give the chunk back
    for (const chunk of ['observability-', 'a_b.', '---', 'x', 'classification-', 'a-verylongpiecewithoutbreaks']) {
      expect(splitLongNameChunk(chunk).join('')).toBe(chunk);
    }
  });
  it('names the credential an endpoint references', () => {
    const credential = {
      id: 'c1',
      name: 'key',
      metadata: { ModelCredential: { kind: 'ApiKey', secrets: { fingerprint: 'abc123' } } },
    } as unknown as ModelCredential;
    expect(credentialRef('c1', [credential])).toEqual({ name: 'key', kind: 'ApiKey', fingerprint: 'abc123' });
    expect(credentialRef('c2', [credential])).toEqual({ name: 'c2', kind: '', fingerprint: '' });
    expect(credentialRef(null, [credential])).toBeNull();
  });
  it('splits a message on its backtick quotes', () => {
    expect(messageParts('model `gpt-x` is banned: retired')).toEqual([
      { text: 'model ', code: false },
      { text: 'gpt-x', code: true },
      { text: ' is banned: retired', code: false },
    ]);
    expect(messageParts('`a``b`')).toEqual([
      { text: 'a', code: true },
      { text: 'b', code: true },
    ]);
    expect(messageParts('plain `unmatched')).toEqual([{ text: 'plain `unmatched', code: false }]);
    expect(messageParts('')).toEqual([]);
  });
});

describe('kindWords', () => {
  it('splits a ban kind between its words', () => {
    expect(kindWords('ModelRemoved')).toEqual(['Model', 'Removed']);
    expect(kindWords('EndpointUnreachable')).toEqual(['Endpoint', 'Unreachable']);
    expect(kindWords('NotAllowedByDeployment')).toEqual(['Not', 'Allowed', 'By', 'Deployment']);
  });
  it('keeps a one word kind whole', () => {
    expect(kindWords('Generic')).toEqual(['Generic']);
    expect(kindWords('Ban')).toEqual(['Ban']);
  });
  it('gives the kind back when the words are joined', () => {
    expect(kindWords('BrokenReference').join('')).toBe('BrokenReference');
    expect(kindWords('')).toEqual([]);
  });
});

describe('parameterNameChunks', () => {
  it('breaks a parameter name only after an underscore once the chunk is long enough to stand alone', () => {
    expect(parameterNameChunks('max_output_tokens')).toEqual(['max_output_', 'tokens']);
    expect(parameterNameChunks('temperature')).toEqual(['temperature']);
    expect(parameterNameChunks('top_p')).toEqual(['top_p']);
    expect(parameterNameChunks('presence_penalty')).toEqual(['presence_', 'penalty']);
    // a short tail stays with the chunk before it
    expect(parameterNameChunks('max_output_len')).toEqual(['max_output_len']);
    expect(parameterNameChunks('')).toEqual([]);
    // a long name with a short first word still wraps after it, so it cannot force a wide column
    expect(parameterNameChunks('stop_sequences')).toEqual(['stop_', 'sequences']);
    expect(parameterNameChunks('max_tokens')).toEqual(['max_tokens']);
    expect(parameterNameChunks('stop_sequences_list').join('')).toBe('stop_sequences_list');
  });
});
