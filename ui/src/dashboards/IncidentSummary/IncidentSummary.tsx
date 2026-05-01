import React, { useMemo } from 'react';
import { useSearchParams } from 'react-router-dom';

import { GraphDataProvider } from '@components/associations/data/GraphDataContext';
import Page from '@components/pages/Page';
import type { Seed } from '@models/trees';
import { IncidentDataProvider } from './IncidentDataProvider';
import FileListPanel from './FileListPanel';
import EntityBreakdownPanel from './EntityBreakdownPanel';
import IncidentGraphTile from './IncidentGraphTile';
import type { IncidentTag, IncidentSummaryProps } from './types';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { DashboardContainer, DashboardHeader, DashboardTitle, TagBadge, DashboardGrid } from './styles';

interface InnerProps {
  incidentTag: IncidentTag;
}

const IncidentDashboardInner: React.FC<InnerProps> = ({ incidentTag }) => {
  const seed = useMemo<Seed>(() => ({ tags: { [incidentTag.key]: [incidentTag.value] } }), [incidentTag.key, incidentTag.value]);

  return (
    <GraphDataProvider initial={seed} depth={2}>
      <IncidentDataProvider>
        <DashboardContainer>
          <DashboardHeader>
            <DashboardTitle>Incident Summary</DashboardTitle>
            <TagBadge>
              {incidentTag.key}: {incidentTag.value}
            </TagBadge>
          </DashboardHeader>

          <DashboardGrid>
            <FileListPanel />
            <EntityBreakdownPanel />
            <IncidentGraphTile />
          </DashboardGrid>
        </DashboardContainer>
      </IncidentDataProvider>
    </GraphDataProvider>
  );
};

const IncidentSummary: React.FC<Partial<IncidentSummaryProps>> = (props) => {
  const [searchParams] = useSearchParams();

  const incidentTag = useMemo<IncidentTag | null>(() => {
    if (props.incidentTag) return props.incidentTag;
    const key = searchParams.get('tag_key');
    const value = searchParams.get('tag_value');
    if (key && value) return { key, value };
    return null;
  }, [props.incidentTag, searchParams]);

  return (
    <Page title="Incident Summary" className="full-min-width">
      {incidentTag ? (
        <IncidentDashboardInner incidentTag={incidentTag} />
      ) : (
        <AlertBanner severity={Severity.Warning}>
          Missing incident tag. Provide <code>tag_key</code> and <code>tag_value</code> query parameters, e.g.{' '}
          <code>/dashboard/incident?tag_key=incident/id&amp;tag_value=INC-2024-001</code>
        </AlertBanner>
      )}
    </Page>
  );
};

export default IncidentSummary;
