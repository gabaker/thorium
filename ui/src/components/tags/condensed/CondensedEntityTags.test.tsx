import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { MemoryRouter } from 'react-router';
import { describe, expect, it } from 'vitest';

// project imports
import CondensedEntityTags from './CondensedEntityTags';
import { Entities } from '@models/entities';
import { Tags } from '@models/tags';

/** Tags like those a repo carries after a tool and a workflow stored results on it. */
const REPO_TAGS: Tags = {
  kind: { cli: ['demo'] },
  Results: { 'cli-tool': ['demo'], 'wf#r1~a8014fd1-e50c-4c71-ac0e-aad8fd3f7f99': ['demo'] },
};

/** Render the tag list for a repo with the given excluded keys. */
const render = (tags: Tags, excludeKeys?: string[]): string =>
  renderToStaticMarkup(
    <MemoryRouter>
      <CondensedEntityTags tags={tags} resource={Entities.Repo} excludeKeys={excludeKeys} />
    </MemoryRouter>,
  );

describe('CondensedEntityTags excludeKeys', () => {
  it('shows every tag when nothing is excluded', () => {
    const html = render(REPO_TAGS);
    expect(html).toContain('Results: cli-tool');
    expect(html).toContain('kind: cli');
  });
  it('leaves out excluded keys without regard to case and keeps the rest', () => {
    const html = render(REPO_TAGS, ['results']);
    expect(html).not.toContain('Results');
    expect(html).not.toContain('a8014fd1');
    expect(html).toContain('kind: cli');
  });
  it('reads as empty when only excluded tags remain', () => {
    const html = render({ Results: REPO_TAGS.Results }, ['Results']);
    expect(html).toContain('No Tags Found');
  });
  it('uses the sentence-case empty copy of the repo panels when the banner spans the panel', () => {
    const html = renderToStaticMarkup(
      <MemoryRouter>
        <CondensedEntityTags tags={{}} resource={Entities.Repo} fullWidthEmpty />
      </MemoryRouter>,
    );
    expect(html).toContain('No tags found.');
  });
});
