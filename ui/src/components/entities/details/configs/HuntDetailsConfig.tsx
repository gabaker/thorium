import { JSX } from 'react';
import { Badge, Col, ProgressBar, Row } from 'react-bootstrap';
import { FaBinoculars, FaCheckCircle } from 'react-icons/fa';

// project imports
import { DetailsMetadataProps } from '../EntityDetails';
import { EntityDetailsConfig } from './configs';
import { makeGetEntityDetails } from './factory';
import HuntFindingsSection from './HuntFindingsSection';
import { describeSelector, huntBarClass, huntStatusVariant, laneBarClass, selectorRuleSetId } from './huntDisplay';
import { laneFraction, overallProgress } from './huntProgress';
import InfoValue from '@entities/shared/InfoValue';
import InfoHeader from '@entities/shared/InfoHeader';
import LinkBadge from '@components/shared/badges/LinkBadge';
import FieldBadge from '@components/shared/badges/FieldBadge';
import AlertBanner from '@components/shared/alerts/AlertBanner';
import { Entities } from '@models/entities';
import { BlankHunt, BlankHuntCounters, HuntLane, HUNT_ENGINES, isActiveHuntStatus } from '@models/entities/hunts';
import { getDetailsBasePathByEntity } from '@components/entities/details/EntityDetailsRoutes';

// the counter tallies in display order; raw counts only — there is no item denominator
const COUNTER_FIELDS = ['listed', 'scanned', 'matched', 'written', 'suppressed', 'skipped', 'missing', 'errors'] as const;

// render a lane bound/current timestamp compactly
const laneTime = (stamp: string): string => (stamp ? new Date(stamp).toLocaleString() : '');

// Hunt metadata is machine-written (created by the dedicated route, advanced by checkpoints):
// the block renders read-only in both modes so the generic edit path (name/description/
// groups) never fabricates metadata inputs.
const HuntMetaInfo = ({ entity }: DetailsMetadataProps<Entities.Hunt>): JSX.Element => {
  const meta = entity.metadata.Hunt;
  // counters and failure are serde-defaulted on the wire; guard hunts from older builds
  const counters = meta.counters ?? BlankHuntCounters;
  const ruleSetId = selectorRuleSetId(meta.selector);
  return (
    <>
      <Row className="d-flex flex-row justify-content-center">
        <InfoHeader>Engine</InfoHeader>
        <InfoValue>{HUNT_ENGINES[meta.engine]?.label ?? meta.engine}</InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Status</InfoHeader>
        <InfoValue>
          <Badge bg={huntStatusVariant(meta.status)}>{meta.status}</Badge>
        </InfoValue>
      </Row>
      {meta.failure && <AlertBanner className="mt-3">{`This hunt failed: ${meta.failure}`}</AlertBanner>}
      <hr className="my-3" />
      <Row>
        <InfoHeader>Selector</InfoHeader>
        <InfoValue>
          {describeSelector(meta.selector)}
          {ruleSetId && (
            <div className="mt-1">
              <LinkBadge internal url={`${getDetailsBasePathByEntity(Entities.RuleSet)}/${ruleSetId}`} label={ruleSetId} />
            </div>
          )}
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Frozen Rules</InfoHeader>
        <InfoValue>
          {`${meta.rules.length} frozen at creation`}
          {meta.excluded_quarantined > 0 && (
            <Badge bg="warning" className="ms-2">{`${meta.excluded_quarantined} quarantined excluded`}</Badge>
          )}
          {meta.excluded_foreign > 0 && <Badge bg="secondary" className="ms-2">{`${meta.excluded_foreign} foreign excluded`}</Badge>}
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Corpus</InfoHeader>
        <InfoValue>
          {'Files' in meta.corpus ? (
            <>
              <div>Files</div>
              <FieldBadge color="gray" field={meta.corpus.Files.groups} />
              <div className="mt-1">
                <small>
                  {laneTime(meta.corpus.Files.end)} → {laneTime(meta.corpus.Files.start)}
                </small>
              </div>
              {Object.keys(meta.corpus.Files.tags).length > 0 && (
                <div className="mt-1">
                  {Object.entries(meta.corpus.Files.tags).map(([key, values]) =>
                    values.map((value) => (
                      <Badge bg="secondary" className="me-1" key={`corpus-tag-${key}-${value}`}>
                        {key}={value}
                      </Badge>
                    )),
                  )}
                  {meta.corpus.Files.tags_case_insensitive && <small> (case-insensitive)</small>}
                </div>
              )}
            </>
          ) : (
            <div>Repos</div>
          )}
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Progress</InfoHeader>
        <InfoValue>
          <ProgressBar
            className={huntBarClass(meta.status)}
            now={Math.round(overallProgress(meta.lanes) * 100)}
            label={`${Math.round(overallProgress(meta.lanes) * 100)}%`}
            animated={isActiveHuntStatus(meta.status)}
          />
          {/* lane detail collapses behind an uncontrolled disclosure (like the error
              samples below) so the 10s poll re-render cannot snap it shut */}
          {meta.lanes.length > 0 && (
            <details className="mt-2">
              <summary>
                <small>{`Lane progress (${meta.lanes.length} ${meta.lanes.length === 1 ? 'lane' : 'lanes'})`}</small>
              </summary>
              {meta.lanes.map((lane: HuntLane, idx: number) => (
                <Row className="mt-2 align-items-center" key={`hunt-lane-${idx}`}>
                  <Col xs={2}>
                    <small>{`Lane ${idx}`}</small>
                  </Col>
                  <Col xs={6}>
                    <ProgressBar
                      className={laneBarClass(meta.status, lane)}
                      now={Math.round(laneFraction(lane) * 100)}
                      label={`${Math.round(laneFraction(lane) * 100)}%`}
                    />
                  </Col>
                  <Col xs={4}>
                    <small>{lane.exhausted ? <FaCheckCircle title="Lane exhausted" /> : laneTime(lane.current)}</small>
                  </Col>
                </Row>
              ))}
            </details>
          )}
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Counters</InfoHeader>
        <InfoValue>
          <Row>
            {COUNTER_FIELDS.map((field) => (
              <Col xs={6} md={3} key={`hunt-counter-${field}`}>
                <small className={field === 'errors' && counters.errors > 0 ? 'text-danger' : undefined}>
                  {field}: <b>{counters[field]}</b>
                </small>
              </Col>
            ))}
          </Row>
          {counters.error_samples.length > 0 && (
            <details className="mt-2">
              <summary>
                <small>{`Error samples (${counters.error_samples.length})`}</small>
              </summary>
              <ul>
                {counters.error_samples.map((sample: string, idx: number) => (
                  <li key={`hunt-error-sample-${idx}`}>
                    <small>
                      <code>{sample}</code>
                    </small>
                  </li>
                ))}
              </ul>
            </details>
          )}
          <div className="mt-1">
            <small>
              <i>Counters are telemetry and can drift under replay — the hits panel below is the exact record.</i>
            </small>
          </div>
        </InfoValue>
      </Row>
    </>
  );
};

const HuntDetailsConfig: EntityDetailsConfig<Entities.Hunt> = {
  getEntityDetails: makeGetEntityDetails(Entities.Hunt),
  EntityMetaInfo: HuntMetaInfo,
  // the findings card (hits by default, frozen rules behind its toggle) replaces the
  // associations graph card
  AssociationsSection: HuntFindingsSection,
  BlankEntity: BlankHunt,
  icon: (size: number) => <FaBinoculars size={size} />,
  supportsGraphic: false,
  // poll lane progress while the hunt is active; stop on terminal status (the blank
  // placeholder's empty id never polls)
  refetchIntervalMs: (hunt) => (hunt.id !== '' && isActiveHuntStatus(hunt.metadata.Hunt.status) ? 10_000 : null),
};

export default HuntDetailsConfig;
