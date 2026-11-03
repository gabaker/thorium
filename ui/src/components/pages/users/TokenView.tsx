import { useMemo, useState, useEffect } from 'react';
import { useNavigate } from 'react-router';
import styled from 'styled-components';
import { FaClipboard, FaCheck } from 'react-icons/fa6';
import { FaEye, FaEyeSlash } from 'react-icons/fa';
import { MdDelete } from 'react-icons/md';
import { toast } from 'react-toastify';

// project imports
import {
  ActionsCell,
  ActionsHeader,
  CellNote,
  GroupsCell,
  MaskedToken,
  NameCell,
  NoWrapCell,
  PanelDescription,
  PanelHead,
  PanelBanner,
  PanelHeading,
  PanelNotice,
  PanelSubsection,
  RowActions,
  ScopedPanel,
  TableFrame,
  TokenTable,
  TokenValue,
} from './ScopedTokens.styled';
import RunTokenTable, { TokenGroups } from './RunTokenTable';
import { ScopedTokenForm } from './ScopedTokenForm';
import { formatExpire, grantableDeveloperPermissions, splitScopedTokens } from './scopedTokens';
import { ScopedTokenList } from './useScopedTokens';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Button, ButtonSize, ButtonVariant, IconButton } from '@components/shared/buttons';
import Modal from '@components/shared/modal/Modal';
import { OverlayTipTop } from '@components/shared/overlay/tips';
import Time from '@components/shared/Time';
import Subtitle from '@components/shared/titles/Subtitle';
import { useAuth } from '@utilities/auth';
import { createScopedToken, deleteScopedToken, updateScopedToken } from '@thorpi/users';
import { BUTTON_BADGE_GAP, spacers } from '@styles';
import { ScopedToken, ScopedTokenRole, ScopedTokenRoleKey, ScopedTokenUpdate, UserInfo } from '@models/users';

// ─── Root token ───────────────────────────────────────────────────────────────

/** Asks before revoking the user's root token, which logs them out. */
const RevokeTokenModal = ({ show, onHide }: { show: boolean; onHide: () => void }) => {
  const { revoke } = useAuth();
  const navigate = useNavigate();
  const handleRevoke = () => {
    void revoke().then(() => {
      void navigate('/');
    });
  };
  return (
    <Modal
      show={show}
      onClose={onHide}
      title="Revoke Your Token?"
      actions={
        <Button variant={ButtonVariant.Danger} onClick={() => handleRevoke()}>
          Confirm
        </Button>
      }
    >
      Revoking your token will automatically log you out of this page and any currently running or queued analysis jobs (reactions) may
      fail. Are you sure?
    </Modal>
  );
};

// Token value with the inline show/hide and revoke controls at the end; the token text takes the
// available width and wraps, keeping the controls at the end of the row.
const TokenLine = styled.div`
  display: flex;
  align-items: center;
  gap: ${BUTTON_BADGE_GAP};
`;

/** The root token's value, or its mask while hidden; a long token breaks anywhere rather than widening the page. */
const TokenText = styled.span`
  flex: 1;
  min-width: 0;
  overflow-wrap: anywhere;
`;

/** The root token's labels in a narrow first column and its value and expiry beside them. */
const RootTokenGrid = styled.div`
  display: grid;
  grid-template-columns: minmax(5rem, 1fr) 5fr;
  align-items: center;
  gap: ${spacers.four} ${spacers.three};
`;

/** The groups a scoped token would limit the user to, scrolled when the list is long. */
const ScopeGroups = styled.ul`
  max-height: 12rem;
  overflow: auto;
  margin: 0;
  padding-left: ${spacers.five};
`;

// Red trash control (next to the show/hide eye) that opens the revoke-token confirmation.
const RevokeTokenButton = styled.button`
  display: flex;
  align-items: center;
  justify-content: center;
  width: 30px;
  height: 30px;
  padding: 0;
  border: none;
  background: none;
  color: var(--thorium-danger-bg);
  cursor: pointer;

  &:hover:not(:disabled) {
    filter: brightness(1.15);
  }
`;

/** The user's root token, hidden until shown, with its expiry and the revoke control. */
const RootToken = () => {
  const [showRevokeModal, setShowRevokeModal] = useState(false);
  const [tokenShowing, setTokenShowing] = useState(false);
  const { userInfo, clearScopedToken, scopedToken } = useAuth();

  return (
    <RootTokenGrid>
      <Subtitle>Token</Subtitle>
      <TokenLine>
        <TokenText>{tokenShowing ? userInfo?.token : '****************************************************************'}</TokenText>
        {scopedToken && (
          <Button variant={ButtonVariant.Secondary} size={ButtonSize.Small} onClick={() => clearScopedToken()}>
            Use Root Token
          </Button>
        )}
        {/* eye toggle reveals/masks the token in place */}
        <OverlayTipTop tip={tokenShowing ? 'Hide' : 'Show'}>
          <IconButton
            variant={ButtonVariant.Icon}
            size={ButtonSize.Small}
            onClick={() => setTokenShowing((prev) => !prev)}
            aria-label={tokenShowing ? 'Hide token' : 'Show token'}
          >
            {tokenShowing ? <FaEyeSlash size={18} /> : <FaEye size={18} />}
          </IconButton>
        </OverlayTipTop>
        {/* red trash opens the revoke-token confirmation */}
        <OverlayTipTop tip={scopedToken !== undefined ? 'Disabled Scoped Token to revoke root token' : 'Revoke Token'}>
          <RevokeTokenButton
            type="button"
            onClick={() => setShowRevokeModal(true)}
            aria-label="Revoke Token"
            disabled={scopedToken !== undefined}
          >
            <MdDelete size={22} />
          </RevokeTokenButton>
        </OverlayTipTop>
      </TokenLine>
      <Subtitle>Expiry</Subtitle>
      <span>{userInfo?.token_expiration ? <Time>{userInfo.token_expiration}</Time> : null}</span>
      <RevokeTokenModal show={showRevokeModal} onHide={() => setShowRevokeModal(false)} />
    </RootTokenGrid>
  );
};

// ─── Create modal ─────────────────────────────────────────────────────────────

/** The props of {@link CreateTokenModal}. */
type CreateModalProps = {
  show: boolean;
  availableGroups: string[];
  onHide: () => void;
  onCreated: () => void;
};

/** The dialog that creates a scoped token. */
const CreateTokenModal = ({ show, availableGroups, onHide, onCreated }: CreateModalProps) => {
  const [name, setName] = useState('');
  const [selectedGroups, setSelectedGroups] = useState<string[]>([]);
  const [role, setRole] = useState<ScopedTokenRole>(ScopedTokenRoleKey.User);
  const [expires, setExpires] = useState('');
  const [error, setError] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const { userInfo } = useAuth();

  const reset = () => {
    setName('');
    setSelectedGroups([]);
    setExpires('');
    setError('');
    setSubmitting(false);
    setRole(ScopedTokenRoleKey.User);
  };

  const handleHide = () => {
    reset();
    onHide();
  };

  const toggleGroup = (group: string) => {
    setSelectedGroups((prev) => (prev.includes(group) ? prev.filter((g) => g !== group) : [...prev, group]));
  };

  const handleSubmit = async () => {
    setError('');
    if (!name.trim()) {
      setError('Name is required.');
      return;
    }
    if (selectedGroups.length === 0) {
      setError('At least one group must be selected.');
      return;
    }
    setSubmitting(true);
    const expiresIso = expires ? `${expires}T00:00:00Z` : undefined;
    const result = await createScopedToken(
      {
        role: role,
        name: name.trim(),
        groups: selectedGroups,
        expires: expiresIso,
      },
      (msg) => setError(msg),
    );
    setSubmitting(false);
    if (result) {
      reset();
      onCreated();
    }
  };

  return (
    <Modal
      show={show}
      onClose={handleHide}
      title="New scoped token"
      actions={
        <>
          <Button variant={ButtonVariant.Secondary} onClick={handleHide}>
            Cancel
          </Button>
          <Button onClick={() => void handleSubmit()} disabled={submitting}>
            {submitting ? 'Creating…' : 'Create'}
          </Button>
        </>
      }
    >
      {error && <AlertBanner>{error}</AlertBanner>}
      <ScopedTokenForm
        name={name}
        onNameChange={setName}
        availableGroups={availableGroups}
        selectedGroups={selectedGroups}
        role={role}
        onRoleChange={setRole}
        onGroupToggle={toggleGroup}
        expires={expires}
        onExpiresChange={setExpires}
        grantable={grantableDeveloperPermissions(userInfo?.role)}
      />
    </Modal>
  );
};

// ─── Delete modal ─────────────────────────────────────────────────────────────

/** The props of {@link DeleteTokenModal}. */
type DeleteModalProps = {
  token: ScopedToken | null;
  onHide: () => void;
  onDeleted: () => void;
};

/** Asks before deleting a scoped token; open while a token is given. */
const DeleteTokenModal = ({ token, onHide, onDeleted }: DeleteModalProps) => {
  const [error, setError] = useState('');
  const [submitting, setSubmitting] = useState(false);

  const handleHide = () => {
    setError('');
    setSubmitting(false);
    onHide();
  };

  const handleDelete = async () => {
    if (!token) return;
    setError('');
    setSubmitting(true);
    const ok = await deleteScopedToken(token.name, (msg) => setError(msg));
    setSubmitting(false);
    if (ok) {
      handleHide();
      onDeleted();
    }
  };

  return (
    <Modal
      show={!!token}
      onClose={handleHide}
      title="Delete Scoped Token?"
      actions={
        <>
          <Button variant={ButtonVariant.Secondary} onClick={handleHide}>
            Cancel
          </Button>
          <Button variant={ButtonVariant.Danger} onClick={() => void handleDelete()} disabled={submitting}>
            {submitting ? 'Deleting…' : 'Delete'}
          </Button>
        </>
      }
    >
      {error && <AlertBanner>{error}</AlertBanner>}
      {token && (
        <p>
          Delete <strong>{token.name}</strong>? This cannot be undone.
        </p>
      )}
    </Modal>
  );
};

// ─── Edit modal ───────────────────────────────────────────────────────────────

/** The props of {@link EditTokenModal}. */
type EditModalProps = {
  token: ScopedToken | null;
  availableGroups: string[];
  onHide: () => void;
  onUpdated: () => void;
};

/** The dialog that changes a scoped token's groups, role, and expiry; open while a token is given. */
const EditTokenModal = ({ token, availableGroups, onHide, onUpdated }: EditModalProps) => {
  const [selectedGroups, setSelectedGroups] = useState<string[]>([]);
  const [expires, setExpires] = useState('');
  const [role, setRole] = useState<ScopedTokenRole>(token ? token.role : ScopedTokenRoleKey.User);
  const [clearExpires, setClearExpires] = useState(false);
  const [error, setError] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const { userInfo } = useAuth();

  useEffect(() => {
    // the form starts from the chosen token's values each time a token is chosen
    if (token) {
      setSelectedGroups(token.groups);
      // token.expires is a full ISO string — take YYYY-MM-DD prefix
      setExpires(token.expires ? token.expires.slice(0, 10) : '');
      setClearExpires(false);
      setError('');
      setSubmitting(false);
      setRole(token.role);
    }
  }, [token]);

  const handleHide = () => {
    setError('');
    setSubmitting(false);
    onHide();
  };

  const toggleGroup = (group: string) => {
    setSelectedGroups((prev) => (prev.includes(group) ? prev.filter((g) => g !== group) : [...prev, group]));
  };

  const handleSubmit = async () => {
    if (!token) return;
    if (selectedGroups.length === 0) {
      setError('At least one group must be selected.');
      return;
    }
    setError('');
    setSubmitting(true);

    const original = new Set(token.groups);
    const updated = new Set(selectedGroups);
    const add_groups = [...updated].filter((g) => !original.has(g));
    const remove_groups = [...original].filter((g) => !updated.has(g));

    const update: ScopedTokenUpdate = { role: role, add_groups, remove_groups, clear_expires: clearExpires };
    if (expires && !clearExpires) {
      update.expires = `${expires}T00:00:00Z`;
    }

    const result = await updateScopedToken(token.name, update, (msg) => setError(msg));
    setSubmitting(false);
    if (result) {
      handleHide();
      onUpdated();
    }
  };

  return (
    <Modal
      show={!!token}
      onClose={handleHide}
      title={`Edit Scoped Token${token ? ` — ${token.name}` : ''}`}
      actions={
        <>
          <Button variant={ButtonVariant.Secondary} onClick={handleHide}>
            Cancel
          </Button>
          <Button onClick={() => void handleSubmit()} disabled={submitting}>
            {submitting ? 'Saving…' : 'Save'}
          </Button>
        </>
      }
    >
      {error && <AlertBanner>{error}</AlertBanner>}
      <ScopedTokenForm
        availableGroups={availableGroups}
        selectedGroups={selectedGroups}
        onGroupToggle={toggleGroup}
        role={role}
        onRoleChange={setRole}
        expires={expires}
        onExpiresChange={setExpires}
        showClearExpires={!!token?.expires}
        clearExpires={clearExpires}
        onClearExpiresChange={(v) => setClearExpires(v)}
        grantable={grantableDeveloperPermissions(userInfo?.role)}
      />
    </Modal>
  );
};

/** The props of {@link SelectModal}. */
type SelectModalProps = {
  token: ScopedToken | null;
  onHide: () => void;
};

/** Asks before this tab starts using a scoped token; open while a token is given. */
const SelectModal = ({ token, onHide }: SelectModalProps) => {
  const { setScopedToken } = useAuth();

  const handleSubmit = () => {
    if (!token) return;
    setScopedToken(token);
    onHide();
  };

  const groups = [...(token?.groups ?? [])].sort().map((g) => {
    return <li key={g}>{g}</li>;
  });

  return (
    <Modal
      show={!!token}
      onClose={onHide}
      title={`Activate Scoped Token${token ? ` — ${token.name}` : ''}`}
      actions={
        <>
          <Button variant={ButtonVariant.Secondary} onClick={onHide}>
            Cancel
          </Button>
          <Button onClick={() => handleSubmit()}>Change Token</Button>
        </>
      }
    >
      <p>Select &quot;{token?.name}&quot; as token? This will limit visibility to the following groups:</p>
      <ScopeGroups>{groups}</ScopeGroups>
      <p>Several features of Thorium UI may be unusable with a scoped token. To re-enable full functionality, disable the scoped token.</p>
    </Modal>
  );
};

/** One scoped token's table row: its value, role, groups, expiries, and actions. */
const TokenRow = ({
  tok,
  selected,
  onEdit,
  onDelete,
  onSelect,
}: {
  tok: ScopedToken;
  selected: boolean;
  onEdit: (t: ScopedToken) => void;
  onDelete: (t: ScopedToken) => void;
  onSelect: (t: ScopedToken) => void;
}) => {
  const [copied, setCopied] = useState(false);
  const { clearScopedToken, scopedToken } = useAuth();

  const handleCopy = () => {
    void navigator.clipboard
      .writeText(tok.token)
      .then(() => {
        setCopied(true);
        setTimeout(() => setCopied(false), 2000);
        toast.success(`Copied value of token "${tok.name}" to clipboard!`);
      })
      .catch(() => {
        toast.error(`Error copying value of token "${tok.name}" to clipboard.`);
      });
  };

  const handleSelect = () => {
    if (selected) {
      clearScopedToken();
    } else {
      onSelect(tok);
    }
  };

  const selectedName = selected ? 'Deactivate' : scopedToken !== undefined ? 'Switch' : 'Activate';

  return (
    <tr data-active={selected}>
      <NameCell>{tok.name}</NameCell>
      <td>
        <TokenValue>
          <IconButton variant={ButtonVariant.Icon} size={ButtonSize.Small} aria-label={`Copy token ${tok.name}`} onClick={handleCopy}>
            {copied ? <FaCheck /> : <FaClipboard />}
          </IconButton>
          <MaskedToken>***********</MaskedToken>
        </TokenValue>
      </td>
      <td>
        <TokenRoleView role={tok.role} />
      </td>
      <GroupsCell>
        <TokenGroups groups={tok.groups} />
      </GroupsCell>
      <NoWrapCell>{formatExpire(tok.token_expiration)}</NoWrapCell>
      <NoWrapCell>{formatExpire(tok.expires)}</NoWrapCell>
      <ActionsCell>
        <RowActions>
          <Button variant={ButtonVariant.Secondary} size={ButtonSize.Small} onClick={handleSelect}>
            {selectedName}
          </Button>
          <Button
            variant={ButtonVariant.Secondary}
            size={ButtonSize.Small}
            onClick={() => onEdit(tok)}
            disabled={scopedToken !== undefined}
          >
            Edit
          </Button>
          <Button variant={ButtonVariant.Danger} size={ButtonSize.Small} onClick={() => onDelete(tok)} disabled={scopedToken !== undefined}>
            Delete
          </Button>
        </RowActions>
      </ActionsCell>
    </tr>
  );
};

// A user role, with the enabled developer permissions listed under a developer role and the full
// permission set in the cell's title.
const TokenRoleView = ({ role }: { role: ScopedTokenRole }) => {
  if (role === ScopedTokenRoleKey.User) {
    return <span>{role}</span>;
  }
  const permissions = Object.entries(role.Developer);
  const enabled = permissions.filter(([, value]) => value).map(([key]) => key);
  return (
    <span title={permissions.map(([key, value]) => `${key}: ${value ? 'true' : 'false'}`).join('\n')}>
      Developer
      {enabled.length > 0 && <CellNote>{enabled.join(', ')}</CellNote>}
    </span>
  );
};

/** The groups a scoped token can be limited to: the user's own groups, even while a scoped token is active. */
const getAvailGroups = (userInfo: UserInfo | null): string[] => {
  if (!userInfo) return [];
  if (userInfo.actual_groups !== undefined) return userInfo.actual_groups;
  return userInfo.groups;
};

/** The scoped token panel: the user's tokens, their dialogs, and the run tokens listed for reference. */
const ScopedTokens = ({ list }: { list: ScopedTokenList }) => {
  const [showCreate, setShowCreate] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<ScopedToken | null>(null);
  const [editTarget, setEditTarget] = useState<ScopedToken | null>(null);
  const [selectTarget, setSelectTarget] = useState<ScopedToken | null>(null);

  const { scopedToken, userInfo } = useAuth();

  const availableGroups = getAvailGroups(userInfo);

  const { userTokens, runTokens } = useMemo(() => splitScopedTokens(list.tokens, scopedToken), [list.tokens, scopedToken]);

  // the modals reload the list after they change a token
  const reload = () => void list.reload();

  return (
    <ScopedPanel>
      <PanelHead>
        <PanelHeading>
          <Subtitle>Scoped tokens</Subtitle>
          <PanelDescription>Tokens limited to a subset of your groups.</PanelDescription>
        </PanelHeading>
        <Button variant={ButtonVariant.Ok} size={ButtonSize.Small} onClick={() => setShowCreate(true)} disabled={scopedToken !== undefined}>
          + New scoped token
        </Button>
      </PanelHead>
      {list.error && (
        <PanelBanner>
          <AlertBanner severity={Severity.Error}>{list.error}</AlertBanner>
        </PanelBanner>
      )}
      {!list.loaded && list.loading && <PanelNotice role="status">Loading scoped tokens…</PanelNotice>}
      {list.loaded && userTokens.length === 0 && <PanelNotice>No scoped tokens yet.</PanelNotice>}
      {userTokens.length > 0 && (
        <TableFrame $busy={list.loading} aria-busy={list.loading}>
          <TokenTable aria-label="Scoped tokens">
            <thead>
              <tr>
                <th>Name</th>
                <th>Token</th>
                <th>Role</th>
                <th>Groups</th>
                <th>Value expiry</th>
                <th>Token expiry</th>
                <ActionsHeader>Actions</ActionsHeader>
              </tr>
            </thead>
            <tbody>
              {userTokens.map((tok) => (
                <TokenRow
                  key={tok.name}
                  selected={scopedToken?.name === tok.name}
                  tok={tok}
                  onEdit={setEditTarget}
                  onDelete={setDeleteTarget}
                  onSelect={setSelectTarget}
                />
              ))}
            </tbody>
          </TokenTable>
        </TableFrame>
      )}
      {runTokens.length > 0 && (
        <PanelSubsection>
          <PanelHead>
            <PanelHeading>
              <Subtitle>Run tokens</Subtitle>
              <PanelDescription>
                Thorium creates a run token for each workflow or tool run and ends it with the run. They are listed for reference and cannot
                be changed here.
              </PanelDescription>
            </PanelHeading>
          </PanelHead>
          <RunTokenTable tokens={runTokens} />
        </PanelSubsection>
      )}

      <CreateTokenModal
        show={showCreate}
        availableGroups={availableGroups}
        onHide={() => setShowCreate(false)}
        onCreated={() => {
          setShowCreate(false);
          reload();
        }}
      />
      <DeleteTokenModal
        token={deleteTarget}
        onHide={() => setDeleteTarget(null)}
        onDeleted={() => {
          setDeleteTarget(null);
          reload();
        }}
      />
      <EditTokenModal
        token={editTarget}
        availableGroups={availableGroups}
        onHide={() => setEditTarget(null)}
        onUpdated={() => {
          setEditTarget(null);
          reload();
        }}
      />
      <SelectModal token={selectTarget} onHide={() => setSelectTarget(null)} />
    </ScopedPanel>
  );
};

// ─── Public export ────────────────────────────────────────────────────────────

/** The profile's token section: the root token and the scoped tokens. */
const TokenView = ({ list }: { list: ScopedTokenList }) => {
  return (
    <>
      <RootToken />
      <hr />
      <ScopedTokens list={list} />
    </>
  );
};

export default TokenView;
