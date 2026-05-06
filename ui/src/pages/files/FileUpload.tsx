import { useEffect, useMemo, useState } from 'react';
import { useLocation } from 'react-router';

import Page from '@components/pages/Page';
import Title from '@components/shared/titles/Title';
import { useAuth } from '@utilities/auth';
import {
  UploadForm,
  UploadStatusDashboard,
  useFileUpload,
  useOriginState,
  handleAssociationUpdate,
  buildUploadFormBase,
  appendOriginToForm,
  AssociationCreate,
  AssociationKind,
  DEFAULT_TLP_SELECTION,
  DropzoneFile,
  TagEntry,
  TLPSelection,
} from '@components/pages/files/upload';

const FileUpload = () => {
  const { state } = useLocation();
  const entity = state?.entity;
  const { userInfo } = useAuth();

  const [filesArray, setFilesArray] = useState<DropzoneFile[]>([]);
  const [description, setDescription] = useState('');
  const [tags, setTags] = useState<TagEntry[]>([]);
  const [selectedGroups, setSelectedGroups] = useState<string[]>(entity ? entity.groups : []);
  const [selectedTLP, setSelectedTLP] = useState<TLPSelection>({ ...DEFAULT_TLP_SELECTION });
  const [reactionsList, setReactionsList] = useState<any[]>([]);
  const [associations, setAssociations] = useState<AssociationCreate[]>([]);

  const { originState, setOriginField } = useOriginState();

  const fileUpload = useFileUpload();
  const sortedGroups = useMemo(() => (userInfo?.groups ? [...userInfo.groups].sort() : []), [userInfo?.groups]);

  useEffect(() => {
    handleAssociationUpdate([AssociationKind.AssociatedWith], entity, selectedGroups, setAssociations);
  }, [entity, selectedGroups]);

  const tlpTags = (): TagEntry[] => {
    return Object.keys(selectedTLP)
      .filter((tlp) => selectedTLP[tlp])
      .map((tlp) => ({ key: 'TLP', value: tlp }));
  };

  const handleUpload = async () => {
    if (filesArray.length === 0) {
      fileUpload.setUploadError(['Please select a file to upload']);
      return;
    }

    const formResult = buildUploadFormBase(description, selectedGroups, tags, tlpTags());
    if ('errors' in formResult) {
      fileUpload.setUploadError(formResult.errors);
      return;
    }

    const originResult = appendOriginToForm(formResult.form, originState);
    if (!originResult.success) {
      fileUpload.setUploadError([originResult.error]);
      return;
    }

    await fileUpload.upload(filesArray, formResult.form, selectedGroups, associations, reactionsList);
  };

  const handleTLPChange = (newSelection: TLPSelection) => {
    setSelectedTLP(newSelection);
    fileUpload.resetStatusMessages();
  };

  const handleOriginChange = (field: keyof typeof originState, value: string) => {
    setOriginField(field, value);
    fileUpload.resetStatusMessages();
  };

  const handleBack = () => {
    fileUpload.resetStatusMessages();
    fileUpload.setShowUploadStatus(false);
  };

  return (
    <Page title="Upload Files · Thorium">
      <div className="d-flex justify-content-center">
        <Title>Upload</Title>
      </div>
      {fileUpload.showUploadStatus ? (
        <UploadStatusDashboard
          uploadInProgress={fileUpload.uploadInProgress}
          activeUploads={fileUpload.activeUploads}
          uploadStatus={fileUpload.uploadStatus}
          uploadFailures={fileUpload.uploadFailures}
          uploadStatusDropdown={fileUpload.uploadStatusDropdown}
          setUploadStatusDropdown={fileUpload.setUploadStatusDropdown}
          uploadReactionRes={fileUpload.uploadReactionRes}
          uploadReactions={fileUpload.uploadReactions}
          uploadReactionFailures={fileUpload.uploadReactionFailures}
          uploadError={fileUpload.uploadError}
          totalProgress={fileUpload.computeTotal()}
          onRetryAllFileUploads={() => fileUpload.retryAllFileUploads(selectedGroups, associations, reactionsList)}
          onRetryAllReactionSubmissions={() => fileUpload.retryAllReactionSubmissions(reactionsList)}
          onRetryFileUpload={(fileName) => fileUpload.retryFileUpload(fileName, selectedGroups, associations, reactionsList)}
          onRetrySubmitReaction={(status) => fileUpload.retrySubmitReaction(status, reactionsList)}
          onBack={handleBack}
          onCancel={fileUpload.cancelUpload}
        />
      ) : (
        <UploadForm
          uploadInProgress={fileUpload.uploadInProgress}
          filesArray={filesArray}
          setFilesArray={setFilesArray}
          selectedGroups={selectedGroups}
          setSelectedGroups={setSelectedGroups}
          userGroups={sortedGroups}
          description={description}
          setDescription={setDescription}
          tags={tags}
          setTags={setTags}
          selectedTLP={selectedTLP}
          onTLPChange={handleTLPChange}
          originState={originState}
          onOriginChange={handleOriginChange}
          reactionsList={reactionsList}
          setReactionsList={setReactionsList}
          userInfo={userInfo}
          onUpload={handleUpload}
          uploadStatus={fileUpload.uploadStatus}
          uploadError={fileUpload.uploadError}
          setUploadError={fileUpload.setUploadError}
          uploadSHA256={fileUpload.uploadSHA256}
          runReactionsRes={fileUpload.runReactionsRes}
          onResetStatus={fileUpload.resetStatusMessages}
        />
      )}
    </Page>
  );
};

export default FileUpload;
