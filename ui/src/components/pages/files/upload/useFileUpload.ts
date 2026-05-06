import { useState } from 'react';
import {
  AssociationCreate,
  DropzoneFile,
  FileUploadStatus,
  ReactionResultEntry,
  ReactionSubmitResult,
  PARALLEL_UPLOAD_LIMIT,
} from './types';
import { createFileAssociations } from './associations';
import { uploadFile } from '@thorpi/files';
import { submitReactions } from '../reactions/reactions';

export function useFileUpload() {
  const [uploadError, setUploadError] = useState<string[]>([]);
  const [runReactionsRes, setRunReactionsRes] = useState<ReactionSubmitResult[]>([]);
  const [uploadSHA256, setUploadSHA256] = useState<string[]>([]);
  const [uploadInProgress, setUploadInProgress] = useState(false);
  const [activeUploads, setActiveUploads] = useState<string[]>([]);
  const [uploadStatus, setUploadStatus] = useState<Record<string, FileUploadStatus>>({});
  const [uploadFailures, setUploadFailures] = useState<Record<string, FormData>>({});
  const [uploadStatusDropdown, setUploadStatusDropdown] = useState<Record<string, boolean>>({});
  const [uploadReactionRes, setUploadReactionRes] = useState<ReactionResultEntry[]>([]);
  const [uploadReactions, setUploadReactions] = useState<Record<string, ReactionSubmitResult[]>>({});
  const [uploadReactionFailures, setUploadReactionFailures] = useState(0);
  const [totalUploadSize, setTotalUploadSize] = useState(0);
  const [showUploadStatus, setShowUploadStatus] = useState(false);
  const [controller, setController] = useState(new AbortController());

  const resetStatusMessages = () => {
    setUploadSHA256([]);
    setUploadError([]);
    setRunReactionsRes([]);
    setUploadStatus({});
    setUploadReactions({});
    setUploadReactionRes([]);
    setUploadReactionFailures(0);
  };

  const computeTotal = (): number => {
    let totalUploaded = 0;
    Object.values(uploadStatus).map((value) => {
      totalUploaded = totalUploaded + Math.ceil((value.progress / 100) * value.size);
    });
    return Math.floor((totalUploaded / totalUploadSize) * 100);
  };

  const trackAndSubmitReactions = (sha256: string, submission: { path: string; size: number }, submitReactionsList: any[]) => {
    const allRunReactionsRes: ReactionSubmitResult[] = [];
    return submitReactions(sha256, submitReactionsList).then((submitRes: any[]) => {
      let error = false;
      Object.values(submitRes).map((value: any, index: number) => {
        if (value.error !== '') {
          error = true;
        } else {
          setUploadReactionFailures((prev) => prev - 1);
        }
        const existingResult = uploadReactionRes.filter((result) => result.id === sha256 + value.pipeline);
        if (existingResult.length === 0) {
          setUploadReactionRes((prev) => [
            ...prev,
            {
              id: sha256 + value.pipeline,
              sha256: sha256,
              result: value,
              submission: submission,
            },
          ]);
        } else {
          setUploadReactionRes((prev) =>
            prev.map((result) => {
              if (result.id === sha256 + value.pipeline) {
                return {
                  id: sha256 + value.pipeline,
                  sha256: sha256,
                  result: value,
                  submission: submission,
                };
              } else {
                return result;
              }
            }),
          );
        }
        submitRes[index] = {
          ...submitRes[index],
          path: submission.path,
          size: submission.size,
          sha256: sha256,
        };
      });
      setUploadReactions((prev) => ({
        ...prev,
        [submission.path]: submitRes,
      }));
      allRunReactionsRes.push(...submitRes);
      if (error) {
        setUploadStatus((prev) => ({
          ...prev,
          [submission.path]: {
            progress: 100,
            size: submission.size,
            type: 'warning',
            msg: 'Error submitting reactions',
            sha256: sha256,
            fileFail: false,
            reactionFail: true,
          },
        }));
      } else {
        setUploadStatus((prev) => ({
          ...prev,
          [submission.path]: {
            progress: 100,
            size: submission.size,
            type: 'success',
            msg: 'Upload successful!',
            sha256: sha256,
            fileFail: false,
            reactionFail: false,
          },
        }));
      }
      setRunReactionsRes([...allRunReactionsRes]);
    });
  };

  const trackAndUploadFile = (form: FormData, selectedGroups: string[], associations: AssociationCreate[], reactionsList: any[]) => {
    const allResSha256: string[] = [];
    const allResErrors: string[] = [];
    const submission = form.get('data') as DropzoneFile;
    const filePath = submission.path || submission.name;
    setActiveUploads((prev) => [...prev, filePath]);

    const uploadFileProgressHandler = (progress: number) => {
      if (progress < 1) {
        setUploadStatus((prev) => ({
          ...prev,
          [filePath]: {
            progress: Math.floor(progress * 100),
            size: submission.size,
            type: 'info',
            msg: 'Upload in progress',
            sha256: '',
            fileFail: false,
            reactionFail: false,
          },
        }));
      }
    };

    const addUploadErrorMsg = (error: string) => {
      allResErrors.push(error);
      setUploadStatus((prev) => ({
        ...prev,
        [filePath]: {
          progress: 100,
          size: submission.size,
          type: 'danger',
          msg: error,
          sha256: '',
          fileFail: true,
          reactionFail: true,
        },
      }));
    };

    return uploadFile(form, addUploadErrorMsg, uploadFileProgressHandler, controller).then((response: any) => {
      if (response) {
        allResSha256.push(response.sha256);
        setUploadStatus((prev) => ({
          ...prev,
          [filePath]: {
            progress: 99,
            size: submission.size,
            type: 'info',
            msg: 'Submitting reactions',
            sha256: response.sha256,
            fileFail: false,
            reactionFail: false,
          },
        }));
        createFileAssociations(response.sha256, selectedGroups, associations);
        setUploadReactionFailures((prev) => prev + reactionsList.length);
        trackAndSubmitReactions(response.sha256, { path: filePath, size: submission.size }, reactionsList);
        if (Object.keys(uploadFailures).length > 0) {
          setUploadFailures((prev) => {
            const { [filePath]: _, ...rest } = prev;
            return rest;
          });
        }
      } else {
        setUploadFailures((prev) => ({
          ...prev,
          [filePath]: form,
        }));
      }
      setUploadSHA256([...allResSha256]);
      if (allResErrors.length > 0) {
        setUploadError((prev) => [...prev, ...allResErrors]);
      }
      setActiveUploads((prev) => prev.filter((item) => item !== filePath));
    });
  };

  const upload = async (
    filesArray: DropzoneFile[],
    formBase: FormData,
    selectedGroups: string[],
    associations: AssociationCreate[],
    reactionsList: any[],
  ) => {
    setUploadStatus({});
    setUploadReactions({});
    setUploadReactionRes([]);
    setTotalUploadSize(0);
    setUploadReactionFailures(0);

    if (filesArray.length > 1) {
      setShowUploadStatus(true);
    }

    const filesUploadProgress: Record<string, FileUploadStatus> = {};
    const statusDropdown: Record<string, boolean> = {};
    let uploadSize = 0;
    setUploadInProgress(true);

    for (const submission of filesArray) {
      const filePath = submission.path || submission.name;
      if (submission) {
        uploadSize = uploadSize + submission.size;
        filesUploadProgress[filePath] = {
          progress: 0,
          size: submission.size,
          type: 'info',
          msg: 'Queued',
          sha256: '',
          fileFail: false,
          reactionFail: false,
        };
      }
      statusDropdown[filePath] = false;
      setUploadReactions((prev) => ({
        ...prev,
        [filePath]: [],
      }));
    }
    setTotalUploadSize(uploadSize);
    setUploadStatus(filesUploadProgress);
    setUploadStatusDropdown(statusDropdown);

    const uploadPromises: Promise<void>[] = [];
    let currentUploadCount = 0;
    for (const submission of filesArray) {
      const newForm = new FormData();
      for (const [key, val] of formBase.entries()) {
        newForm.append(key, val);
      }
      if (submission) {
        newForm.set('data', submission);
      }

      while (currentUploadCount >= PARALLEL_UPLOAD_LIMIT) {
        await new Promise((f) => setTimeout(f, 1000));
      }
      currentUploadCount = currentUploadCount + 1;
      uploadPromises.push(
        trackAndUploadFile(newForm, selectedGroups, associations, reactionsList).then(() => {
          currentUploadCount = currentUploadCount - 1;
        }),
      );
    }
    Promise.all(uploadPromises).then(() => {
      setUploadInProgress(false);
    });
  };

  const retryFileUpload = (fileName: string, selectedGroups: string[], associations: AssociationCreate[], reactionsList: any[]) => {
    setUploadInProgress(true);
    trackAndUploadFile(uploadFailures[fileName], selectedGroups, associations, reactionsList).then(() => setUploadInProgress(false));
  };

  const retryAllFileUploads = async (selectedGroups: string[], associations: AssociationCreate[], reactionsList: any[]) => {
    setUploadInProgress(true);
    const failureSnapshot = { ...uploadFailures };
    setUploadFailures({});
    const uploadPromises: Promise<void>[] = [];
    let currentUploadCount = 0;
    Object.entries(failureSnapshot).map(async ([, form]) => {
      while (currentUploadCount >= PARALLEL_UPLOAD_LIMIT) {
        await new Promise((f) => setTimeout(f, 1000));
      }
      currentUploadCount = currentUploadCount + 1;
      uploadPromises.push(
        trackAndUploadFile(form, selectedGroups, associations, reactionsList).then(() => {
          currentUploadCount = currentUploadCount - 1;
        }),
      );
    });
    Promise.all(uploadPromises).then(() => {
      setUploadInProgress(false);
    });
  };

  const retrySubmitReaction = (status: ReactionResultEntry, reactionsList: any[]) => {
    setUploadInProgress(true);
    setUploadStatus((prev) => ({
      ...prev,
      [status.submission.path]: {
        progress: 99,
        size: status.submission.size,
        type: 'info',
        msg: 'Submitting reactions',
        sha256: status.sha256,
        fileFail: false,
        reactionFail: false,
      },
    }));
    const failedReaction = uploadReactionRes.filter((failure) => failure.id === status.id)[0];
    trackAndSubmitReactions(
      status.sha256,
      failedReaction.submission,
      reactionsList.filter((reaction: any) => reaction.pipeline === failedReaction.result.pipeline),
    ).then(() => setUploadInProgress(false));
  };

  const retryAllReactionSubmissions = (reactionsList: any[]) => {
    setUploadInProgress(true);
    const submissionPromises: Promise<void>[] = [];
    uploadReactionRes.map((value) => {
      if (value.result.error !== '') {
        const failedReaction = uploadReactionRes.filter((failure) => failure.id === value.id)[0];
        submissionPromises.push(
          trackAndSubmitReactions(
            value.sha256,
            failedReaction.submission,
            reactionsList.filter((reaction: any) => reaction.pipeline === failedReaction.result.pipeline),
          ),
        );
      }
    });
    Promise.all(submissionPromises).then(() => {
      setUploadInProgress(false);
    });
  };

  const cancelUpload = () => {
    controller.abort();
    setController(new AbortController());
  };

  return {
    uploadError,
    setUploadError,
    runReactionsRes,
    uploadSHA256,
    uploadInProgress,
    activeUploads,
    uploadStatus,
    uploadFailures,
    uploadStatusDropdown,
    setUploadStatusDropdown,
    uploadReactionRes,
    uploadReactions,
    uploadReactionFailures,
    totalUploadSize,
    showUploadStatus,
    setShowUploadStatus,
    resetStatusMessages,
    computeTotal,
    upload,
    retryFileUpload,
    retryAllFileUploads,
    retrySubmitReaction,
    retryAllReactionSubmissions,
    cancelUpload,
  };
}
