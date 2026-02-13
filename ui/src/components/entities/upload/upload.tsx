import React, { useEffect } from 'react';
import { AssociationKind, BlankAssociationCreate, Entity } from '@models';
import { UploadFormProvider, useUploadForm } from './context';
import { useUploadActions } from './use_upload';
import { FileUploadSection } from './sections/upload_file';
import { GroupsSelectionSection } from './sections/groups';
import { DescriptionSection } from './sections/description';
import { TagsSection } from './sections/tags';
import { TLPSelectionSection } from './sections/tlp';
import { OriginFormTabsSection } from './sections/origin/tabs';
import { PipelinesSection } from './sections/pipelines';
import { UploadFormFooter } from './sections/footer';
import { UploadStatusPanel } from './sections/upload_status';

/** Props for the top-level Upload page component */
type UploadProps = {
  entity: Entity | undefined;
};

/** Inner upload component that renders either the upload form or the upload status panel */
const UploadInner: React.FC<{ entity: Entity | undefined }> = ({ entity }) => {
  const { state, dispatch } = useUploadForm();
  const { startUpload } = useUploadActions();

  useEffect(() => {
    if (entity) {
      const newAssociation = structuredClone(BlankAssociationCreate);
      newAssociation.kind = AssociationKind.AssociatedWith;
      newAssociation.source = { Entity: { id: entity.id, name: entity.name } };
      newAssociation.groups = state.groups;
      dispatch({ type: 'SET_ASSOCIATIONS', payload: [newAssociation] });
    }
  }, []);

  return (
    <>
      {state.showUploadStatus && <UploadStatusPanel />}
      {!state.showUploadStatus && (
        <>
          <FileUploadSection />
          <GroupsSelectionSection />
          <DescriptionSection />
          <TagsSection />
          <TLPSelectionSection />
          <OriginFormTabsSection />
          <PipelinesSection />
          <UploadFormFooter onUpload={startUpload} />
        </>
      )}
    </>
  );
};

/** Top-level upload page component that wraps the form in its context provider */
export const Upload: React.FC<UploadProps> = ({ entity }) => {
  return (
    <UploadFormProvider entity={entity}>
      <UploadInner entity={entity} />
    </UploadFormProvider>
  );
};
