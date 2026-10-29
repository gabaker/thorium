// EntityDetails.tsx
import React, { useEffect, useState, createContext, useContext, Fragment, JSX, useMemo, useCallback, useRef } from 'react';
import { useParams, useNavigate } from 'react-router-dom';
import { Button, Card, Row, Col, Form, Modal } from 'react-bootstrap';
import { FaBackspace, FaRegEdit, FaSave, FaTrash } from 'react-icons/fa';
import { FaFileCirclePlus, FaSquarePlus } from 'react-icons/fa6';
import styled from 'styled-components';

// project imports
import ListCollectionButton from './ListCollectionsButton';
import ListHuntHitsButton from './ListHuntHitsButton';
import CancelHuntButton from './CancelHuntButton';
import { EntityDetailsConfig } from './configs/configs';
const AssociationGraph = React.lazy(() => import('@components/associations/graph/AssociationGraph'));
import { buildUpdateEntityForm } from '../utilities';
import Markdown from '@components/shared/syntax/Markdown';
import InfoHeader from '../shared/InfoHeader';
import InfoValue from '../shared/InfoValue';
import EntityGraphicUpload from '../shared/EntityGraphicUpload';
import Page from '@components/pages/Page';
import Title from '@components/shared/titles/Title';
import Subtitle from '@components/shared/titles/Subtitle';
import FieldBadge from '@components/shared/badges/FieldBadge';
import UserAvatar from '@components/shared/UserAvatar';
import { BuildDashboardButton, BuildDashboardResource } from '@components/shared/buttons';
import { OverlayTipBottom } from '@components/shared/overlay/tips';
import SelectInputArray from '@components/shared/inputs/selectable/SelectInputArray';
import CondensedEntityTags from '@components/tags/condensed/CondensedEntityTags';
import { useAuth } from '@utilities/auth';
import { deleteEntity, updateEntity, fetchEntityImage } from '@thorpi/entities';
import { CollectionKind } from '@models/entities/collections';
import { Entities, entityLabel, EntityTypeMap } from '@models/entities';
import { ButtonProps } from '@models/components';
import { GraphDataProvider } from '@components/associations/data/GraphDataContext';
import { getBrowsingPathByEntity } from '../browsing/EntityBrowsingRoutes';
import { getCreatePathByEntity } from '../create/EntityCreateRoutes';
import AlertBanner from '@components/shared/alerts/AlertBanner';

export type DetailsMetadataProps<T extends keyof EntityTypeMap> = {
  entity: EntityTypeMap[T];
  pendingEntity: EntityTypeMap[T];
  handleUpdate: <K extends keyof EntityTypeMap[T]>(field: K, value: EntityTypeMap[T][K]) => void;
  editing: boolean;
  /**
   * Re-fetch the saved entity after a mutation. Kind-gated controls living inside a meta
   * row (e.g. the yara rule quarantine controls) call this after their thorpi call lands,
   * mirroring how `EntityButtons` hands `refreshEntity` to `CancelHuntButton`.
   */
  refreshEntity: () => void;
  /** Surface an error on the details page's banner. */
  setError: (error: string) => void;
};

export type MetadataComponent<K extends keyof EntityTypeMap> = React.ComponentType<DetailsMetadataProps<K>>;

/**
 * Props handed to a kind-specific section that replaces the default Associations card.
 *
 * The section always renders the saved (fetched) entity: pending edits live in the
 * metadata card's inputs until a save publishes them back into `entity`.
 */
export type DetailsSectionProps<T extends keyof EntityTypeMap> = {
  entity: EntityTypeMap[T];
  editing: boolean;
};

export type SectionComponent<K extends keyof EntityTypeMap> = React.ComponentType<DetailsSectionProps<K>>;

type EntityDetailsContextType<T extends keyof EntityTypeMap> = {
  entity: EntityTypeMap[T];
  Metadata: MetadataComponent<T>;
  pendingEntity: EntityTypeMap[T];
  updatePendingEntity: <K extends keyof EntityTypeMap[T]>(field: K, value: EntityTypeMap[T][K]) => void;
  editing: boolean;
  applyEntityUpdates: (entity: EntityTypeMap[T], pendingEntity: EntityTypeMap[T]) => Promise<void>;
  toggleEditMode: () => void;
  refreshEntity: () => void;
  error: string;
  setError: (error: string) => void;
  supportsGraphic: boolean;
  graphicFile: File | null;
  setGraphicFile: (file: File | null) => void;
  clearGraphic: boolean;
  setClearGraphic: (clear: boolean) => void;
  existingImageUrl: string | null;
  existingImageIsSvg: boolean;
};

type EntityHeaderProps = {
  icon: (size: number) => JSX.Element;
  className?: string;
};

const EntityGraphic = styled.img`
  width: 80px;
  height: 80px;
  object-fit: contain;
  border-radius: 8px;
  background-color: var(--thorium-panel-bg);
`;

// submitter name followed by the submitter's avatar, vertically centered so the taller line reads cleanly
const SubmitterLine = styled.div`
  display: flex;
  align-items: center;
  gap: 0.5rem;
`;

const SvgGraphic = styled.div<{ $src: string }>`
  width: 80px;
  height: 80px;
  background-color: currentColor;
  mask-image: url(${(props) => props.$src});
  mask-size: contain;
  mask-repeat: no-repeat;
  mask-position: center;
  -webkit-mask-image: url(${(props) => props.$src});
  -webkit-mask-size: contain;
  -webkit-mask-repeat: no-repeat;
  -webkit-mask-position: center;
`;

/**
 * Decide whether a failed entity refetch means the entity itself no longer exists.
 *
 * `getEntity` formats a 404 either from the API's JSON error body (`Get Entity:
 * Entity <id> not found`) or, when no parseable body exists, as `Failed to Get
 * Entity: 404`. A gone entity can never report fresh status again, so the details
 * poll treats either shape as terminal and stops refetching.
 *
 * @param error - The formatted message handed to the refetch error handler.
 * @returns True when the message indicates the entity no longer exists.
 */
export function isEntityGoneError(error: string): boolean {
  return /\bnot found\b/i.test(error) || /\b404\b/.test(error);
}

/**
 * Compute the page error after a successful poll tick.
 *
 * A transient blip (one failed tick) must not pin a permanent banner over a
 * live-updating page, but a successful tick may only clear the banner the poll
 * itself raised — an error a user action surfaced in between stays visible.
 *
 * @param current - The error banner currently shown.
 * @param lastPollError - The message the last failed poll tick raised, if any.
 * @returns An empty string when the current banner is the poll's own, otherwise `current` unchanged.
 */
export function nextErrorAfterPollSuccess(current: string, lastPollError: string | null): string {
  return current !== '' && current === lastPollError ? '' : current;
}

export function createEntityDetailsPage<T extends keyof EntityTypeMap>(config: EntityDetailsConfig<T>) {
  const EntityDetailsContext = createContext<EntityDetailsContextType<T> | undefined>(undefined);

  const useEntityContext = () => {
    const context = useContext(EntityDetailsContext);
    if (context === undefined) {
      throw new Error('useEntityContext must be used within an EntityDetailsContext.Provider');
    }
    return context;
  };

  const EntityInfo = () => {
    const {
      entity,
      Metadata,
      pendingEntity,
      editing,
      updatePendingEntity,
      refreshEntity,
      setError,
      supportsGraphic,
      graphicFile,
      setGraphicFile,
      clearGraphic,
      setClearGraphic,
      existingImageUrl,
    } = useEntityContext();
    const { userInfo } = useAuth();

    const handleGraphicChange = useCallback(
      (file: File | null) => {
        setGraphicFile(file);
        if (file) setClearGraphic(false);
      },
      [setGraphicFile, setClearGraphic],
    );

    const handleClearExisting = useCallback(() => {
      setClearGraphic(true);
      setGraphicFile(null);
    }, [setClearGraphic, setGraphicFile]);
    return (
      <>
        <Card className="panel">
          <Card.Body>
            {!editing && (
              <>
                <Row>
                  <InfoHeader>ID</InfoHeader>
                  <Col>{entity.id}</Col>
                </Row>
                <hr className="my-3" />
              </>
            )}
            <Row className="d-flex flex-row justify-content-center">
              <InfoHeader>Name</InfoHeader>
              <InfoValue>
                {editing ? (
                  <Form.Group>
                    <Form.Control onChange={(e) => updatePendingEntity('name', String(e.target.value))} value={pendingEntity.name ?? ''} />
                  </Form.Group>
                ) : (
                  <>{entity.name}</>
                )}
              </InfoValue>
            </Row>
            <hr className="my-3" />
            {!editing && (
              <>
                <Row>
                  <InfoHeader>Type</InfoHeader>
                  <Col>{entityLabel(entity.kind)}</Col>
                </Row>
                <hr className="my-3" />
              </>
            )}
            <Metadata
              entity={entity}
              pendingEntity={pendingEntity}
              handleUpdate={updatePendingEntity}
              editing={editing}
              refreshEntity={refreshEntity}
              setError={setError}
            />
            <hr className="my-3" />
            <Row>
              <InfoHeader>Description</InfoHeader>
              <InfoValue>
                {editing ? (
                  <Form.Group>
                    <Form.Control
                      onChange={(e) => updatePendingEntity('description', String(e.target.value))}
                      value={pendingEntity.description ?? ''}
                      as="textarea"
                    />
                  </Form.Group>
                ) : (
                  <Markdown>{entity.description && entity.description !== 'null' ? entity.description : ''}</Markdown>
                )}
              </InfoValue>
            </Row>
            {supportsGraphic && editing && (
              <>
                <hr className="my-3" />
                <Row>
                  <InfoHeader>Graphic</InfoHeader>
                  <InfoValue>
                    <EntityGraphicUpload
                      file={graphicFile}
                      onChange={handleGraphicChange}
                      existingImageUrl={clearGraphic ? null : existingImageUrl}
                      onClearExisting={handleClearExisting}
                    />
                  </InfoValue>
                </Row>
              </>
            )}
            <hr className="my-3" />
            {!editing && (
              <>
                <Row>
                  <InfoHeader>Submitter</InfoHeader>
                  <InfoValue>
                    <SubmitterLine>
                      <UserAvatar username={entity.submitter} size={28} />
                      {entity.submitter}
                    </SubmitterLine>
                  </InfoValue>
                </Row>
                <hr className="my-3" />
              </>
            )}
            <Row>
              <InfoHeader>Groups</InfoHeader>
              <InfoValue>
                {editing ? (
                  <SelectInputArray
                    isCreatable={false}
                    options={[...(userInfo?.groups ?? [])].sort((a, b) => a.localeCompare(b))}
                    values={pendingEntity.groups}
                    onChange={(groups) => updatePendingEntity('groups', groups)}
                  />
                ) : (
                  <FieldBadge color="gray" field={entity.groups} />
                )}
              </InfoValue>
            </Row>
            {!editing && (
              <>
                <hr className="my-3" />
                <Row>
                  <InfoHeader>Created On</InfoHeader>
                  <Col>{entity.created}</Col>
                </Row>
              </>
            )}
          </Card.Body>
        </Card>
        {entity.tags !== undefined && (
          <Card className="panel mt-4">
            <Card.Body>
              <center>
                <Subtitle>Tags</Subtitle>
              </center>
              <CondensedEntityTags resource={entity.kind} tags={entity.tags} />
            </Card.Body>
          </Card>
        )}
      </>
    );
  };

  const DeleteButton: React.FC<ButtonProps> = ({ disabled = false }) => {
    const navigate = useNavigate();
    const { entity, setError } = useEntityContext();
    const [showDeleteModal, setShowDeleteModal] = useState(false);
    const [disableConfirmButton, setDisableConfirmButton] = useState(false);

    const handleRemoveClick = async () => {
      setDisableConfirmButton(true);
      const res = await deleteEntity(entity.id, setError);
      if (res === true) {
        setShowDeleteModal(false);
        void navigate(`${getBrowsingPathByEntity(entity.kind)}`);
      }
    };

    return (
      <>
        <OverlayTipBottom tip={`Delete "${entity.name}"`}>
          <Button className="icon-btn mx-1" variant="" disabled={disabled} onClick={() => setShowDeleteModal(true)}>
            <FaTrash size={20} />
          </Button>
        </OverlayTipBottom>

        <Modal show={showDeleteModal} onHide={() => setShowDeleteModal(false)} backdrop="static" keyboard={false}>
          <Modal.Header closeButton>
            <Modal.Title>Confirm deletion</Modal.Title>
          </Modal.Header>
          <Modal.Body>
            <div className="text-center">
              Do you really want to delete this {entityLabel(entity.kind)}?
              <div className="pt-4">
                <b>{entity.name}</b>
              </div>
              <b>{entity.id}</b>
            </div>
          </Modal.Body>
          <Modal.Footer className="d-flex justify-content-center">
            <Button
              className="danger-btn"
              onClick={() => {
                void handleRemoveClick();
              }}
              disabled={disableConfirmButton}
            >
              Confirm
            </Button>
            <Button className="primary-btn" onClick={() => setShowDeleteModal(false)}>
              Cancel
            </Button>
          </Modal.Footer>
        </Modal>
      </>
    );
  };

  const UploadFileButton: React.FC<ButtonProps> = ({ disabled = false }) => {
    const navigate = useNavigate();
    const { entity } = useEntityContext();

    return (
      <OverlayTipBottom tip={`Upload files associated with this ${entityLabel(entity.kind)}.`}>
        <Button
          className="icon-btn mx-1"
          variant=""
          disabled={disabled}
          onClick={() => {
            void navigate('/analyze', { state: { entity } });
          }}
        >
          <FaFileCirclePlus size={20} />
        </Button>
      </OverlayTipBottom>
    );
  };

  const CreateButton: React.FC<ButtonProps> = ({ className, disabled = false }) => {
    const navigate = useNavigate();
    const { entity } = useEntityContext();
    // resolve the canonical create route for this kind (undefined for machine-written kinds)
    const createPath = getCreatePathByEntity(entity.kind);

    // no create page to copy into: render nothing rather than a dead hand-built /create link
    if (createPath === undefined) {
      return null;
    }

    return (
      <OverlayTipBottom tip={`Copy this ${entityLabel(entity.kind)}.`}>
        <Button
          className={`${className} icon-btn mx-1`}
          variant=""
          disabled={disabled}
          onClick={() => {
            void navigate(createPath, { state: { entity } });
          }}
        >
          <FaSquarePlus size={20} />
        </Button>
      </OverlayTipBottom>
    );
  };

  const EditButton: React.FC<ButtonProps> = ({ disabled }) => {
    const { entity, pendingEntity, applyEntityUpdates, editing, toggleEditMode } = useEntityContext();

    return (
      <Fragment>
        <OverlayTipBottom tip={editing ? `Cancel changes to "${entity.name}"` : `Edit "${entity.name}"`}>
          <Button className="icon-btn mx-1" variant="" disabled={disabled} onClick={toggleEditMode}>
            {editing ? <FaBackspace size="24" /> : <FaRegEdit size="24" />}
          </Button>
        </OverlayTipBottom>

        {editing && (
          <OverlayTipBottom tip="Save pending changes">
            <Button
              className="icon-btn mx-1"
              variant=""
              disabled={disabled}
              onClick={() => {
                void applyEntityUpdates(entity, pendingEntity);
              }}
            >
              <FaSave size={20} />
            </Button>
          </OverlayTipBottom>
        )}
      </Fragment>
    );
  };

  const EntityButtons: React.FC<ButtonProps> = ({ className }) => {
    const { entity, setError, refreshEntity } = useEntityContext();
    const canModify = true;
    const canCreate = true;
    // only kinds with a create route are human-creatable: machine-written kinds (hits,
    // carved sections, ...) get neither a copy-into-create button nor an upload flow,
    // so the next machine-written kind stays suppressed without extending a kind list
    const creatable = getCreatePathByEntity(entity.kind) !== undefined;

    return (
      <div className={`d-flex justify-content-center ${className}`}>
        <EditButton disabled={!canModify} />
        <DeleteButton disabled={!canModify} />
        {creatable && (
          <>
            <CreateButton disabled={!canCreate} />
            <UploadFileButton disabled={!canCreate} />
          </>
        )}
        <BuildDashboardButton
          className="mx-1"
          resource={BuildDashboardResource.Entity}
          id={entity.id}
          name={entity.name}
          label={entityLabel(entity.kind).toLowerCase()}
        />

        {entity.kind === Entities.Collection && entity.metadata.Collection.collection_kind === CollectionKind.Files && (
          <ListCollectionButton collection={entity} />
        )}

        {entity.kind === Entities.Hunt && (
          <>
            {/* the full hit browse keeps the omnibar/time filtering the inline findings
                panel skips; the button self-hides for blank hunts and unknown engines */}
            <ListHuntHitsButton hunt={entity} />
            <CancelHuntButton hunt={entity} setError={setError} onCancelled={refreshEntity} />
          </>
        )}
      </div>
    );
  };

  const IconTitle = styled.div`
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 8px;
    color: var(--thorium-text);
    word-break: break-all;
  `;

  const EntityHeader: React.FC<EntityHeaderProps> = ({ className, icon }) => {
    const { entity, existingImageUrl, existingImageIsSvg, graphicFile } = useEntityContext();
    const headerImageUrl = graphicFile ? URL.createObjectURL(graphicFile) : existingImageUrl;
    const isSvg = !graphicFile && existingImageIsSvg;

    return (
      <Card className={`panel ${className ?? ''}`}>
        <Card.Body>
          <IconTitle>
            <div className="pe-8">
              {headerImageUrl ? (
                isSvg ? (
                  <SvgGraphic $src={headerImageUrl} />
                ) : (
                  <EntityGraphic src={headerImageUrl} alt={entity.name} />
                )
              ) : (
                icon(100)
              )}
            </div>
            <Title className="title">{entity.name}</Title>
          </IconTitle>
        </Card.Body>
      </Card>
    );
  };

  const BoundEntityDetailsPage = () => {
    const { entityID } = useParams();
    const [entity, setEntity] = useState<EntityTypeMap[T]>(config.BlankEntity);
    const [pendingEntity, setPendingEntity] = useState<EntityTypeMap[T]>(config.BlankEntity);
    const [error, setError] = useState('');
    const [editing, setEditing] = useState(false);
    const [graphicFile, setGraphicFile] = useState<File | null>(null);
    const [clearGraphic, setClearGraphic] = useState(false);
    const [existingImageUrl, setExistingImageUrl] = useState<string | null>(null);
    const [existingImageIsSvg, setExistingImageIsSvg] = useState(false);
    // set once a background refetch learns the entity no longer exists (deleted elsewhere):
    // it gates the auto-refresh interval off so the page stops refiring a doomed request
    const [entityGone, setEntityGone] = useState(false);
    // graphic upload is on for every entity by default; a config may set supportsGraphic:false to opt out
    const supportsGraphic = config.supportsGraphic ?? true;
    // live mirrors of edit/mount state for background responses: refreshEntity reads these at
    // response time, so an edit begun (or an unmount) after the request fired still blocks the update
    const editingRef = useRef(editing);
    const mountedRef = useRef(true);
    // the banner text raised by the last failed poll tick, so the next successful tick clears
    // only its own noise and never an error a user action surfaced in between
    const lastPollErrorRef = useRef<string | null>(null);

    useEffect(() => {
      // keep the edit-mode mirror current for responses already in flight
      editingRef.current = editing;
    }, [editing]);

    useEffect(() => {
      // mark mounted for the page's lifetime (re-marking on a StrictMode remount) so
      // refresh responses landing after teardown can be dropped
      mountedRef.current = true;
      return () => {
        mountedRef.current = false;
      };
    }, []);

    const toggleEditMode = () => {
      setEditing((prev) => {
        if (prev) {
          setGraphicFile(null);
          setClearGraphic(false);
        }
        return !prev;
      });
    };

    function updatePendingEntity<K extends keyof EntityTypeMap[T]>(field: K, value: EntityTypeMap[T][K]) {
      const updates: EntityTypeMap[T] = structuredClone(pendingEntity);
      updates[field] = value;
      setPendingEntity(updates);
    }

    const handleEntityUpdate = useCallback((newEntity: EntityTypeMap[T]) => {
      setEntity(newEntity);
      setPendingEntity(newEntity);
    }, []);

    // re-fetch the entity on demand (kind-gated action buttons use this after a mutation);
    // guarded like the poll below: responses are dropped after unmount and never overwrite
    // pendingEntity once edit mode is open, even when editing started mid-flight
    const refreshEntity = useCallback(() => {
      if (entityID) {
        config.getEntityDetails(entityID, setError, (newEntity: EntityTypeMap[T]) => {
          // drop stale responses instead of clobbering pending edits or an unmounted page
          if (mountedRef.current && !editingRef.current) {
            handleEntityUpdate(newEntity);
          }
        });
      }
    }, [entityID, handleEntityUpdate]);

    const applyEntityUpdates = async (entity: EntityTypeMap[T], pendingEntity: EntityTypeMap[T]) => {
      if (entityID) {
        const success = await updateEntity(entityID, buildUpdateEntityForm(entity, pendingEntity, graphicFile, clearGraphic), setError);
        if (success) {
          setEntity(pendingEntity);
          setGraphicFile(null);
          setClearGraphic(false);
          toggleEditMode();
          config.getEntityDetails(entityID, setError, handleEntityUpdate);
          if (supportsGraphic) {
            const img = await fetchEntityImage(entityID);
            setExistingImageUrl(img?.url ?? null);
            setExistingImageIsSvg(img?.isSvg ?? false);
          }
        }
      }
    };

    useEffect(() => {
      if (entityID) {
        // a new entity id starts with a fresh verdict for the refetch gate below
        setEntityGone(false);
        lastPollErrorRef.current = null;
        config.getEntityDetails(entityID, setError, handleEntityUpdate);
        if (supportsGraphic) {
          void fetchEntityImage(entityID).then((img) => {
            setExistingImageUrl(img?.url ?? null);
            setExistingImageIsSvg(img?.isSvg ?? false);
          });
        }
      }
    }, [entityID, supportsGraphic, handleEntityUpdate]);

    // opt-in auto-refresh: a primitive interval derived from the freshly fetched entity, so
    // the effect only re-runs when the derived value changes (e.g. 10s while a hunt is
    // active, null once terminal) and never while an edit is in flight — a landing refetch
    // would clobber pendingEntity. A deleted entity is terminal too: its stale status would
    // otherwise ask for polling forever, so the gone flag forces the interval off
    const refetchMs = !entityGone && config.refetchIntervalMs ? config.refetchIntervalMs(entity) : null;
    useEffect(() => {
      if (!entityID || editing || refetchMs == null) {
        return;
      }
      // drop any poll response landing after teardown: a fetch in flight when editing
      // starts would otherwise reset pendingEntity mid-edit
      let active = true;
      const guardedUpdate = (newEntity: EntityTypeMap[T]) => {
        if (active) {
          // a good tick heals the banner a previous failed tick raised (and only that one)
          setError((current) => nextErrorAfterPollSuccess(current, lastPollErrorRef.current));
          lastPollErrorRef.current = null;
          handleEntityUpdate(newEntity);
        }
      };
      // surface poll failures, remembering the message so the next good tick can clear it;
      // an entity-gone failure stops the polling for good and leaves one clear banner
      const guardedError = (err: string) => {
        if (active) {
          lastPollErrorRef.current = err;
          setError(err);
          if (isEntityGoneError(err)) {
            setEntityGone(true);
          }
        }
      };
      const intervalId = setInterval(() => config.getEntityDetails(entityID, guardedError, guardedUpdate), refetchMs);
      return () => {
        active = false;
        clearInterval(intervalId);
      };
    }, [entityID, editing, refetchMs, handleEntityUpdate]);

    const associationInitial = useMemo(() => ({ entities: entityID ? [entityID] : [] }), [entityID]);

    // a kind config may replace the default associations graph card with its own section
    const AssociationsSection = config.AssociationsSection;

    return (
      <EntityDetailsContext.Provider
        value={{
          entity,
          Metadata: config.EntityMetaInfo,
          pendingEntity,
          updatePendingEntity,
          editing,
          applyEntityUpdates,
          toggleEditMode,
          refreshEntity,
          error,
          setError,
          supportsGraphic,
          graphicFile,
          setGraphicFile,
          clearGraphic,
          setClearGraphic,
          existingImageUrl,
          existingImageIsSvg,
        }}
      >
        <Page className="full-min-width" title={`${entityLabel(entity.kind)} · ${entity.name}`}>
          <EntityHeader icon={config.icon} />
          <EntityButtons className="py-3" />
          {error !== '' && <AlertBanner className="mb-3">{error}</AlertBanner>}
          <EntityInfo />
          {entityID &&
            (AssociationsSection ? (
              // the kind-specific section owns the whole card and skips graph data fetching
              <AssociationsSection entity={entity} editing={editing} />
            ) : (
              <GraphDataProvider initial={associationInitial}>
                <Card className="panel mt-4">
                  <Card.Body>
                    <div className="d-flex justify-content-center">
                      <Subtitle>Associations</Subtitle>
                    </div>
                    <AssociationGraph inView bordered={false} />
                  </Card.Body>
                </Card>
              </GraphDataProvider>
            ))}
        </Page>
      </EntityDetailsContext.Provider>
    );
  };

  return BoundEntityDetailsPage;
}
