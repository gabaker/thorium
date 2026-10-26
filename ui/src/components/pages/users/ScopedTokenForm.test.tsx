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
  it('ties each group label to its checkbox so clicking the label toggles it', () => {
    const html = render(ScopedTokenRoleKey.User);
    for (const group of ['minerva', 'juno']) {
      const id = new RegExp(`<input[^>]*id="([^"]*group-${group})"`).exec(html)?.[1];
      expect(id).toBeDefined();
      expect(html).toContain(`<label title="" for="${id}" class="form-check-label">${group}</label>`);
    }
  });
  it('ties the Name, Role and Permanent expiry labels to their controls', () => {
    const html = render(ScopedTokenRoleKey.User);
    // each label's for attribute must match the id of the control it names
    for (const [label, control] of [
      ['Name', 'input'],
      ['Role', 'select'],
      ['Permanent expiry \\(optional\\)', 'input'],
    ]) {
      const target = new RegExp(`<label[^>]*for="([^"]+)"[^>]*>${label}</label>`).exec(html)?.[1];
      expect(target).toBeDefined();
      expect(html).toMatch(new RegExp(`<${control}[^>]*id="${target}"`));
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
    const input = (permission: string) => new RegExp(`<input[^>]*permission-${permission}[^>]*>`).exec(html)?.[0] ?? '';
    // the developer holds only k8s, which the token already has off, so it stays tickable while the rest are disabled
    expect(input('k8s')).not.toContain('disabled');
    expect(input('kvm')).toContain('disabled');
    expect(input('windows')).toContain('disabled');
  });
});
