import { useMemo, useState, useEffect } from 'react';
import { useNavigate } from 'react-router-dom';
import { Alert, Button, Col, Container, Modal, Row } from 'react-bootstrap';
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
import styles from './TokenViewStyles.module.scss';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Button as ActionButton, ButtonSize, ButtonVariant, IconButton } from '@components/shared/buttons';
import { OverlayTipTop } from '@components/shared/overlay/tips';
import Time from '@components/shared/Time';
import Subtitle from '@components/shared/titles/Subtitle';
import { useAuth } from '@utilities/auth';
import { createScopedToken, deleteScopedToken, updateScopedToken } from '@thorpi/users';
import { BUTTON_BADGE_GAP } from '@styles';
import { ScopedToken, ScopedTokenRole, ScopedTokenRoleKey, ScopedTokenUpdate, UserInfo } from '@models/users';

// ─── Shared form ──────────────────────────────────────────────────────────────
// ─── Root token ───────────────────────────────────────────────────────────────

const RevokeTokenModal = ({ show, onHide }: { show: boolean; onHide: () => void }) => {
  const { revoke } = useAuth();
  const navigate = useNavigate();
  const handleRevoke = () => {
    void revoke().then(() => {
      void navigate('/');
    });
  };
  return (
    <Modal show={show} onHide={onHide}>
      <Modal.Header closeButton>
        <Modal.Title>Revoke Your Token?</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        Revoking your token will automatically log you out of this page and any currently running or queued analysis jobs (reactions) may
        fail. Are you sure?
      </Modal.Body>
      <Modal.Footer className="d-flex justify-content-center">
        <Button className="danger-btn" onClick={() => handleRevoke()}>
          Confirm
        </Button>
      </Modal.Footer>
    </Modal>
  );
};

// Token value with the inline show/hide and revoke controls at the end; the token text takes the
// available width and wraps, keeping the controls at the end of the row.
const TokenLine = styled.div`
  display: flex;
  align-items: center;
  gap: ${BUTTON_BADGE_GAP};

  .wrap-token {
    flex: 1;
    min-width: 0;
  }

  p {
    margin: 0;
  }
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

const RootToken = () => {
  const [showRevokeModal, setShowRevokeModal] = useState(false);
  const [tokenShowing, setTokenShowing] = useState(false);
  const { userInfo, clearScopedToken, scopedToken } = useAuth();

  return (
    <Container>
      <Row>
        <Col xs={2}>
          <Subtitle>Token</Subtitle>
        </Col>
        <Col xs={10}>
          <TokenLine>
            <div className="wrap-token">
              {tokenShowing ? (
                <p>{userInfo?.token}</p>
              ) : (
                <p className="hidden">****************************************************************</p>
              )}
            </div>
            {scopedToken && <Button onClick={() => clearScopedToken()}>Use Root Token</Button>}
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
        </Col>
      </Row>
      <Row className="pt-3">
        <Col xs={2}>
          <Subtitle>Expiry</Subtitle>
        </Col>
        <Col>
          <p>{userInfo?.token_expiration ? <Time>{userInfo.token_expiration}</Time> : null}</p>
        </Col>
      </Row>
      <RevokeTokenModal show={showRevokeModal} onHide={() => setShowRevokeModal(false)} />
    </Container>
  );
};

// ─── Create modal ─────────────────────────────────────────────────────────────

type CreateModalProps = {
  show: boolean;
  availableGroups: string[];
  onHide: () => void;
  onCreated: () => void;
};

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
    <Modal show={show} onHide={handleHide}>
      <Modal.Header closeButton>
        <Modal.Title>New scoped token</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        {error && <Alert variant="danger">{error}</Alert>}
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
      </Modal.Body>
      <Modal.Footer>
        <Button variant="secondary" onClick={handleHide}>
          Cancel
        </Button>
        <Button className="primary-btn" onClick={() => void handleSubmit()} disabled={submitting}>
          {submitting ? 'Creating…' : 'Create'}
        </Button>
      </Modal.Footer>
    </Modal>
  );
};

// ─── Delete modal ─────────────────────────────────────────────────────────────

type DeleteModalProps = {
  token: ScopedToken | null;
  onHide: () => void;
  onDeleted: () => void;
};

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
    <Modal show={!!token} onHide={handleHide}>
      <Modal.Header closeButton>
        <Modal.Title>Delete Scoped Token?</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        {error && <Alert variant="danger">{error}</Alert>}
        {token && (
          <p>
            Delete <strong>{token.name}</strong>? This cannot be undone.
          </p>
        )}
      </Modal.Body>
      <Modal.Footer>
        <Button variant="secondary" onClick={handleHide}>
          Cancel
        </Button>
        <Button className="danger-btn" onClick={() => void handleDelete()} disabled={submitting}>
          {submitting ? 'Deleting…' : 'Delete'}
        </Button>
      </Modal.Footer>
    </Modal>
  );
};

// ─── Edit modal ───────────────────────────────────────────────────────────────

type EditModalProps = {
  token: ScopedToken | null;
  availableGroups: string[];
  onHide: () => void;
  onUpdated: () => void;
};

const EditTokenModal = ({ token, availableGroups, onHide, onUpdated }: EditModalProps) => {
  const [selectedGroups, setSelectedGroups] = useState<string[]>([]);
  const [expires, setExpires] = useState('');
  const [role, setRole] = useState<ScopedTokenRole>(token ? token.role : ScopedTokenRoleKey.User);
  const [clearExpires, setClearExpires] = useState(false);
  const [error, setError] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const { userInfo } = useAuth();

  useEffect(() => {
    //refresh any time token selected
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
    <Modal show={!!token} onHide={handleHide}>
      <Modal.Header closeButton>
        <Modal.Title>Edit Scoped Token{token ? ` — ${token.name}` : ''}</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        {error && <Alert variant="danger">{error}</Alert>}
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
      </Modal.Body>
      <Modal.Footer>
        <Button variant="secondary" onClick={handleHide}>
          Cancel
        </Button>
        <Button className="primary-btn" onClick={() => void handleSubmit()} disabled={submitting}>
          {submitting ? 'Saving…' : 'Save'}
        </Button>
      </Modal.Footer>
    </Modal>
  );
};

type SelectModalProps = {
  token: ScopedToken | null;
  onHide: () => void;
};

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
    <Modal show={!!token} onHide={onHide}>
      <Modal.Header closeButton>
        <Modal.Title>Activate Scoped Token{token ? ` — ${token.name}` : ''}</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        Select "{token?.name}" as token? This will limit visibility to the following groups:
        <ul className={styles.auto_overflow}>{groups}</ul>
        Several features of Thorium UI may be unusable with a scoped token. To re-enable full functionality, disable the scoped token.
      </Modal.Body>
      <Modal.Footer>
        <Button variant="secondary" onClick={onHide}>
          Cancel
        </Button>
        <Button variant="primary" onClick={() => void handleSubmit()}>
          Change Token
        </Button>
      </Modal.Footer>
    </Modal>
  );
};

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
          <ActionButton variant={ButtonVariant.Secondary} size={ButtonSize.Small} onClick={handleSelect}>
            {selectedName}
          </ActionButton>
          <ActionButton
            variant={ButtonVariant.Secondary}
            size={ButtonSize.Small}
            onClick={() => onEdit(tok)}
            disabled={scopedToken !== undefined}
          >
            Edit
          </ActionButton>
          <ActionButton
            variant={ButtonVariant.Danger}
            size={ButtonSize.Small}
            onClick={() => onDelete(tok)}
            disabled={scopedToken !== undefined}
          >
            Delete
          </ActionButton>
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

const getAvailGroups = (userInfo: UserInfo | null): string[] => {
  if (!userInfo) return [];
  if (userInfo.actual_groups !== undefined) return userInfo.actual_groups;
  return userInfo.groups;
};

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
        <ActionButton
          variant={ButtonVariant.Ok}
          size={ButtonSize.Small}
          onClick={() => setShowCreate(true)}
          disabled={scopedToken !== undefined}
        >
          + New scoped token
        </ActionButton>
      </PanelHead>
      {list.error && (
        <AlertBanner severity={Severity.Error} className="mt-3 mb-0">
          {list.error}
        </AlertBanner>
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
