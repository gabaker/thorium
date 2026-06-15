// spec: ./EntityBrowser.spec.md
import React, { useState } from 'react';
import { FaAngleDown, FaAngleUp } from 'react-icons/fa6';

// project imports
import { MetadataContent, MetadataSection, MetadataToggleRow } from './EntityBrowser.styled';
import { ExpandToggle } from '@components/shared/buttons/ExpandToggle';
import EntitySummary, { SummaryVariant } from '@components/shared/info/EntitySummary';
import { InfoModel, SummaryPart } from '@components/shared/info/info';

interface MetadataBoxProps {
  model: InfoModel;
}

/**
 * Condensed metadata affordance under a row's header: a single "details" up/down caret (no preview peek),
 * collapsed by default with minimal vertical footprint. Expanding reveals the node's full metadata via the
 * shared {@link EntitySummary} (kind/title omitted — the header already shows the name; the duplicate marker
 * lives on the header, so it's suppressed here too).
 */
const MetadataBox: React.FC<MetadataBoxProps> = ({ model }) => {
  const [expanded, setExpanded] = useState(false);
  return (
    <MetadataSection>
      <MetadataToggleRow>
        <ExpandToggle data-testid="entity-details-toggle" aria-expanded={expanded} onClick={() => setExpanded((e) => !e)}>
          {expanded ? <FaAngleUp /> : <FaAngleDown />} details
        </ExpandToggle>
      </MetadataToggleRow>
      {expanded && (
        <MetadataContent>
          <EntitySummary model={model} variant={SummaryVariant.Compact} exclude={[SummaryPart.Kind, SummaryPart.Title]} />
        </MetadataContent>
      )}
    </MetadataSection>
  );
};

export default MetadataBox;
