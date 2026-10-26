import { useCallback, useEffect, useRef, useState } from 'react';
import type { FC } from 'react';
import styled from 'styled-components';

// project imports
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import { Badge } from '@components/shared/badges/Badge';
import ImageAccordionItem from '@components/pages/images/ImageAccordionItem';
import Page from '@components/pages/Page';
import { OmnibarImages } from '@components/shared/inputs/omnibar/Bars';
import type { Clause } from '@components/shared/inputs/omnibar/ClauseTypes';
import { defaultTimeSelection } from '@components/shared/inputs/omnibar/timepicker/utils';
import { useOmnibarUrlState } from '@components/shared/inputs/omnibar/useOmnibarUrlState';
import { getGroupsFromClauses, getSearchTextFromClauses, matchesStringClauses } from '@components/shared/inputs/omnibar/utils';
import ListErrorBanner from '@components/shared/alerts/ListErrorBanner';
import NoResultsBanner from '@components/shared/alerts/NoResultsBanner';
import Title from '@components/shared/titles/Title';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import { OverlayTipRight } from '@components/shared/overlay/tips';
import { useAuth } from '@utilities/auth';
import { getThoriumRole } from '@utilities/role';
import { fetchImages, fetchGroups } from '@utilities/fetch';
import type { Image } from '@models/images';
import type { Group } from '@models/groups';
import { RoleKey } from '@models/users';
import { scrollToSection } from '@utilities/interactions';
import { Accordion } from '@components/shared/accordion';
import { HeaderBar, NoteRow, OmnibarRow } from '@pages/pipelines/PipelineBrowsing.styled';

/** The header with the title centred on the page: the count on the left and an empty cell of the same share on the right. */
const CentredHeaderBar = styled(HeaderBar)`
  display: grid;
  grid-template-columns: 1fr auto 1fr;
`;

/** Filter images client-side by the omnibar clauses (group, creator, name, free text). */
const filterImages = (images: Image[], clauses: Clause[]): Image[] => {
  const groups = getGroupsFromClauses(clauses);
  const text = getSearchTextFromClauses(clauses);

  return images.filter((image) => {
    const groupFilter = groups.length > 0 ? groups.includes(image.group) : true;
    const creatorFilter = matchesStringClauses(clauses, 'creator', image.creator);
    const nameFilter = matchesStringClauses(clauses, 'name', image.name);
    const textFilter = image.name.includes(text) || (image.description?.includes(text) ?? false);
    return groupFilter && creatorFilter && nameFilter && textFilter;
  });
};

const ImageBrowsing: FC = () => {
  const [loading, setLoading] = useState(false);
  const [images, setImages] = useState<Image[]>([]);
  const [groups, setGroups] = useState<Record<string, Group>>({});
  // the messages from image list reads that failed, so an unreadable list is not shown as an empty one
  const [listErrors, setListErrors] = useState<string[]>([]);
  // expanded accordion rows + omnibar filters live in the URL so the view is shareable
  const { clauses, setClauses } = useOmnibarUrlState({ clauses: [], time: defaultTimeSelection() });
  const { userInfo, checkCookie } = useAuth();
  const cancelUpdateRef = useRef(false);
  const [activeKeys, setActiveKeys] = useState<string[]>([]);

  const filteredImages = filterImages(images, clauses);

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

  useEffect(() => {
    void fetchGroups(setGroups as (groups: { [name: string]: Group } | Group[] | string[]) => void, null as never, true);
  }, []);

  useEffect(() => {
    cancelUpdateRef.current = false;
    if (groups && Object.keys(groups).length) {
      void fetchImageData();
    }
    return () => {
      cancelUpdateRef.current = true;
    };
  }, [groups]);

  // Record a failed list read for display and re-check the session.
  const onListError = useCallback(
    (message: string) => {
      setListErrors((prev) => (prev.includes(message) ? prev : [...prev, message]));
      void checkCookie();
    },
    [checkCookie],
  );

  const fetchImageData = async () => {
    setListErrors([]);
    await fetchImages(Object.keys(groups), setImages, cancelUpdateRef.current, onListError, setLoading, true);
    // need to wait 1 second because the page takes so long to load
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

  // Full list reload (with the loading spinner) used after a delete removes an image.
  const reloadImages = useCallback(() => {
    if (Object.keys(groups).length) {
      setListErrors([]);
      void fetchImages(Object.keys(groups), setImages, false, onListError, setLoading, true);
    }
  }, [groups, onListError]);

  // a failed read with nothing loaded has no count to show; 0 would claim the list is empty
  const listFailed = listErrors.length > 0 && images.length === 0;

  const imageCountTip =
    userInfo && getThoriumRole(userInfo.role) == RoleKey.Admin
      ? `There are a total of ${images.length} Thorium images.`
      : `There are a total of ${images.length} Thorium images owned by your groups.`;

  return (
    <Page title="Images · Thorium">
      <CentredHeaderBar>
        <div>
          <h2>
            {!listFailed && (
              <OverlayTipRight tip={imageCountTip}>
                <Badge count data-testid="image-count">
                  {images.length}
                </Badge>
              </OverlayTipRight>
            )}
          </h2>
        </div>
        <Title>Images</Title>
        <div />
      </CentredHeaderBar>
      <NoteRow>
        <AlertBanner severity={Severity.Info}>
          <span data-testid="images-note">
            Images are legacy: they can be viewed and deleted but no longer created or changed. Tools replace them.
          </span>
        </AlertBanner>
      </NoteRow>
      <OmnibarRow>
        <OmnibarImages clauses={clauses} setClauses={setClauses} images={images} />
      </OmnibarRow>
      <ListErrorBanner errors={listErrors} />
      <LoadingSpinner loading={loading}></LoadingSpinner>
      {!loading && listErrors.length === 0 && filteredImages.length === 0 && <NoResultsBanner type="Images" />}
      <Accordion alwaysOpen activeKey={activeKeys} onSelect={handleAccordionSelect}>
        {filteredImages.map((image) => (
          <ImageAccordionItem
            key={`${image.name}_${image.group}`}
            image={image}
            images={images}
            groups={groups}
            setImages={setImages}
            onRefresh={reloadImages}
            onExpand={expandAccordionKey}
          />
        ))}
      </Accordion>
    </Page>
  );
};

export default ImageBrowsing;
