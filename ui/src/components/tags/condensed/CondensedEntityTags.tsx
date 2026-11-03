import React from 'react';
import styled from 'styled-components';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';

// project imports
import TagBadge from '../TagBadge';
import { filterIncludedTags, filterExcludedTags } from '../utilities';
import { Tags } from '@models/tags';
import { Entities } from '@models/entities';

/** How a condensed tag list aligns its badges within the row. */
export enum TagAlign {
  Left = 'left',
  Center = 'center',
}

const TagContainer = styled.div<{ $align: TagAlign }>`
  display: flex;
  flex-wrap: wrap;
  justify-content: ${({ $align }) => ($align === TagAlign.Left ? 'flex-start' : 'center')};

  /* left variant only: cap the badge width and left-align text so long values wrap within a column
     instead of stretching the row (the global .tag-item rule already breaks long words) */
  ${({ $align }) =>
    $align === TagAlign.Left &&
    `
    & .tag-item {
      max-width: 400px;
      white-space: normal;
      text-align: left;
    }
  `}
`;

interface CondensedEntityTagProps {
  tags: Tags;
  resource?: Entities;
  align?: TagAlign;
  // tag keys to leave out of the list, compared without regard to case
  excludeKeys?: string[];
  // whether the empty-state banner spans the full width of its container rather than being inset
  fullWidthEmpty?: boolean;
}

const CondensedEntityTags: React.FC<CondensedEntityTagProps> = ({
  tags,
  resource,
  align = TagAlign.Center,
  excludeKeys = [],
  fullWidthEmpty = false,
}) => {
  const generalTags = filterExcludedTags(tags, excludeKeys);
  const tlpTags = filterIncludedTags(generalTags, ['TLP']);
  // the count covers only the tags that can be shown, so a list of only excluded tags reads as empty
  const tagsCount = Object.keys(generalTags).length;
  return (
    <>
      {tagsCount == 0 && (
        <div className={fullWidthEmpty ? 'py-2' : 'px-3 py-2'}>
          <AlertBanner severity={Severity.Info} className={fullWidthEmpty ? '' : 'ms-4 me-4'}>
            {fullWidthEmpty ? 'No tags found.' : 'No Tags Found'}
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
                  <TagBadge
                    resource={resource}
                    key={'TLP_' + tagKey + '_' + tagValue}
                    tag={tagKey}
                    value={tagValue}
                    condensed={true}
                    action={'link'}
                  />
                )),
            )}
        {Object.keys(generalTags)
          .sort()
          .map((tagKey) =>
            Object.keys(generalTags[tagKey])
              .sort()
              .map((tagValue) => (
                <TagBadge
                  resource={resource}
                  key={'General_' + tagKey + '_' + tagValue}
                  tag={tagKey}
                  value={tagValue}
                  condensed={true}
                  action={'link'}
                />
              )),
          )}
      </TagContainer>
    </>
  );
};

export default CondensedEntityTags;
