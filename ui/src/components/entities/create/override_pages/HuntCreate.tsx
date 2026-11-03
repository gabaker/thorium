import { JSX, useEffect, useState } from 'react';
import { useLocation, useNavigate } from 'react-router';
import { Button, Card, Form, Row } from 'react-bootstrap';
import styled from 'styled-components';

// project imports
import {
  BlankHuntCreateFormState,
  buildHuntCreateRequest,
  describeCorpusBounds,
  HuntCreateFormState,
  HuntSelectorMode,
  huntToFormState,
  validateHuntForm,
} from './huntCreateRequest';
import InfoHeader from '@entities/shared/InfoHeader';
import InfoValue from '@entities/shared/InfoValue';
import { getAvailableEntities } from '@entities/utilities';
import { getDetailsBasePathByEntity } from '@entities/details/EntityDetailsRoutes';
import { CreateOnlyNote } from '@entities/details/configs/shared';
import Page from '@components/pages/Page';
import Title from '@components/shared/titles/Title';
import DatePicker from '@components/shared/inputs/DatePicker';
import { OverlayTipBottom } from '@components/shared/overlay/tips';
import SelectInputArray from '@components/shared/inputs/selectable/SelectInputArray';
import SelectableDictionary, { DictionaryEntry } from '@components/shared/inputs/selectable/SelectableDictionary';
import AlertBanner from '@components/shared/alerts/AlertBanner';
import { createHunt } from '@thorpi/hunts';
import { useAuth } from '@utilities/auth';
import { safeDateToStringConversion } from '@utilities/inputs';
import { Entities, EntityTypes } from '@models/entities/entities';
import { HuntEngine, HUNT_ENGINES } from '@models/entities/hunts';

const CreateEntityTitle = styled.div`
  display: grid;
  place-items: center;
  padding-bottom: 1rem;
  padding-top: 0.5rem;
`;

// the selector modes in display order with their user-facing labels
const SELECTOR_MODE_LABELS: Record<HuntSelectorMode, string> = {
  [HuntSelectorMode.Rules]: 'Explicit rules',
  [HuntSelectorMode.Tag]: 'Rules by tag',
  [HuntSelectorMode.All]: 'All visible rules',
  [HuntSelectorMode.RuleSet]: 'A rule set',
};

/**
 * The dedicated hunt create page.
 *
 * Hunts are created through `POST /entities/hunt/` (JSON) — the create does real work
 * (freeze the selector, split lanes, emit chunk events) — so this page replaces the
 * generic multipart create factory while mirroring its chrome. The engine select drives
 * the rule picker's kind through the engine registry, keeping the page engine-aware.
 */
const HuntCreate = (): JSX.Element => {
  const navigate = useNavigate();
  const { state: routeState } = useLocation() as { state: { entity?: EntityTypes } | null };
  // the details copy button lands here with a hunt to re-run; prefill from it once
  const initialState =
    routeState?.entity && routeState.entity.kind === Entities.Hunt
      ? huntToFormState(routeState.entity)
      : structuredClone(BlankHuntCreateFormState);
  const [form, setForm] = useState<HuntCreateFormState>(initialState);
  const [error, setError] = useState('');
  const [creating, setCreating] = useState(false);
  const [rulesMap, setRulesMap] = useState<{ [key: string]: string }>({});
  const [ruleSetsMap, setRuleSetsMap] = useState<{ [key: string]: string }>({});
  const { userInfo } = useAuth();

  // update a single form field immutably
  function updateForm<F extends keyof HuntCreateFormState>(field: F, value: HuntCreateFormState[F]): void {
    setForm((prev) => ({ ...prev, [field]: value }));
  }

  // changing the hunt groups prunes the corpus groups down to the still-selected ones:
  // the corpus picker's options shrink but its chips would survive, and the API hard-400s
  // on a corpus group outside the hunt's groups
  function updateGroups(groups: string[]): void {
    setForm((prev) => ({ ...prev, groups, corpusGroups: prev.corpusGroups.filter((group) => groups.includes(group)) }));
  }

  // switching engines swaps the rule picker's options, so clear the engine-scoped picks
  // (explicit rules and rule set) — stale foreign picks would otherwise be submitted
  function updateEngine(engine: HuntEngine): void {
    setForm((prev) => (prev.engine === engine ? prev : { ...prev, engine, selectorRules: [], selectorRuleSet: '' }));
  }

  // the selector shapes the server only rejects opaquely are blocked before posting
  const validationError = validateHuntForm(form);
  // open the corpus panel when the copy flow prefilled bounds, so a narrowed re-run
  // window is visible before Create; derived from the initial state only, so React
  // never fights the user's own toggling afterwards
  const corpusPrefilled = describeCorpusBounds(initialState) !== '';
  // the live summary of the bounds the form would post, or '' when the defaults apply
  const corpusSummary = describeCorpusBounds(form);

  // the engine decides which rule kind the explicit picker lists; reload it on engine
  // change (an unknown engine can never crash the page; fall back to the default's kind)
  useEffect(() => {
    void getAvailableEntities([(HUNT_ENGINES[form.engine] ?? HUNT_ENGINES[BlankHuntCreateFormState.engine]).ruleKind], setRulesMap);
  }, [form.engine]);

  // load the rule set picker options once
  useEffect(() => {
    void getAvailableEntities([Entities.RuleSet], setRuleSetsMap);
  }, []);

  const handleCreateHunt = async (): Promise<void> => {
    // belt and braces: the button is disabled on a validation error, but never post one
    if (validationError !== '') {
      setError(validationError);
      return;
    }
    setCreating(true);
    // always re-enable the button: a throw anywhere in the build/post path must never
    // wedge the form (the error, if any, is already surfaced through setError)
    try {
      const hunt = await createHunt(buildHuntCreateRequest(form), setError);
      if (hunt) {
        void navigate(`${getDetailsBasePathByEntity(Entities.Hunt)}/${hunt.id}`);
      }
    } finally {
      setCreating(false);
    }
  };

  return (
    <Page className="full-min-width" title="Create Hunt">
      <CreateEntityTitle>
        <Title className="title">New Hunt</Title>
      </CreateEntityTitle>
      <Card className="panel">
        <Card.Body>
          <Row>
            <InfoHeader>
              Name<sub>*</sub>
            </InfoHeader>
            <InfoValue>
              <Form.Group>
                <Form.Control onChange={(e) => updateForm('name', String(e.target.value))} value={form.name} />
              </Form.Group>
            </InfoValue>
          </Row>
          <hr className="my-3" />
          <Row>
            <InfoHeader>
              Groups<sub>*</sub>
            </InfoHeader>
            <InfoValue>
              <SelectInputArray
                isCreatable={false}
                options={[...(userInfo?.groups ?? [])].sort((a, b) => a.localeCompare(b))}
                values={form.groups}
                onChange={(groups) => updateGroups(groups)}
              />
              <CreateOnlyNote>The hunt and everything it writes is bounded to these groups.</CreateOnlyNote>
            </InfoValue>
          </Row>
          <hr className="my-3" />
          <Row>
            <InfoHeader>Engine</InfoHeader>
            <InfoValue>
              <Form.Select value={form.engine} onChange={(e) => updateEngine(e.target.value as HuntEngine)}>
                {Object.values(HuntEngine).map((engine) => (
                  <option key={`hunt-engine-${engine}`} value={engine}>
                    {HUNT_ENGINES[engine].label}
                  </option>
                ))}
              </Form.Select>
            </InfoValue>
          </Row>
          <hr className="my-3" />
          <Row>
            <InfoHeader>
              Rules<sub>*</sub>
            </InfoHeader>
            <InfoValue>
              <Form.Select
                className="mb-2"
                value={form.selectorMode}
                onChange={(e) => updateForm('selectorMode', e.target.value as HuntSelectorMode)}
              >
                {Object.values(HuntSelectorMode).map((mode) => (
                  <option key={`hunt-selector-${mode}`} value={mode}>
                    {SELECTOR_MODE_LABELS[mode]}
                  </option>
                ))}
              </Form.Select>
              {form.selectorMode === HuntSelectorMode.Rules && (
                <SelectInputArray
                  isCreatable={false}
                  options={Object.keys(rulesMap)}
                  values={form.selectorRules}
                  onChange={(rules) => updateForm('selectorRules', rules)}
                  valuesMap={rulesMap}
                />
              )}
              {form.selectorMode === HuntSelectorMode.Tag && (
                <>
                  <Form.Control
                    className="mb-2"
                    placeholder="tag key"
                    value={form.selectorTagKey}
                    onChange={(e) => updateForm('selectorTagKey', String(e.target.value))}
                  />
                  <Form.Control
                    placeholder="tag value"
                    value={form.selectorTagValue}
                    onChange={(e) => updateForm('selectorTagValue', String(e.target.value))}
                  />
                </>
              )}
              {form.selectorMode === HuntSelectorMode.All && (
                <CreateOnlyNote>
                  Every {HUNT_ENGINES[form.engine]?.label ?? form.engine} rule you can see at creation joins the frozen snapshot.
                </CreateOnlyNote>
              )}
              {form.selectorMode === HuntSelectorMode.RuleSet && (
                <Form.Select value={form.selectorRuleSet} onChange={(e) => updateForm('selectorRuleSet', String(e.target.value))}>
                  <option value="">Select a rule set...</option>
                  {Object.entries(ruleSetsMap).map(([id, name]) => (
                    <option key={`hunt-ruleset-${id}`} value={id}>
                      {name}
                    </option>
                  ))}
                </Form.Select>
              )}
              <CreateOnlyNote>
                The selection freezes to a rule snapshot at creation; rules created later never join the hunt. Quarantined (Noisy/Slow)
                rules are excluded and counted.
              </CreateOnlyNote>
            </InfoValue>
          </Row>
          <hr className="my-3" />
          <Row>
            <InfoHeader>Corpus</InfoHeader>
            <InfoValue>
              <details open={corpusPrefilled}>
                <summary>
                  {/* the summary must stay truthful: claim the server defaults only while they apply */}
                  <small>
                    {corpusSummary === '' ? 'Bound the corpus (defaults to every file visible to you, up to creation)' : corpusSummary}
                  </small>
                </summary>
                <div className="mt-2">
                  <small>Corpus groups (defaults to the hunt&apos;s groups)</small>
                  <SelectInputArray
                    isCreatable={false}
                    options={[...form.groups].sort((a, b) => a.localeCompare(b))}
                    values={form.corpusGroups}
                    onChange={(groups) => updateForm('corpusGroups', groups)}
                  />
                  <div className="mt-2 d-flex align-items-center gap-2">
                    <small>Oldest</small>
                    <DatePicker
                      selected={form.corpusEnd}
                      disabled={false}
                      onChange={(date) => updateForm('corpusEnd', safeDateToStringConversion(date) ?? '')}
                    />
                    <small>Newest</small>
                    <DatePicker
                      selected={form.corpusStart}
                      disabled={false}
                      onChange={(date) => updateForm('corpusStart', safeDateToStringConversion(date) ?? '')}
                    />
                  </div>
                  <div className="mt-2">
                    <small>Tag filters (strict AND across every pair)</small>
                    <SelectableDictionary
                      entries={form.corpusTags}
                      setEntries={(entries: DictionaryEntry[]) => updateForm('corpusTags', entries)}
                      keyPlaceholder="tag key"
                      valuePlaceholder="tag value"
                      trim
                    />
                    <Form.Check
                      type="switch"
                      id="corpus-tags-case-insensitive"
                      label="Case-insensitive tag matching"
                      checked={form.corpusCaseInsensitive}
                      onChange={(e) => updateForm('corpusCaseInsensitive', e.target.checked)}
                    />
                  </div>
                </div>
              </details>
            </InfoValue>
          </Row>
          <hr className="my-3" />
          <Row>
            <InfoHeader>Description</InfoHeader>
            <InfoValue>
              <Form.Group>
                <Form.Control onChange={(e) => updateForm('description', String(e.target.value))} value={form.description} as="textarea" />
              </Form.Group>
            </InfoValue>
          </Row>
          <Row className="mt-4">
            <InfoHeader />
            <InfoValue>
              <CreateOnlyNote>
                * Field is required to create a new hunt. Server-configured defaults: 4 lanes per hunt, 10,000 rules per hunt, 2 active
                hunts per user, and 8 active hunts cluster-wide.
              </CreateOnlyNote>
            </InfoValue>
          </Row>
        </Card.Body>
      </Card>
      {error !== '' && <AlertBanner>{error}</AlertBanner>}
      <div className="d-flex justify-content-center pt-4">
        <OverlayTipBottom
          tip={
            validationError !== ''
              ? validationError
              : 'Create a new hunt. You must be a user, manager, or owner in a selected group to create this hunt.'
          }
        >
          <Button
            className="secondary-btn"
            variant="info"
            disabled={creating || validationError !== ''}
            onClick={() => {
              void handleCreateHunt();
            }}
          >
            Create
          </Button>
        </OverlayTipBottom>
      </div>
    </Page>
  );
};

export default HuntCreate;
