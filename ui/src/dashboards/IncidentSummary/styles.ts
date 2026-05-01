import styled from 'styled-components';
import { scaling } from '@styles';

export const DashboardContainer = styled.div`
  display: flex;
  flex-direction: column;
  gap: 1rem;
  padding-bottom: 2rem;
`;

export const DashboardHeader = styled.div`
  display: flex;
  align-items: baseline;
  gap: 1rem;
  flex-wrap: wrap;
`;

export const DashboardTitle = styled.h2`
  color: var(--thorium-text);
  margin: 0;
  font-size: 1.5rem;
  font-weight: 600;
`;

export const TagBadge = styled.span`
  display: inline-flex;
  align-items: center;
  gap: 0.25rem;
  padding: 0.2rem 0.6rem;
  border-radius: 4px;
  font-size: 0.8rem;
  font-family: monospace;
  background: var(--thorium-secondary-panel-bg, #2a2d35);
  color: var(--thorium-secondary-text, #aab);
  border: 1px solid var(--thorium-panel-border, #3a3d45);
`;

export const DashboardGrid = styled.div`
  display: grid;
  grid-template-columns: 1fr 1fr;
  grid-template-rows: auto auto;
  gap: 1rem;

  @media (max-width: ${scaling.lg}) {
    grid-template-columns: 1fr;
  }
`;

export const Tile = styled.div`
  background: var(--thorium-panel-bg);
  border: 1px solid var(--thorium-panel-border, #3a3d45);
  border-radius: 8px;
  overflow: hidden;
  display: flex;
  flex-direction: column;
`;

export const GraphTile = styled(Tile)`
  grid-column: 1 / -1;
  min-height: 500px;
`;

export const TileHeader = styled.div`
  padding: 0.75rem 1rem;
  font-size: 0.9rem;
  font-weight: 600;
  color: var(--thorium-text);
  border-bottom: 1px solid var(--thorium-panel-border, #3a3d45);
  background: var(--thorium-secondary-panel-bg, #2a2d35);
`;

export const TileBody = styled.div`
  padding: 0.75rem 1rem;
  flex: 1;
  overflow-y: auto;
`;

export const ScrollableBody = styled(TileBody)`
  max-height: 400px;
`;

export const Table = styled.table`
  width: 100%;
  border-collapse: collapse;
  font-size: 0.85rem;
`;

export const Th = styled.th`
  text-align: left;
  padding: 0.4rem 0.6rem;
  color: var(--thorium-secondary-text, #aab);
  border-bottom: 1px solid var(--thorium-panel-border, #3a3d45);
  font-weight: 500;
  white-space: nowrap;
`;

export const Td = styled.td`
  padding: 0.4rem 0.6rem;
  color: var(--thorium-text);
  border-bottom: 1px solid color-mix(in srgb, var(--thorium-panel-border, #3a3d45) 50%, transparent);
  vertical-align: middle;
`;

export const TrClickable = styled.tr`
  cursor: pointer;
  &:hover {
    background: var(--thorium-highlight-panel-bg, #363940);
  }
`;

export const Mono = styled.span`
  font-family: monospace;
  font-size: 0.8rem;
`;

export const BarContainer = styled.div`
  display: flex;
  flex-direction: column;
  gap: 0.5rem;
  padding: 0.25rem 0;
`;

export const BarRow = styled.div`
  display: flex;
  align-items: center;
  gap: 0.75rem;
`;

export const BarLabel = styled.span`
  min-width: 90px;
  font-size: 0.8rem;
  color: var(--thorium-text);
  text-align: right;
  text-transform: capitalize;
`;

export const BarTrack = styled.div`
  flex: 1;
  height: 20px;
  background: var(--thorium-secondary-panel-bg, #2a2d35);
  border-radius: 3px;
  overflow: hidden;
  position: relative;
`;

export const BarFill = styled.div<{ $width: number; $color: string }>`
  height: 100%;
  width: ${(p) => p.$width}%;
  background: ${(p) => p.$color};
  border-radius: 3px;
  transition: width 0.4s ease;
  min-width: ${(p) => (p.$width > 0 ? '2px' : '0')};
`;

export const BarCount = styled.span`
  min-width: 32px;
  font-size: 0.8rem;
  color: var(--thorium-secondary-text, #aab);
  font-variant-numeric: tabular-nums;
`;

export const EmptyState = styled.div`
  display: flex;
  align-items: center;
  justify-content: center;
  min-height: 120px;
  color: var(--thorium-secondary-text, #aab);
  font-size: 0.9rem;
`;

export const LoadingContainer = styled.div`
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 0.5rem;
  min-height: 120px;
  color: var(--thorium-secondary-text, #aab);
  font-size: 0.85rem;
`;

export const Spinner = styled.div`
  width: 18px;
  height: 18px;
  border: 2px solid var(--thorium-panel-border, #3a3d45);
  border-top-color: var(--thorium-text, #eee);
  border-radius: 50%;
  animation: spin 0.8s linear infinite;

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
`;

export const StatRow = styled.div`
  display: flex;
  gap: 1rem;
  flex-wrap: wrap;
`;

export const StatCard = styled.div`
  flex: 1;
  min-width: 100px;
  padding: 0.6rem 0.8rem;
  background: var(--thorium-secondary-panel-bg, #2a2d35);
  border-radius: 6px;
  text-align: center;
`;

export const StatValue = styled.div`
  font-size: 1.5rem;
  font-weight: 700;
  color: var(--thorium-text);
  font-variant-numeric: tabular-nums;
`;

export const StatLabel = styled.div`
  font-size: 0.75rem;
  color: var(--thorium-secondary-text, #aab);
  text-transform: uppercase;
  letter-spacing: 0.05em;
`;

export const SectionDivider = styled.div`
  padding: 0.75rem 1rem;
  margin: 1rem -1rem 0.5rem;
  font-size: 0.9rem;
  font-weight: 600;
  color: var(--thorium-text);
  border-top: 1px solid var(--thorium-panel-border, #3a3d45);
  border-bottom: 1px solid var(--thorium-panel-border, #3a3d45);
  background: var(--thorium-secondary-panel-bg, #2a2d35);
`;
