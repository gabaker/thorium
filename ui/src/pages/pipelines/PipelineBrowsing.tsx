import { useCallback, useEffect, useState } from 'react';
import type { FC } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';

// project imports
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Badge } from '@components/shared/badges/Badge';
import { HeaderBar, NoteRow, OmnibarRow } from './PipelineBrowsing.styled';
import PipelineAccordionItem from '@components/pages/pipelines/PipelineAccordionItem';
import { orderComparePipeline } from '@components/pages/files/reactions/pipelines';
import Page from '@components/pages/Page';
import { Accordion } from '@components/shared/accordion';
import { Button, ButtonVariant } from '@components/shared/buttons';
import { OmnibarPipelines } from '@components/shared/inputs/omnibar/Bars';
import type { Clause } from '@components/shared/inputs/omnibar/ClauseTypes';
import { defaultTimeSelection } from '@components/shared/inputs/omnibar/timepicker/utils';
import { useOmnibarUrlState } from '@components/shared/inputs/omnibar/useOmnibarUrlState';
import { getGroupsFromClauses, matchesStringClauses } from '@components/shared/inputs/omnibar/utils';
import ListErrorBanner from '@components/shared/alerts/ListErrorBanner';
import NoResultsBanner from '@components/shared/alerts/NoResultsBanner';
import Title from '@components/shared/titles/Title';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import { OverlayTipRight, OverlayTipBottom } from '@components/shared/overlay/tips';
import { getPipeline, listPipelines, updatePipeline } from '@thorpi/pipelines';
import { useAuth } from '@utilities/auth';
import { fetchGroups } from '@utilities/fetch';
import { generateCopyName } from '@utilities/naming';
import { getThoriumRole } from '@utilities/role';
import { editorObjectToPipelineUpdate } from '@utilities/transforms/pipeline';
import type { Group } from '@models/groups';
import type { Pipeline, PipelineUpdate } from '@models/pipelines';
import { RoleKey } from '@models/users';
import { scrollToSection } from '@utilities/interactions';

/** Filter pipelines client-side by the omnibar clauses (name, creator, group). */
const filterPipelines = (pipelines: Pipeline[], clauses: Clause[]): Pipeline[] => {
  const groups = getGroupsFromClauses(clauses);

  return pipelines.filter((pipeline) => {
    const nameFilter = matchesStringClauses(clauses, 'name', pipeline.name);
    const creatorFilter = matchesStringClauses(clauses, 'creator', pipeline.creator);
    const groupFilter = groups.length > 0 ? groups.includes(pipeline.group) : true;
    return nameFilter && creatorFilter && groupFilter;
  });
};

const PipelineBrowsing: FC = () => {
  const navigate = useNavigate();
  const [loading, setLoading] = useState(false);
  const [pipelines, setPipelines] = useState<Pipeline[]>([]);
  const [groups, setGroups] = useState<Record<string, Group>>({});
  // the messages from pipeline list reads that failed, so an unreadable list is not shown as an empty one
  const [listErrors, setListErrors] = useState<string[]>([]);
  // expanded accordion rows + omnibar filters live in the URL so the view is shareable
  // const [activeKeys, setActiveKeys] = useUrlState(listCodec('open'), []);
  const { clauses, setClauses } = useOmnibarUrlState({ clauses: [], time: defaultTimeSelection() });
  const { userInfo, checkCookie } = useAuth();

  const [activeKeys, setActiveKeys] = useState<string[]>([]);
  const location = useLocation();

  const filteredPipelines = filterPipelines(pipelines, clauses);

  const handleAccordionSelect = useCallback((eventKey: string | string[] | null | undefined) => {
    if (eventKey === null || eventKey === undefined) {
      setActiveKeys([]);
      return;
    }
    setActiveKeys(Array.isArray(eventKey) ? [...eventKey] : [eventKey]);
  }, []);

  const expandAccordionKey = useCallback((key: string) => {
    setActiveKeys((prev) => (prev.includes(key) ? prev : [...prev, key]));
  }, []);

  const fetchPipelinesData = async () => {
    setLoading(true);
    // each failed group read is recorded for display and still re-checks the session
    const failures: string[] = [];
    const onListError = (message: string) => {
      failures.push(message);
      void checkCookie();
    };
    const results = await Promise.all(Object.keys(groups).map((group) => listPipelines(group, onListError, true, null, 1000)));
    setListErrors(failures);
    setPipelines(results.filter(Boolean).flat() as Pipeline[]);
    setLoading(false);
    setTimeout(scrollOnLoad, 1000);
  };

  const scrollOnLoad = () => {
    const hash = location.hash;
    if (hash.length <= 1) return;

    const scroll_id = hash.slice(1);
    if (scroll_id != '') {
      scrollToSection(scroll_id);
      handleAccordionSelect(scroll_id);
    }
  };

  // Refetch a single pipeline and replace just that entry in place, so an edit re-renders only
  // the changed pipeline without reloading the whole list or collapsing the open accordion.
  const refreshPipeline = useCallback(async (group: string, name: string) => {
    const fresh = await getPipeline(group, name, () => {});
    if (fresh) {
      setPipelines((prev) => prev.map((p) => (p.group === group && p.name === name ? fresh : p)));
    }
  }, []);

  useEffect(() => {
    void fetchGroups(setGroups as (groups: { [name: string]: Group } | Group[] | string[]) => void, () => {}, true);
  }, []);

  useEffect(() => {
    void fetchPipelinesData();
  }, [groups]);

  async function handlePipelineUpdate(
    editorObj: Record<string, unknown>,
    originalPipeline: Pipeline,
    setUpdateError: (e: string) => void,
  ): Promise<boolean> {
    const result = editorObjectToPipelineUpdate(editorObj, originalPipeline);
    if (!result) {
      setUpdateError('Invalid pipeline data');
      return false;
    }
    if (await updatePipeline(result.group, result.name, result.data as PipelineUpdate, setUpdateError)) {
      // Refresh only the edited pipeline so the open accordion stays put and only its content rerenders.
      void refreshPipeline(originalPipeline.group, originalPipeline.name);
      return true;
    }
    return false;
  }

  const canCreatePipeline = userInfo
    ? ([RoleKey.Developer, RoleKey.Analyst, RoleKey.Admin] as string[]).includes(getThoriumRole(userInfo.role))
    : false;

  const handleOpenCreate = useCallback(() => {
    void navigate('/create/pipeline');
  }, [navigate]);

  // Copy navigates to the create page seeded with the source pipeline and a fresh, unique name.
  const handleCopyPipeline = useCallback(
    (pipeline: Pipeline) => {
      const copyName = generateCopyName(
        pipeline.name,
        pipelines.map((p) => p.name),
      );
      void navigate('/create/pipeline', { state: { ...pipeline, name: copyName } });
    },
    [navigate, pipelines],
  );

  // a failed read with nothing loaded has no count to show; 0 would claim the list is empty
  const listFailed = listErrors.length > 0 && pipelines.length === 0;

  const pipelineCountTip =
    userInfo && getThoriumRole(userInfo.role) == RoleKey.Admin
      ? `There are a total of ${pipelines.length} Thorium pipelines.`
      : `There are a total of ${pipelines.length} Thorium pipelines owned by your groups.`;

  const createPipelineTip = canCreatePipeline
    ? `Create a new pipeline. You must be a
      Thorium developer, analyst, or admin to create a pipeline.`
    : `You must be a Thorium developer or
      admin to create a pipeline.`;

  return (
    <Page title="Pipelines · Thorium">
      <HeaderBar>
        <div>
          <h2>
            {!listFailed && (
              <OverlayTipRight tip={pipelineCountTip}>
                <Badge count data-testid="pipeline-count">
                  {pipelines.length}
                </Badge>
              </OverlayTipRight>
            )}
          </h2>
        </div>
        <Title>Pipelines</Title>
        <div>
          <OverlayTipBottom tip={createPipelineTip}>
            <Button
              className="ok-btn m-1"
              variant={ButtonVariant.Ok}
              data-testid="create-pipeline-btn"
              onClick={handleOpenCreate}
              disabled={!canCreatePipeline}
            >
              <b>+</b>
            </Button>
          </OverlayTipBottom>
        </div>
      </HeaderBar>
      <NoteRow>
        <AlertBanner severity={Severity.Info}>
          <span data-testid="pipelines-note">
            Pipeline stages run tools. A stage marked <b>legacy</b> still names an image that no tool backs and cannot be run until a tool
            is chosen for it.
          </span>
        </AlertBanner>
      </NoteRow>
      <OmnibarRow>
        <OmnibarPipelines clauses={clauses} setClauses={setClauses} pipelines={pipelines} />
      </OmnibarRow>
      <ListErrorBanner errors={listErrors} />
      <LoadingSpinner loading={loading}></LoadingSpinner>
      {!loading && listErrors.length === 0 && filteredPipelines.length === 0 && <NoResultsBanner type="Pipelines" />}
      <Accordion alwaysOpen activeKey={activeKeys} onSelect={handleAccordionSelect}>
        {filteredPipelines
          .sort((a, b) => orderComparePipeline(a, b))
          .map((pipeline) => (
            <PipelineAccordionItem
              key={`${pipeline.name}_${pipeline.group}`}
              pipeline={pipeline}
              groups={groups}
              canCreatePipeline={canCreatePipeline}
              onUpdate={handlePipelineUpdate}
              onRefresh={() => void fetchPipelinesData()}
              refreshPipeline={(group, name) => void refreshPipeline(group, name)}
              onExpand={expandAccordionKey}
              onCopy={handleCopyPipeline}
            />
          ))}
      </Accordion>
    </Page>
  );
};

export default PipelineBrowsing;
