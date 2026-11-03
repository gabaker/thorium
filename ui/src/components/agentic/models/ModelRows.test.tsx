import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { ServerStyleSheet } from 'styled-components';
import { describe, expect, it } from 'vitest';

// project imports
import { Bans, CredentialTable, EndpointTable, IdText, Notifications, ProfileTable } from './ModelRows';
import { ModelCatalogEntry } from '@models/modelconfigs';

/** Render a model id through IdText to markup. */
const render = (id: string): string => renderToStaticMarkup(<IdText id={id} />);

/** Strip the tags from rendered markup, leaving its text. */
const text = (markup: string): string => markup.replace(/<[^>]+>/g, '');

describe('IdText', () => {
  it('offers a line break only after each separator', () => {
    const markup = render('us.anthropic.claude-sonnet-4-5-20250929-v1:0');
    expect(text(markup)).toBe('us.anthropic.claude-sonnet-4-5-20250929-v1:0');
    // every break opportunity follows a separator, so no word of the id is split
    const pieces = markup
      .replace(/^<code[^>]*>|<\/code>$/g, '')
      .split('<wbr/>')
      .map((piece) => text(piece))
      .filter((piece) => piece.length > 0);
    expect(pieces).toEqual(['us.', 'anthropic.', 'claude-', 'sonnet-', '4-', '5-', '20250929-', 'v1:0']);
  });
  it('keeps a short id without separators whole', () => {
    const markup = render('98f4255836b1');
    expect(text(markup)).toBe('98f4255836b1');
    // a single piece has nothing to break between, so it carries no break opportunity at all
    expect(markup).not.toContain('<wbr');
    expect(markup).not.toContain('<span');
  });
  it('never offers a break inside a short id with separators', () => {
    const markup = render('stub-model-a');
    expect(text(markup)).toBe('stub-model-a');
    expect(markup).not.toContain('<wbr');
  });
  it('lets an overlong piece break anywhere so it cannot widen its table', () => {
    const long = 'a'.repeat(64);
    const markup = render(`uiloop-${long}`);
    expect(text(markup)).toBe(`uiloop-${long}`);
    expect(markup).toMatch(new RegExp(`<span[^>]*>${long}</span>`));
  });
  it('lets a vendor prefix longer than a short id break in a narrow panel, so the Models column fits at 1024 pixels wide', () => {
    const markup = render('cognitivecomputations/dolphin-mixtral-8x22b');
    expect(text(markup)).toBe('cognitivecomputations/dolphin-mixtral-8x22b');
    expect(markup).toMatch(/<span[^>]*>cognitivecomputation<\/span><span[^>]*>s\/<\/span>/);
    // a prefix no longer than a short id stays whole
    expect(render('NousResearch/Hermes-3-Llama-3.1-405B')).not.toMatch(/<span[^>]*>NousResearch\/<\/span>/);
  });
  it('breaks a piece of 17 to 24 characters anywhere only in a narrow panel, but an overlong piece at every width', () => {
    /** Render an id with its styles and return the rules of the class on the span holding `piece`. */
    const rulesOf = (id: string, piece: string): string => {
      const sheet = new ServerStyleSheet();
      const markup = renderToStaticMarkup(sheet.collectStyles(<IdText id={id} />));
      const css = sheet.getStyleTags();
      sheet.seal();
      const classes = new RegExp(`<span class="([^"]*)">${piece.replace('/', '\\/')}</span>`).exec(markup)?.[1].split(' ') ?? [];
      expect(classes.length).toBeGreaterThan(0);
      // gather every rule of those classes, including the ones nested in a media query
      return classes.map((name) => css.match(new RegExp(`(?:@media[^{]*\\{)?\\.${name}\\{[^}]*\\}`, 'g'))?.join(';') ?? '').join(';');
    };
    // a 22 character prefix keeps its whole width as the column's minimum wherever the panel is wide
    const roomy = rulesOf('cognitivecomputations/dolphin-mixtral-8x22b', 'cognitivecomputation');
    expect(roomy).toContain('overflow-wrap:break-word');
    expect(roomy).toMatch(/@media \(max-width: 1400px\)\{\.\w+\{overflow-wrap:anywhere/);
    // a 25 character piece breaks anywhere at every width
    const overlong = rulesOf('vendor.abcdefghijklmnopqrstuvwxy-model-x', 'abcdefghijklmnopqrstuvwx');
    expect(overlong).toContain('overflow-wrap:anywhere');
    expect(overlong).not.toContain('@media');
    expect(overlong).not.toContain('break-word');
  });
  it('keeps a short tail glued to the piece before it so it cannot wrap after a hyphen', () => {
    const markup = render('amazon.titan-text-lite-v1');
    expect(text(markup)).toBe('amazon.titan-text-lite-v1');
    // the merged final piece sits in its own element, which the browser cannot break inside
    expect(markup).toMatch(/<span[^>]*>lite-v1<\/span>$|<span[^>]*>lite-v1<\/span><\/code>$/);
    expect(markup).not.toContain('>v1<');
    expect(render('openai/gpt-4o-mini-tts')).toMatch(/<span[^>]*>mini-tts<\/span>/);
    expect(render('anthropic.claude-sonnet-4-5-20250929-v1:0')).toMatch(/<span[^>]*>v1:0<\/span>/);
  });
  it('keeps a merged tail on one line in a compact id too', () => {
    const markup = renderToStaticMarkup(<IdText id="amazon.titan-text-express-v1" compact />);
    expect(text(markup)).toBe('amazon.titan-text-express-v1');
    expect(markup).toMatch(/<span[^>]*>express-v1<\/span>/);
  });
  it('lets a compact id break an organisation prefix longer than a badge can hold, but keeps ordinary words whole', () => {
    const markup = renderToStaticMarkup(<IdText id="cognitivecomputations/dolphin-mixtral-8x22b" compact />);
    expect(text(markup)).toBe('cognitivecomputations/dolphin-mixtral-8x22b');
    // the 22 character prefix is a breakable span; the words after it are plain text
    expect(markup).toMatch(/<span[^>]*>cognitivecomputation<\/span><span[^>]*>s\/<\/span>/);
    expect(markup).toContain('dolphin-<wbr/>');
    const anthropic = renderToStaticMarkup(<IdText id="us.anthropic.claude-sonnet-4-5-20250929-v1:0" compact />);
    expect(anthropic).not.toMatch(/<span[^>]*>(us\.|anthropic\.|claude-|sonnet-|20250929-)<\/span>/);
  });
  it('keeps the separator after a breakable piece with the letter before it on one line', () => {
    const markup = renderToStaticMarkup(<IdText id="cognitivecomputations/dolphin-mixtral-8x22b" compact />);
    const bodyClass = /<span class="([^"]*)">cognitivecomputation<\/span>/.exec(markup)?.[1];
    const tailClass = /<span class="([^"]*)">s\/<\/span>/.exec(markup)?.[1];
    expect(bodyClass).toBeDefined();
    expect(tailClass).toBeDefined();
    // the tail is not the breakable style, so the browser cannot start a line with the separator
    expect(tailClass).not.toBe(bodyClass);
  });
  it('breaks a merged final piece that is longer than the limit, compact or not', () => {
    const id = 'vendor.abcdefghijklmnopqrstuvwx-v1';
    const piece = 'abcdefghijklmnopqrstuvwx-v1';
    // a whole merged piece and a broken one are both spans, told apart by their style
    const wholeClass = /<span class="([^"]*)">lite-v1<\/span>/.exec(render('amazon.titan-text-lite-v1'))?.[1];
    expect(wholeClass).toBeDefined();
    for (const markup of [render(id), renderToStaticMarkup(<IdText id={id} compact />)]) {
      const brokenClass = new RegExp(`<span class="([^"]*)">${piece}</span>`).exec(markup)?.[1];
      expect(brokenClass).toBeDefined();
      expect(brokenClass).not.toBe(wholeClass);
    }
  });
  it('does not wrap a final piece that absorbed no tail', () => {
    expect(render('uiloop-extra-model-xyzzy')).not.toContain('<span');
  });
  it('carries the whole id as its title', () => {
    expect(render('uiloop-extra-model')).toContain('title="uiloop-extra-model"');
  });
});

/** A catalog entry whose model is banned by a rate limit. */
const bannedEntry = (modelId: string): ModelCatalogEntry =>
  ({
    model_id: modelId,
    bans: { ban1: { id: 'ban1', msg: 'rate limited', ban_kind: { Generic: {} } } },
  }) as unknown as ModelCatalogEntry;

describe('Bans', () => {
  it('shows a banned catalog model id through IdText so it can wrap', () => {
    const id = 'us.anthropic.claude-sonnet-4-5-20250929-v1:0';
    const markup = renderToStaticMarkup(<Bans bans={{}} catalog={[bannedEntry(id)]} />);
    expect(markup).toContain('data-testid="model-ban-badge"');
    expect(markup).toContain('data-testid="id-text"');
    expect(markup).toContain('<wbr/>');
    expect(text(markup).startsWith(id)).toBe(true);
  });
  it('lets even a short banned model id wrap, so its badge can be as narrow as the word after it', () => {
    const markup = renderToStaticMarkup(<Bans bans={{}} catalog={[bannedEntry('claude-haiku-4-5')]} />);
    // a 16 character id would be kept on one line outside a ban badge
    expect(markup).toContain('<wbr/>');
    expect(text(markup).startsWith('claude-haiku-4-5')).toBe(true);
  });
  it('lets a long vendor prefix of a banned model id break anywhere, so its badge cannot push the table past its panel', () => {
    const markup = renderToStaticMarkup(<Bans bans={{}} catalog={[bannedEntry('togethercomputer/RedPajama-INCITE-7B-Chat')]} />);
    expect(text(markup).startsWith('togethercomputer/RedPajama-INCITE-7B-Chat')).toBe(true);
    // the 17 character prefix would hold the Bans column as wide as itself on top of the Models column doing the same
    expect(markup).toMatch(/<span[^>]*>togethercompute<\/span><span[^>]*>r\/<\/span>/);
    // the words after it are no longer than a word of a common id, so they stay whole
    expect(markup).not.toMatch(/<span[^>]*>RedPajama-<\/span>/);
  });
  it('keeps every word of a banned model id whole, breaking it only after a separator', () => {
    const markup = renderToStaticMarkup(<Bans bans={{}} catalog={[bannedEntry('us.anthropic.claude-sonnet-4-5-20250929-v1:0')]} />);
    // besides the badge, only the final piece that absorbed a short tail is a span, and it keeps that piece on one line
    expect(markup.match(/<span/g)).toHaveLength(2);
    expect(markup).toMatch(/<span class="[^"]*">v1:0<\/span>/);
    const pieces = markup
      .replace(/^.*?<code[^>]*>|<\/code>.*$/g, '')
      .split('<wbr/>')
      .map((piece) => piece.replace(/<[^>]*>/g, ''))
      .filter((piece) => piece.length > 0);
    expect(pieces).toEqual(['us.', 'anthropic.', 'claude-', 'sonnet-', '4-', '5-', '20250929-', 'v1:0']);
  });
  it('wraps a banned model id after its separators, not mid-word, with a word that starts no line with a separator', () => {
    const markup = renderToStaticMarkup(<Bans bans={{}} catalog={[bannedEntry('cohere.command-r-plus-v1')]} />);
    const pieces = markup
      .replace(/^.*?<code[^>]*>|<\/code>.*$/g, '')
      .split('<wbr/>')
      .map((piece) => piece.replace(/<[^>]*>/g, ''))
      .filter((piece) => piece.length > 0);
    // each piece ends with its separator, so no line begins with one
    expect(pieces.slice(0, -1).every((piece) => /[.:/_@-]$/.test(piece))).toBe(true);
    expect(pieces.every((piece) => /^[a-z0-9]/.test(piece))).toBe(true);
  });
  it('lets an overlong banned model id break anywhere', () => {
    const long = 'f'.repeat(64);
    const markup = renderToStaticMarkup(<Bans bans={{}} catalog={[bannedEntry(long)]} />);
    expect(markup).toMatch(new RegExp(`<span[^>]*>${long}</span>`));
  });
});

describe('Bans kinds', () => {
  const removed = {
    ...bannedEntry('claude-haiku-4-5'),
    bans: { ban2: { id: 'ban2', msg: 'gone', ban_kind: { ModelRemoved: {} } } },
  } as unknown as ModelCatalogEntry;
  it('lets the kind of a catalog model ban wrap between its words', () => {
    const markup = renderToStaticMarkup(<Bans bans={{}} catalog={[removed]} />);
    expect(markup).toContain('Model<wbr/>Removed');
    expect(text(markup)).toBe('claude-haiku-4-5: ModelRemoved');
  });
  it('lets the kind of an endpoint ban wrap between its words', () => {
    const bans = { b1: { id: 'b1', msg: 'down', ban_kind: { EndpointUnreachable: {} } } };
    const markup = renderToStaticMarkup(<Bans bans={bans as never} />);
    expect(markup).toContain('data-testid="ban-badge"');
    expect(markup).toContain('Endpoint<wbr/>Unreachable');
    expect(text(markup)).toBe('EndpointUnreachable');
  });
  it('keeps a one word kind whole', () => {
    const markup = renderToStaticMarkup(<Bans bans={{}} catalog={[bannedEntry('claude-haiku-4-5')]} />);
    expect(text(markup)).toBe('claude-haiku-4-5: Generic');
    expect(markup).not.toContain('Gener<wbr');
  });
});

describe('Notifications', () => {
  it('renders each message in an element that may break anywhere', () => {
    const items = [{ id: 'n1', level: 'Error', msg: `model ${'f'.repeat(64)} is banned` }];
    const markup = renderToStaticMarkup(
      <table>
        <tbody>
          <Notifications items={items as never} columns={8} testId="notes" />
        </tbody>
      </table>,
    );
    expect(markup).toContain('data-testid="model-notification"');
    expect(markup).toContain('f'.repeat(64));
  });
});

/** A profile with the given name, set on a missing endpoint, as the profile table lists it. */
const profileNamed = (name: string) =>
  ({
    id: `id-${name}`,
    name,
    groups: ['g'],
    metadata: { ModelProfile: { endpoint: 'missing', model_id: 'm', api: 'OpenAiChat', parameters: { max_output_tokens: 512 }, bans: {} } },
  }) as never;

describe('ProfileTable', () => {
  it('keeps a short name whole and wraps a long one only between whole chunks', () => {
    const markup = renderToStaticMarkup(
      <ProfileTable items={[profileNamed('jup-mock-a'), profileNamed('uiloop-profile-banned')]} endpoints={[]} notifications={{}} />,
    );
    // a short name is one nowrap chunk with no break opportunity
    expect(markup).toMatch(/<span class="[^"]*">jup-mock-a<\/span>/);
    // a long name offers a break only after its first chunks, so no chunk is split or stranded
    expect(markup).toMatch(
      /<span class="[^"]*">uiloop-<\/span><wbr\/><span class="[^"]*">profile-<\/span><wbr\/><span class="[^"]*">banned<\/span>/,
    );
  });
  it('lets the head of a name chunk longer than a column can hold break mid-word while its tail stays whole', () => {
    for (const [name, head, tail] of [
      ['uiloopnoseparatorinthisnam', 'uiloopnoseparatorinthi', 'snam'],
      ['uiloopnoseparatorinthisname1', 'uiloopnoseparatorinthisn', 'ame1'],
    ]) {
      const markup = renderToStaticMarkup(
        <ProfileTable items={[profileNamed('jup-mock-a'), profileNamed(name)]} endpoints={[]} notifications={{}} />,
      );
      const shortClass = /<span class="([^"]*)">jup-mock-a<\/span>/.exec(markup)?.[1];
      const headClass = new RegExp(`<span class="([^"]*)">${head}</span>`).exec(markup)?.[1];
      const tailClass = new RegExp(`<span class="([^"]*)">${tail}</span>`).exec(markup)?.[1];
      // the head breaks anywhere, so it is styled differently from a whole (nowrap) chunk
      expect(shortClass).toBeDefined();
      expect(headClass).toBeDefined();
      expect(headClass).not.toBe(shortClass);
      // the tail is a whole chunk, so a break never strands part of it
      expect(tailClass).toBe(shortClass);
    }
  });
  it('keeps the last letter, separator and short tail of an ordinary long word on one whole line', () => {
    const markup = renderToStaticMarkup(
      <ProfileTable
        items={[profileNamed('jup-mock-a'), profileNamed('classification-eu'), profileNamed('observability-b')]}
        endpoints={[]}
        notifications={{}}
      />,
    );
    const wholeClass = /<span class="([^"]*)">jup-mock-a<\/span>/.exec(markup)?.[1];
    const headClass = /<span class="([^"]*)">classificatio<\/span>/.exec(markup)?.[1];
    expect(wholeClass).toBeDefined();
    // the head is styled apart from a whole chunk, so a narrow page can break it as a last resort
    expect(headClass).toBeDefined();
    expect(headClass).not.toBe(wholeClass);
    // the separator and the last letter before it are one nowrap span, so no line starts with a separator
    expect(markup).toContain(`<span class="${wholeClass}">n-</span><wbr/><span class="${wholeClass}">eu</span>`);
    // the lone character after the separator stays with the separator and its letter
    expect(markup).toContain(`<span class="${headClass}">observabilit</span><span class="${wholeClass}">y-b</span>`);
  });
  it('styles the head of an ordinary long word apart from the head of a very long chunk', () => {
    const markup = renderToStaticMarkup(
      <ProfileTable
        items={[profileNamed('classification-endpoint'), profileNamed(`${'a'.repeat(30)}-more`)]}
        endpoints={[]}
        notifications={{}}
      />,
    );
    const ordinary = /<span class="([^"]*)">classificatio<\/span>/.exec(markup)?.[1];
    const veryLong = new RegExp(`<span class="([^"]*)">${'a'.repeat(29)}</span>`).exec(markup)?.[1];
    // the first breaks only on a narrow viewport and the second at every width, so their styles differ
    expect(ordinary).toBeDefined();
    expect(veryLong).toBeDefined();
    expect(ordinary).not.toBe(veryLong);
  });
  it('keeps the words of a name that are fourteen characters or fewer whole', () => {
    const markup = renderToStaticMarkup(
      <ProfileTable
        items={[profileNamed('uiloop-production-claude-sonnet-high-effort'), profileNamed('summarization')]}
        endpoints={[]}
        notifications={{}}
      />,
    );
    // every word, including the eleven character `production-` and the separator-free `summarization`, is one whole chunk
    for (const word of ['uiloop-', 'production-', 'claude-', 'sonnet-', 'high-', 'effort', 'summarization']) {
      expect(markup).toMatch(new RegExp(`<span class="[^"]*">${word}</span>`));
    }
  });
  it('splits a name of a very long chunk at every width and keeps its separator with the character before it', () => {
    const long = `${'a'.repeat(30)}-`;
    const markup = renderToStaticMarkup(<ProfileTable items={[profileNamed(`${long}more`)]} endpoints={[]} notifications={{}} />);
    expect(markup).toMatch(new RegExp(`<span class="[^"]*">${'a'.repeat(29)}</span><span class="[^"]*">a-</span>`));
  });
  it('keeps the lone character after a separator out of the part of a very long chunk that may break', () => {
    const head = 'observabilityendpointsandcredentials';
    const markup = renderToStaticMarkup(<ProfileTable items={[profileNamed(`${head}-b`)]} endpoints={[]} notifications={{}} />);
    const body = head.slice(0, -1);
    const longClass = new RegExp(`<span class="([^"]*)">${body}</span>`).exec(markup)?.[1];
    // the body breaks anywhere while the separator, the letter before it and the lone character form one nowrap span
    expect(longClass).toBeDefined();
    expect(markup).toMatch(new RegExp(`<span class="[^"]*">${body}</span><span class="(?!${longClass})[^"]*">s-b</span>`));
  });
  it('keeps each limit with its value on a line of its own with no separator', () => {
    const profile = {
      id: 'id-p',
      name: 'p',
      groups: ['g'],
      metadata: {
        ModelProfile: {
          endpoint: 'missing',
          model_id: 'm',
          api: 'OpenAiChat',
          limits: { context_window: 128000, max_output_tokens: 8192 },
          bans: {},
        },
      },
    } as never;
    const markup = renderToStaticMarkup(<ProfileTable items={[profile]} endpoints={[]} notifications={{}} />);
    // a stacked list has no separator, so the column is no wider than its widest limit
    expect(markup).toMatch(/<span class="[^"]*">context 128000<\/span><span class="[^"]*">output 8192<\/span>/);
    expect(markup).not.toContain('context 128000 ·');
  });
  it('lets only a stacked limit too long to fit break between its label and its value, so a 1M context window cannot widen the table', () => {
    const profile = {
      id: 'id-big',
      name: 'big',
      groups: ['g'],
      metadata: {
        ModelProfile: {
          endpoint: 'missing',
          model_id: 'm',
          api: 'OpenAiChat',
          limits: { context_window: 1000000, max_output_tokens: 128000 },
          bans: {},
        },
      },
    } as never;
    const sheet = new ServerStyleSheet();
    const markup = renderToStaticMarkup(sheet.collectStyles(<ProfileTable items={[profile]} endpoints={[]} notifications={{}} />));
    const css = sheet.getStyleTags();
    sheet.seal();
    // every class on the limit's span contributes rules, so gather them all (a styled extension carries its base's class)
    const rulesOf = (text: string): string => {
      const classes = new RegExp(`<span class="([^"]*)">${text}</span>`).exec(markup)?.[1].split(' ') ?? [];
      expect(classes.length).toBeGreaterThan(0);
      return classes.map((name) => new RegExp(`\\.${name}\\{([^}]*)\\}`).exec(css)?.[1] ?? '').join(';');
    };
    // the long limit is a block that is free to wrap at its space, so none of its classes sets nowrap
    expect(rulesOf('context 1000000')).toContain('display:block');
    expect(rulesOf('context 1000000')).not.toContain('nowrap');
    // a limit that fits stays on one line, so the table keeps its width for the limits
    expect(rulesOf('output 128000')).toContain('display:block');
    expect(rulesOf('output 128000')).toContain('nowrap');
  });
  it('keeps each capability whole and leaves the space between names outside the nowrap span', () => {
    const profile = {
      id: 'id-c',
      name: 'c',
      groups: ['g'],
      metadata: {
        ModelProfile: {
          endpoint: 'missing',
          model_id: 'm',
          api: 'OpenAiChat',
          capabilities: { tool_calling: true, vision: true, thinking: true },
          bans: {},
        },
      },
    } as never;
    const markup = renderToStaticMarkup(<ProfileTable items={[profile]} endpoints={[]} notifications={{}} />);
    // the separating space follows each nowrap span, so the list can wrap between whole names
    expect(markup).toMatch(
      /<span class="[^"]*">tool_calling,<\/span> <span class="[^"]*">vision,<\/span> <span class="[^"]*">thinking<\/span>/,
    );
  });
  it('offers a line break in a parameter name only after a chunk long enough to stand alone', () => {
    const markup = renderToStaticMarkup(<ProfileTable items={[profileNamed('p')]} endpoints={[]} notifications={{}} />);
    // `max_` is too short to end a line, so the first break comes after `max_output_`
    expect(markup).toMatch(
      /<span class="[^"]*">max_output_<\/span><wbr\/><span class="[^"]*">tokens<\/span> <span class="[^"]*">512<\/span>/,
    );
    expect(markup).not.toContain('max_<wbr/>');
  });
});

const endpointWithCredential = {
  id: 'id-ep',
  name: 'ep',
  groups: ['g'],
  metadata: { ModelEndpoint: { provider: { OpenAi: { base_url: 'https://x' } }, credential: 'cred-1', catalog: [], bans: {} } },
} as never;

describe('EndpointTable catalog limits', () => {
  it('lists each limit of a catalog model on its own line with no separator', () => {
    const endpoint = {
      id: 'id-ep',
      name: 'ep',
      groups: ['g'],
      metadata: {
        ModelEndpoint: {
          provider: { OpenAi: { base_url: 'https://x' } },
          credential: 'cred-1',
          catalog: [{ model_id: 'm', limits: { context_window: 128000, max_output_tokens: 8192 }, bans: {} }],
          bans: {},
        },
      },
    } as never;
    const markup = renderToStaticMarkup(<EndpointTable items={[endpoint]} credentials={[]} notifications={{}} />);
    expect(markup).toMatch(/<span class="[^"]*">context 128000<\/span><span class="[^"]*">output 8192<\/span>/);
    expect(markup).not.toContain('context 128000 ·');
  });
});

describe('EndpointTable provider', () => {
  it('lets the provider name wrap between its words so it does not hold its column wide', () => {
    const endpoint = {
      id: 'id-ep',
      name: 'ep',
      groups: ['g'],
      metadata: {
        ModelEndpoint: {
          provider: { OpenAiCompatible: { base_url: 'https://x' } },
          credential: null,
          catalog: [],
          bans: {},
        },
      },
    } as never;
    const markup = renderToStaticMarkup(<EndpointTable items={[endpoint]} credentials={[]} notifications={{}} />);
    expect(markup).toContain('Open<wbr/>Ai<wbr/>Compatible');
  });
});

describe('EndpointTable credential reference', () => {
  it('says an unresolved credential is not visible when the credentials loaded', () => {
    const markup = renderToStaticMarkup(<EndpointTable items={[endpointWithCredential]} credentials={[]} notifications={{}} />);
    expect(markup).toContain('not visible to you');
  });
  it('does not call an unresolved credential hidden when the credentials failed to load', () => {
    const markup = renderToStaticMarkup(
      <EndpointTable items={[endpointWithCredential]} credentials={[]} credentialsFailed notifications={{}} />,
    );
    expect(markup).not.toContain('not visible to you');
    expect(markup).toContain('details unavailable');
    expect(markup).toContain('cred-1');
  });
});

describe('EndpointTable credential reference while loading', () => {
  it('says loading, not hidden, while the credentials are still loading', () => {
    const markup = renderToStaticMarkup(
      <EndpointTable items={[endpointWithCredential]} credentials={[]} credentialsLoading notifications={{}} />,
    );
    expect(markup).not.toContain('not visible to you');
    expect(markup).toContain('loading');
    expect(markup).toContain('cred-1');
  });
});

describe('model tables when a list fails to load', () => {
  it('say nothing instead of "none" for an empty list that failed', () => {
    const props = { items: [], notifications: {}, failed: true };
    expect(renderToStaticMarkup(<EndpointTable {...props} credentials={[]} />)).toBe('');
    expect(renderToStaticMarkup(<ProfileTable {...props} endpoints={[]} />)).toBe('');
    expect(renderToStaticMarkup(<CredentialTable {...props} />)).toBe('');
  });
  it('still say there are none for an empty list that loaded', () => {
    const props = { items: [], notifications: {} };
    expect(renderToStaticMarkup(<EndpointTable {...props} credentials={[]} />)).toContain('No model endpoints');
    expect(renderToStaticMarkup(<ProfileTable {...props} endpoints={[]} />)).toContain('No model profiles');
    expect(renderToStaticMarkup(<CredentialTable {...props} />)).toContain('No model credentials');
  });
});
