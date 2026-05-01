import React, { Suspense } from 'react';
import { useInView } from 'react-intersection-observer';

import { GraphTile, TileHeader, LoadingContainer, Spinner } from './styles';

const AssociationGraph = React.lazy(() => import('@components/associations/graph/AssociationGraph'));

const GraphFallback = (
  <LoadingContainer>
    <Spinner />
    Loading graph...
  </LoadingContainer>
);

const IncidentGraphTile: React.FC = () => {
  const { ref, inView } = useInView({ triggerOnce: true, threshold: 0.1 });

  return (
    <GraphTile ref={ref}>
      <TileHeader>Association Graph</TileHeader>
      <Suspense fallback={GraphFallback}>
        <AssociationGraph inView={inView} />
      </Suspense>
    </GraphTile>
  );
};

export default IncidentGraphTile;
