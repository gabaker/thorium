import React, { useMemo } from 'react';

import { useIncidentData } from './IncidentDataProvider';
import {
  Tile,
  TileHeader,
  TileBody,
  BarContainer,
  BarRow,
  BarLabel,
  BarTrack,
  BarFill,
  BarCount,
  EmptyState,
  LoadingContainer,
  Spinner,
  StatRow,
  StatCard,
  StatValue,
  StatLabel,
  SectionDivider,
} from './styles';

const TYPE_COLORS: Record<string, string> = {
  files: '#f1d592',
  repos: '#f03c2e',
  tags: '#427d8c',
  Device: '#ed9624',
  Vendor: '#8f30b8',
  Collection: '#6a5acd',
  FileSystem: '#4a9d8f',
  Folder: '#c9a85c',
  Incident: '#e74c3c',
  Other: '#cacfca',
};

function colorFor(label: string): string {
  return TYPE_COLORS[label] ?? '#7a8a9a';
}

interface BreakdownEntry {
  label: string;
  count: number;
}

const EntityBreakdownPanel: React.FC = () => {
  const { nodeTypeCounts, fileExtensions, totalNodes, loading } = useIncidentData();

  const breakdown = useMemo<BreakdownEntry[]>(() => {
    const items: BreakdownEntry[] = [];
    if (nodeTypeCounts.files > 0) items.push({ label: 'files', count: nodeTypeCounts.files });
    if (nodeTypeCounts.repos > 0) items.push({ label: 'repos', count: nodeTypeCounts.repos });
    if (nodeTypeCounts.tags > 0) items.push({ label: 'tags', count: nodeTypeCounts.tags });
    nodeTypeCounts.entities.forEach((count, kind) => {
      items.push({ label: kind, count });
    });
    items.sort((a, b) => b.count - a.count);
    return items;
  }, [nodeTypeCounts]);

  const maxCount = useMemo(() => Math.max(1, ...breakdown.map((e) => e.count)), [breakdown]);

  const maxExtCount = useMemo(() => Math.max(1, ...fileExtensions.map((e) => e.count)), [fileExtensions]);

  if (loading && totalNodes === 0) {
    return (
      <Tile>
        <TileHeader>Breakdown</TileHeader>
        <LoadingContainer>
          <Spinner />
          Analyzing graph...
        </LoadingContainer>
      </Tile>
    );
  }

  if (totalNodes === 0) {
    return (
      <Tile>
        <TileHeader>Breakdown</TileHeader>
        <EmptyState>No data available</EmptyState>
      </Tile>
    );
  }

  return (
    <Tile>
      <TileHeader>Breakdown</TileHeader>
      <TileBody>
        <StatRow>
          <StatCard>
            <StatValue>{totalNodes}</StatValue>
            <StatLabel>Total Nodes</StatLabel>
          </StatCard>
          <StatCard>
            <StatValue>{nodeTypeCounts.files}</StatValue>
            <StatLabel>Files</StatLabel>
          </StatCard>
          <StatCard>
            <StatValue>{nodeTypeCounts.entities.size}</StatValue>
            <StatLabel>Entity Types</StatLabel>
          </StatCard>
        </StatRow>

        <SectionDivider>Node Types</SectionDivider>
        <BarContainer>
          {breakdown.map((entry) => (
            <BarRow key={entry.label}>
              <BarLabel>{entry.label}</BarLabel>
              <BarTrack>
                <BarFill $width={(entry.count / maxCount) * 100} $color={colorFor(entry.label)} />
              </BarTrack>
              <BarCount>{entry.count}</BarCount>
            </BarRow>
          ))}
        </BarContainer>

        {fileExtensions.length > 0 ? (
          <>
            <SectionDivider>File Extensions</SectionDivider>
            <BarContainer>
              {fileExtensions.slice(0, 10).map((ext) => (
                <BarRow key={ext.extension}>
                  <BarLabel>{ext.extension}</BarLabel>
                  <BarTrack>
                    <BarFill $width={(ext.count / maxExtCount) * 100} $color="#5b9bd5" />
                  </BarTrack>
                  <BarCount>{ext.count}</BarCount>
                </BarRow>
              ))}
            </BarContainer>
          </>
        ) : null}
      </TileBody>
    </Tile>
  );
};

export default EntityBreakdownPanel;
