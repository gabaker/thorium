import React from 'react';
import styled from 'styled-components';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';

// project imports
import TagBadge from '../TagBadge';
import { filterIncludedTags, filterExcludedTags } from '../utilities';
import { Tags } from '@models/tags';
import { Entities } from '@models/entities';

// spec: ../tags.spec.md

const TagContainer = styled.div<{ $align: 'left' | 'center' }>`
  display: flex;
  flex-wrap: wrap;
  justify-content: ${({ $align }) => ($align === 'left' ? 'flex-start' : 'center')};

  /* left variant: let long tag values wrap (breaking anywhere) within a capped width instead of
     overflowing the row — scoped here so the global .tag-item style and other views are untouched */
  ${({ $align }) =>
    $align === 'left' &&
    `
    & .tag-item {
      max-width: 400px;
      white-space: normal;
      word-break: break-all;
      text-align: left;
    }
  `}
`;

interface CondensedEntityTagProps {
  tags: Tags; // tags to display in condensed non-editable view
  resource?: Entities;
  align?: 'left' | 'center'; // tag alignment (default centered)
}

const CondensedEntityTags: React.FC<CondensedEntityTagProps> = ({ tags, resource, align = 'center' }) => {
  const excludeTags: string[] = [];
  const generalTags = filterExcludedTags(tags, excludeTags);
  const tlpTags = filterIncludedTags(tags, ['TLP']);
  const tagsCount = Object.keys(tags).length;
  return (
    <>
      {tagsCount == 0 && (
        <div className="px-3 py-2">
          <AlertBanner severity={Severity.Info} className="ms-4 me-4">
            No Tags Found
          </AlertBanner>
        </div>
      )}
      <TagContainer $align={align}>
        {Object.keys(tlpTags).length > 0 &&
          Object.keys(tlpTags)
            .sort()
            .map((tagKey) =>
              Object.keys(tlpTags[tagKey])
                .sort()
                .map((tagValue) => (
                  <TagBadge resource={resource} key={'TLP_' + tagValue} tag={tagKey} value={tagValue} condensed={true} action={'link'} />
                )),
            )}
        {Object.keys(generalTags)
          .sort()
          .map((tagKey) =>
            Object.keys(generalTags[tagKey])
              .sort()
              .map((tagValue) => (
                <TagBadge resource={resource} key={'General_' + tagValue} tag={tagKey} value={tagValue} condensed={true} action={'link'} />
              )),
          )}
      </TagContainer>
    </>
  );
};

export default CondensedEntityTags;
