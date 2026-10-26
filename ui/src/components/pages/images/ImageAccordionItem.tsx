import { useRef, useState } from 'react';
import type { FC } from 'react';

// project imports
import { Badge, BadgeTone } from '@components/agentic/shared';
import ImageInfo from './ImageInfo';
import type { ImageInfoHandle } from './ImageInfo';
import { BanWarningIcon, DeleteConfirmModal, HeaderActions, HeaderBtn } from '@components/shared/browsing';
import { ButtonVariant } from '@components/shared/buttons';
import { OverlayTipBottom, OverlayTipLeft, OverlayTipRight } from '@components/shared/overlay/tips';
import { deleteImage } from '@thorpi/images';
import { useAuth } from '@utilities/auth';
import { canDeleteImage } from '@utilities/permissions';
import type { Group } from '@models/groups';
import type { Image } from '@models/images';
import { updateURLSection } from '@utilities/url';
import { toast } from 'react-toastify';
import { FaLink } from 'react-icons/fa';
import { AccordionItem, AccordionHeader, AccordionBody } from '@components/shared/accordion/Accordion';

interface ImageAccordionItemProps {
  image: Image;
  images: Image[];
  groups: Record<string, Group>;
  setImages: (images: Image[]) => void;
  // Full list reload (with spinner) used after this image is deleted.
  onRefresh: () => void;
  onExpand: (key: string) => void;
}

const ImageAccordionItem: FC<ImageAccordionItemProps> = ({ image, images, groups, setImages, onRefresh, onExpand }) => {
  const { userInfo } = useAuth();
  const [showDeleteModal, setShowDeleteModal] = useState(false);
  const [deleteError, setDeleteError] = useState('');
  const imageInfoRef = useRef<ImageInfoHandle>(null);

  // gate edit/delete/create against the backend image authorization predicates
  const group = groups[image.group];
  const userCanDelete = !!userInfo && !!group && canDeleteImage(image, group, userInfo);

  const hasBans = image.bans && Object.keys(image.bans).length > 0;

  const handleCloseDeleteModal = () => {
    setShowDeleteModal(false);
    setDeleteError('');
  };

  const handleDelete = async () => {
    if (await deleteImage(image.group, image.name, setDeleteError)) {
      // Reload the whole list (deleting changes the set of images); onRefresh drives the spinner.
      onRefresh();
      handleCloseDeleteModal();
    }
  };

  const id = `${image.name}--${image.group}`;

  const handleLink = (e: React.MouseEvent<HTMLAnchorElement, MouseEvent>) => {
    e.stopPropagation(); //don't close accordion
    updateURLSection(id, '');
    void navigator.clipboard.writeText(window.location.href);
    const notify = () => toast(`Copied "${window.location.href}" to clipboard!`);
    notify();
    onExpand(id);
  };

  return (
    <AccordionItem eventKey={id}>
      <AccordionHeader>
        <div className="accordion-text">
          <div className="accordion-item-name">
            <div className="text">
              <OverlayTipBottom tip={`Copy image link to clipboard`}>
                <a className="title-link" onClick={handleLink}>
                  <FaLink className="title-link-no-color me-3" size={12} />
                </a>
              </OverlayTipBottom>
              {image.name}
            </div>
          </div>
          <div className="accordion-item-subtitle">
            <OverlayTipLeft tip={`This image is owned by the ${image.group} group.`}>
              <small>
                <i>{image.group}</i>
              </small>
            </OverlayTipLeft>
          </div>
        </div>
        <div className="accordion-item-status">
          <OverlayTipRight tip="Images can no longer be created or changed. This one still works in pipelines that name it; create a tool to replace it.">
            <Badge $tone={BadgeTone.Legacy} data-testid="legacy-badge">
              legacy
            </Badge>
          </OverlayTipRight>
          {hasBans && (
            <OverlayTipRight tip="This image has active bans and cannot be used.">
              <BanWarningIcon />
            </OverlayTipRight>
          )}
        </div>
        <HeaderActions onClick={(e) => e.stopPropagation()}>
          {userCanDelete && (
            <OverlayTipBottom tip="Delete this image.">
              <HeaderBtn $variant={ButtonVariant.Warning} data-testid="header-btn-delete" onClick={() => setShowDeleteModal(true)}>
                Delete
              </HeaderBtn>
            </OverlayTipBottom>
          )}
        </HeaderActions>
      </AccordionHeader>
      <AccordionBody>
        <ImageInfo
          ref={imageInfoRef}
          images={images}
          image={image}
          groups={groups}
          setImages={setImages}
          inEditMode={false}
          onExitEditMode={() => undefined}
          userCanModify={false}
        />
      </AccordionBody>
      <DeleteConfirmModal
        show={showDeleteModal}
        onHide={handleCloseDeleteModal}
        onConfirm={() => void handleDelete()}
        itemName={image.name}
        itemType="image"
        error={deleteError}
      />
    </AccordionItem>
  );
};

export default ImageAccordionItem;
