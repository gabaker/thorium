import { JSX } from 'react';
import { Row } from 'react-bootstrap';
import { FaCrosshairs } from 'react-icons/fa';

// project imports
import { DetailsMetadataProps } from '../EntityDetails';
import { EntityDetailsConfig } from './configs';
import { makeGetEntityDetails } from './factory';
import InfoValue from '@entities/shared/InfoValue';
import InfoHeader from '@entities/shared/InfoHeader';
import LinkBadge from '@components/shared/badges/LinkBadge';
import { Entities } from '@models/entities';
import { BlankYaraRuleHit, MatchedRule, MatchedString } from '@models/entities/rules/yara_hits';
import { getDetailsBasePathByEntity } from '@components/entities/details/EntityDetailsRoutes';

// render one matched string as `$id ×count @ first offsets` (both lists are API-capped)
const matchedStringLine = (matched: MatchedString): string => {
  const offsets = matched.first_offsets.map((offset) => `0x${offset.toString(16)}`).join(', ');
  return offsets.length > 0 ? `${matched.id} ×${matched.count} @ ${offsets}` : `${matched.id} ×${matched.count}`;
};

// Hit metadata is machine-written by the scan path and hunts: the block renders read-only in
// both modes so the generic edit path (name/description/groups) never fabricates metadata inputs.
const YaraRuleHitMetaInfo = ({ entity }: DetailsMetadataProps<Entities.YaraRuleHit>): JSX.Element => {
  const meta = entity.metadata.YaraRuleHit;
  return (
    <>
      <Row className="d-flex flex-row justify-content-center">
        <InfoHeader>Rule</InfoHeader>
        <InfoValue>
          {meta.rule_id && (
            <LinkBadge
              internal
              url={`${getDetailsBasePathByEntity(Entities.YaraRule)}/${meta.rule_id}`}
              label={`${meta.rule_name} (${meta.rule_id})`}
            />
          )}
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Sample</InfoHeader>
        <InfoValue>{meta.sha256 && <LinkBadge internal url={`/file/${meta.sha256}`} label={meta.sha256} />}</InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Scanned At</InfoHeader>
        <InfoValue>{meta.scanned_at ? new Date(meta.scanned_at).toLocaleString() : ''}</InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Rule Hash</InfoHeader>
        <InfoValue>
          <code>{meta.rule_hash}</code>
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Ruleset Hash</InfoHeader>
        <InfoValue>
          <code>{meta.ruleset_hash}</code>
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Engine Version</InfoHeader>
        <InfoValue>{meta.engine_version}</InfoValue>
      </Row>
      {meta.matched.length > 0 && (
        <>
          <hr className="my-3" />
          <Row>
            <InfoHeader>Matched</InfoHeader>
            <InfoValue>
              {meta.matched.map((rule: MatchedRule, idx: number) => (
                <div key={`matched-rule-${idx}`}>
                  <code>{rule.rule_identifier}</code>
                  {rule.strings.length > 0 && (
                    <ul className="mb-1">
                      {rule.strings.map((str: MatchedString, strIdx: number) => (
                        <li key={`matched-string-${idx}-${strIdx}`}>
                          <code>{matchedStringLine(str)}</code>
                        </li>
                      ))}
                    </ul>
                  )}
                </div>
              ))}
            </InfoValue>
          </Row>
        </>
      )}
    </>
  );
};

const YaraRuleHitDetailsConfig: EntityDetailsConfig<Entities.YaraRuleHit> = {
  getEntityDetails: makeGetEntityDetails(Entities.YaraRuleHit),
  EntityMetaInfo: YaraRuleHitMetaInfo,
  BlankEntity: BlankYaraRuleHit,
  icon: (size: number) => <FaCrosshairs size={size} />,
  supportsGraphic: false,
};

export default YaraRuleHitDetailsConfig;
