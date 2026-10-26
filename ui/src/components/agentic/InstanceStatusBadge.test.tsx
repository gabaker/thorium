import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

// project imports
import { InstanceStatusBadge } from './InstanceStatusBadge';
import { ToolSetInstanceView } from './toolsets/ToolSetInstanceView';
import { ToolSetInstance } from '@models/toolsets';

/** Strip styled-component class names and ids so only the tags and text remain. */
const plain = (markup: string): string => markup.replace(/ (class|id)="[^"]*"/g, '');

describe('InstanceStatusBadge', () => {
  it('renders the names in an invalid instance reason as inline code without backticks', () => {
    const markup = plain(renderToStaticMarkup(<InstanceStatusBadge status={{ Invalid: { errors: ['workflow `wf` has no agents'] } }} />));
    expect(markup).toContain('workflow <code>wf</code> has no agents');
    // only the tooltip carries the text, and it has no markdown marks either
    expect(markup).toContain('title="workflow wf has no agents"');
    expect(markup).not.toContain('`');
  });
});

describe('ToolSetInstanceView', () => {
  it('renders the names in an instance warning as inline code without backticks', () => {
    const instance = {
      id: 'i1',
      name: 'set#r1',
      metadata: {
        ToolSetInstance: {
          status: 'Ready',
          triggered_by: 'test',
          digest: 'd',
          warnings: ['toolset `set` has no members, so it gives agents no tools'],
          members: [],
          excluded: [],
        },
      },
    } as unknown as ToolSetInstance;
    const markup = plain(renderToStaticMarkup(<ToolSetInstanceView instance={instance} />));
    expect(markup).toContain('toolset <code>set</code> has no members');
    expect(markup).not.toContain('`');
  });
});
