import { useMemo, useState, JSX } from 'react';
import { Link } from 'react-router-dom';
import { Card, Col, Pagination, Row } from 'react-bootstrap';

// project imports
import { DetailsSectionProps } from '../EntityDetails';
import { clampPage, filterRuleSetMembers, memberDetailsUrl, pageCount } from './ruleSetRules';
import { DEFAULT_LIST_LIMIT } from '@entities/utilities';
import { BrowsingCard, BrowsingContents, EntityName, EntitySecondary, LinkFields } from '@entities/browsing/shared';
import { OmnibarRuleSetRules } from '@components/shared/inputs/omnibar/Bars';
import { Clause } from '@components/shared/inputs/omnibar/ClauseTypes';
import { defaultTimeSelection, TimeSelection } from '@components/shared/inputs/omnibar/timepicker/utils';
import { getLimitFromClauses } from '@components/shared/inputs/omnibar/utils';
import NoResultsBanner from '@components/shared/alerts/NoResultsBanner';
import FieldBadge from '@components/shared/badges/FieldBadge';
import Subtitle from '@components/shared/titles/Subtitle';
import { Entities, entityLabel } from '@models/entities';
import { RuleSetMember } from '@models/entities/rulesets';

interface RuleRowProps {
  rule: RuleSetMember;
}

/**
 * One member rule row: the name links to the rule's details page, unroutable kinds render linkless.
 *
 * @param props - The member rule to render.
 * @returns The member row.
 */
const RuleRow = ({ rule }: RuleRowProps): JSX.Element => {
  // members with no routable kind render linkless until a refetch hydrates them
  const url = memberDetailsUrl(rule);
  const fields = (
    <LinkFields>
      <EntityName>{rule.name ?? rule.id}</EntityName>
      <EntitySecondary>
        {/* a pending fallback member can lack a kind at runtime; skip its engine badge */}
        {rule.kind && <FieldBadge color="gray" field={entityLabel(rule.kind)} />}
      </EntitySecondary>
    </LinkFields>
  );
  return (
    <BrowsingCard>
      <BrowsingContents>
        {url ? (
          <Link to={url} className="no-decoration">
            {fields}
          </Link>
        ) : (
          fields
        )}
      </BrowsingContents>
    </BrowsingCard>
  );
};

/**
 * The ruleset details page's replacement for the default Associations graph card.
 *
 * Rulesets arrive with every member rule hydrated (membership is capped server side), so
 * this section filters and pages the member list entirely client side: an omnibar narrows
 * by name substring, engine kind, and page size, a time picker narrows by rule creation
 * date, and a Back/Next pager mirrors the browse pages' look. Filter state is local on
 * purpose — no details-page widget persists filters to the URL today; `useOmnibarUrlState`
 * is a drop-in if shareable filter links are wanted.
 *
 * @param props - The saved ruleset entity (pending edits stay in the metadata card's picker).
 * @returns The member rules section.
 */
const RuleSetRulesSection = ({ entity }: DetailsSectionProps<Entities.RuleSet>): JSX.Element => {
  const [clauses, setClausesState] = useState<Clause[]>([]);
  const [time, setTimeState] = useState<TimeSelection>(defaultTimeSelection());
  const [page, setPage] = useState(0);

  // a filter edit restarts paging; a remembered page index over a reshaped set is confusing
  const setClauses = (next: Clause[]) => {
    setClausesState(next);
    setPage(0);
  };

  // a time range edit restarts paging exactly like a clause edit
  const setTime = (next: TimeSelection) => {
    setTimeState(next);
    setPage(0);
  };

  // filter + name-sort the hydrated members whenever the entity, clauses, or range change
  const members = entity.metadata.RuleSet.rules;
  const filtered = useMemo(() => filterRuleSetMembers(members, clauses, time), [members, clauses, time]);
  // the page size rides in the clauses like every other omnibar-driven list
  const limit = getLimitFromClauses(clauses, DEFAULT_LIST_LIMIT);
  const maxPage = pageCount(filtered.length, limit);
  // clamp at render (not setState) so a shrink via save/refetch can never show an empty page
  const safePage = clampPage(page, filtered.length, limit);
  const pageMembers = filtered.slice(safePage * limit, (safePage + 1) * limit);
  const memberCount = members?.length ?? 0;

  return (
    <Card className="panel mt-4">
      <Card.Body>
        <div className="d-flex justify-content-center">
          <Subtitle>Rules</Subtitle>
        </div>
        <Row>
          <Col className="d-flex justify-content-center">
            <OmnibarRuleSetRules clauses={clauses} setClauses={setClauses} time={time} setTime={setTime} />
          </Col>
        </Row>
        {memberCount === 0 ? (
          <NoResultsBanner type="Rules" />
        ) : filtered.length === 0 ? (
          <NoResultsBanner type="Matching Rules" />
        ) : (
          <>
            <Row className="d-flex justify-content-center g-0">
              <BrowsingCard>
                <BrowsingContents>
                  <Row>
                    <EntityName>Name</EntityName>
                    <EntitySecondary>Kind</EntitySecondary>
                  </Row>
                </BrowsingContents>
              </BrowsingCard>
            </Row>
            {pageMembers.map((rule) => (
              <Row key={`ruleset-rule-${rule.id}`} className="d-flex justify-content-center g-0">
                <RuleRow rule={rule} />
              </Row>
            ))}
            <Row className="mt-3">
              <Col className="d-flex justify-content-center">
                {/* visible count so the filter's effect is obvious even mid-page */}
                <small>
                  {filtered.length} of {memberCount} rules
                </small>
              </Col>
            </Row>
            {filtered.length > limit && (
              <Row>
                <Col className="d-flex justify-content-center">
                  <Pagination>
                    <Pagination.Item onClick={() => setPage(safePage - 1)} disabled={safePage === 0}>
                      Back
                    </Pagination.Item>
                    <Pagination.Item onClick={() => setPage(safePage + 1)} disabled={safePage + 1 >= maxPage}>
                      Next
                    </Pagination.Item>
                  </Pagination>
                </Col>
              </Row>
            )}
          </>
        )}
      </Card.Body>
    </Card>
  );
};

export default RuleSetRulesSection;
