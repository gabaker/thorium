import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

// project imports
import { ScopedTokenForm } from './ScopedTokenForm';
import { ScopedTokenRole, ScopedTokenRoleKey } from '@models/users';

/** The developer permissions a developer user holds, for tests. */
const held = { k8s: true, bare_metal: false, windows: false, external: false, kvm: false };

/** Render the form with two groups and the given role and grantable permissions. */
const render = (role: ScopedTokenRole, grantable?: typeof held | null): string =>
  renderToStaticMarkup(
    <ScopedTokenForm
      name=""
      onNameChange={() => {}}
      availableGroups={['minerva', 'juno']}
      selectedGroups={[]}
      onGroupToggle={() => {}}
      role={role}
      onRoleChange={() => {}}
      expires=""
      onExpiresChange={() => {}}
      grantable={grantable}
    />,
  );

describe('ScopedTokenForm', () => {
  it('holds each group checkbox inside its label so clicking the label toggles it', () => {
    const html = render(ScopedTokenRoleKey.User);
    for (const group of ['minerva', 'juno']) {
      expect(html).toMatch(new RegExp(`<label[^>]*><input type="checkbox"[^>]*/>${group}</label>`));
    }
  });
  it('holds the Name, Role and Permanent expiry controls inside their labels', () => {
    const html = render(ScopedTokenRoleKey.User);
    // a control inside its label is named by it without an id
    for (const [label, control] of [
      ['Name', 'input'],
      ['Role', 'select'],
      ['Permanent expiry \\(optional\\)', 'input'],
    ]) {
      expect(html).toMatch(new RegExp(`<label[^>]*><span class="label">${label}</span><${control}`));
    }
  });
  it('offers the Developer role when the form is not limited', () => {
    expect(render(ScopedTokenRoleKey.User)).toContain('<option value="Developer">');
  });
  it('leaves the Developer role out for a user the API would refuse it', () => {
    const html = render(ScopedTokenRoleKey.User, null);
    expect(html).not.toContain('<option value="Developer">');
    expect(html).toContain('<option value="User"');
  });
  it('keeps the Developer role for a token that already has it', () => {
    expect(render({ Developer: held }, null)).toContain('<option value="Developer"');
  });
  it('disables the developer permissions the user does not hold', () => {
    const html = render({ Developer: { ...held, k8s: false } }, held);
    const input = (label: string) => new RegExp(`(<input[^>]*>)${label}</label>`).exec(html)?.[1] ?? '';
    // the developer holds only k8s, which the token already has off, so it stays tickable while the rest are disabled
    expect(input('K8s')).toMatch(/^<input type="checkbox"/);
    expect(input('K8s')).not.toContain('disabled');
    expect(input('KVM')).toContain('disabled');
    expect(input('Windows')).toContain('disabled');
  });
});
