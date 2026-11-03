import { useId, useMemo } from 'react';
import styled from 'styled-components';

// project imports
import { CheckList, CheckRow, Field, Muted, Select } from '@components/agentic/shared';
import TextInput from '@components/shared/inputs/TextInput';
import { ScopedTokenRole, ScopedTokenRoleKey, ThoriumDeveloperRoleValue } from '@models/users';

/** The token form's fields, stacked; the fields' own bottom margins space them. */
const FormStack = styled.div`
  display: flex;
  flex-direction: column;
`;

/** A checkbox and its label, where a long group name wraps inside the label instead of overflowing the dialog. */
const WrappingCheck = styled(CheckRow)`
  white-space: normal;
  overflow-wrap: anywhere;
`;

/** The permissions a token gets when its role is switched to Developer: none. */
const defaultDeveloperPermissions: ThoriumDeveloperRoleValue = {
  k8s: false,
  bare_metal: false,
  windows: false,
  external: false,
  kvm: false,
};

/** A developer permission's name. */
type DeveloperPermissionKey = keyof ThoriumDeveloperRoleValue;

/** The props of {@link ScopedTokenForm}. */
type ScopedTokenFormProps = {
  /** The token's name; only a new token has one to fill in. */
  name?: string;
  /** Called with the edited name; the name field is shown only when this is set. */
  onNameChange?: (v: string) => void;
  /** The groups the user can scope the token to. */
  availableGroups: string[];
  /** The groups ticked for the token. */
  selectedGroups: string[];
  /** Called when a group's box is ticked or cleared. */
  onGroupToggle: (group: string) => void;
  /** The token's role. */
  role: ScopedTokenRole;
  /** Called with the edited role. */
  onRoleChange: (role: ScopedTokenRole) => void;
  /** The token's permanent expiry date (YYYY-MM-DD), or empty for none. */
  expires: string;
  /** Called with the edited expiry date. */
  onExpiresChange: (v: string) => void;
  /** Whether to offer clearing the token's current expiry. */
  showClearExpires?: boolean;
  /** Whether the token's current expiry is to be cleared. */
  clearExpires?: boolean;
  /** Called when the clear-expiry box is ticked or cleared. */
  onClearExpiresChange?: (v: boolean) => void;
  /** The developer permissions the user may grant, or null when they may not make a developer token; unset puts no limit on the form. */
  grantable?: ThoriumDeveloperRoleValue | null;
};

/**
 * The fields of the new and edit scoped token dialogs.
 *
 * @param props - The token's fields, their change handlers, and the groups and permissions on offer.
 * @returns The form fields.
 */
export const ScopedTokenForm = ({
  name,
  onNameChange,
  availableGroups,
  selectedGroups,
  onGroupToggle,
  role,
  onRoleChange,
  expires,
  onExpiresChange,
  showClearExpires,
  clearExpires,
  onClearExpiresChange,
  grantable,
}: ScopedTokenFormProps) => {
  // a prefix that keeps the ids of the checkbox groups' headings unique, so each group can be named by its heading
  const idPrefix = useId();
  const sortedGroups = useMemo(() => [...availableGroups].sort(), [availableGroups]);
  const roleKey = role === ScopedTokenRoleKey.User ? ScopedTokenRoleKey.User : ScopedTokenRoleKey.Developer;
  // the roles offered; Developer is left out for a user the API would refuse it, unless the token already has it
  const offeredRoles = Object.values(ScopedTokenRoleKey).filter(
    (candidate) => candidate !== ScopedTokenRoleKey.Developer || grantable !== null || roleKey === ScopedTokenRoleKey.Developer,
  );

  const developerPermissions = [
    { key: 'k8s', label: 'K8s' },
    { key: 'bare_metal', label: 'Bare Metal' },
    { key: 'windows', label: 'Windows' },
    { key: 'external', label: 'External' },
    { key: 'kvm', label: 'KVM' },
  ] as const satisfies readonly {
    key: DeveloperPermissionKey;
    label: string;
  }[];

  const onChangeRoleKey = (roleKey: ScopedTokenRoleKey) => {
    switch (roleKey) {
      case ScopedTokenRoleKey.User:
        onRoleChange(ScopedTokenRoleKey.User);
        break;
      case ScopedTokenRoleKey.Developer:
        onRoleChange({ Developer: { ...defaultDeveloperPermissions } });
        break;
      default:
        console.error('Error: attempted to change role key to unsupported role');
    }
  };

  const updateDeveloperPermission = (key: DeveloperPermissionKey, value: boolean) => {
    if (role === ScopedTokenRoleKey.User) return;
    onRoleChange({
      Developer: {
        ...role.Developer,
        [key]: value,
      },
    });
  };

  return (
    <FormStack>
      {onNameChange !== undefined && (
        <Field>
          <span className="label">Name</span>
          <TextInput type="text" placeholder="my-token" value={name ?? ''} onChange={(e) => onNameChange(e.target.value)} />
          <span className="hint">Scoped token names can contain lower case letters, numbers, and dashes.</span>
        </Field>
      )}
      <Field as="div" role="group" aria-labelledby={`${idPrefix}-groups`}>
        <span className="label" id={`${idPrefix}-groups`}>
          Groups
        </span>
        {availableGroups.length === 0 ? (
          <Muted>You are not a member of any groups.</Muted>
        ) : (
          <CheckList>
            {sortedGroups.map((group) => (
              <WrappingCheck key={group}>
                <input type="checkbox" checked={selectedGroups.includes(group)} onChange={() => onGroupToggle(group)} />
                {group}
              </WrappingCheck>
            ))}
          </CheckList>
        )}
      </Field>
      <Field>
        <span className="label">Role</span>
        <Select value={roleKey} onChange={(e) => onChangeRoleKey(e.target.value as ScopedTokenRoleKey)}>
          {offeredRoles.map((selectedRole) => (
            <option key={selectedRole} value={selectedRole}>
              {selectedRole}
            </option>
          ))}
        </Select>
      </Field>
      {roleKey === ScopedTokenRoleKey.Developer && (
        <Field as="div" role="group" aria-labelledby={`${idPrefix}-permissions`}>
          <span className="label" id={`${idPrefix}-permissions`}>
            Developer permissions
          </span>
          {developerPermissions.map(({ key, label }) => (
            <WrappingCheck key={key}>
              <input
                type="checkbox"
                // a permission the user doesn't hold themselves is refused by the API, so it can't be ticked
                disabled={grantable != null && !grantable[key] && !(role !== ScopedTokenRoleKey.User && role.Developer[key])}
                checked={role !== ScopedTokenRoleKey.User && role.Developer[key]}
                onChange={() => role !== ScopedTokenRoleKey.User && updateDeveloperPermission(key, !role.Developer[key])}
              />
              {label}
            </WrappingCheck>
          ))}
        </Field>
      )}
      <Field>
        <span className="label">Permanent expiry (optional)</span>
        <TextInput type="date" value={expires} disabled={clearExpires} onChange={(e) => onExpiresChange(e.target.value)} />
      </Field>
      {showClearExpires && onClearExpiresChange && (
        <WrappingCheck>
          <input
            type="checkbox"
            checked={clearExpires ?? false}
            onChange={(e) => {
              onClearExpiresChange(e.target.checked);
              if (e.target.checked) onExpiresChange('');
            }}
          />
          Clear permanent expiry
        </WrappingCheck>
      )}
    </FormStack>
  );
};
