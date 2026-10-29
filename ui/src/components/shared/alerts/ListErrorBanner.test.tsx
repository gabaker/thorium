import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

// project imports
import ListErrorBanner from './ListErrorBanner';

describe('ListErrorBanner', () => {
  it('renders nothing when no list read failed', () => {
    expect(renderToStaticMarkup(<ListErrorBanner errors={[]} />)).toBe('');
  });

  it('shows each failure message', () => {
    const html = renderToStaticMarkup(<ListErrorBanner errors={['List Pipelines: uiloop boom']} />);
    expect(html).toContain('data-testid="list-error"');
    expect(html).toContain('List Pipelines: uiloop boom');
  });

  it('shows a message repeated by several groups once', () => {
    const html = renderToStaticMarkup(<ListErrorBanner errors={['List Images: boom', 'List Images: boom', 'List Images: other']} />);
    expect(html.match(/data-testid="list-error"/g)).toHaveLength(2);
  });

  it('skips the empty messages of lists that loaded cleanly', () => {
    expect(renderToStaticMarkup(<ListErrorBanner errors={['', '']} />)).toBe('');
    const html = renderToStaticMarkup(<ListErrorBanner errors={['', 'List Agent: down', 'List Tool: down', 'List Agent: down']} />);
    expect(html.match(/data-testid="list-error"/g)).toHaveLength(2);
    expect(html.match(/List Agent: down/g)).toHaveLength(1);
  });

  it('renders backticked names as inline code', () => {
    const html = renderToStaticMarkup(<ListErrorBanner errors={['List Tool: `scanner` is unreadable']} />);
    expect(html.replace(/<code[^>]*>/g, '<code>')).toContain('List Tool: <code>scanner</code> is unreadable');
  });
});
