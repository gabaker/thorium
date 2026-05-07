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
import { SampleSubmissionResponse } from '@models/files';
import { ReactionSelection } from '@models/reactions';
import { submitReactions } from '../reactions/reactions';

function createSemaphore(limit: number) {
  let active = 0;
  const waiting: (() => void)[] = [];

  const acquire = (): Promise<void> => {
    if (active < limit) {
      active++;
      return Promise.resolve();
    }
    return new Promise<void>((resolve) => waiting.push(resolve));
  };

  const release = () => {
    active--;
    const next = waiting.shift();
    if (next) {
      active++;
      next();
    }
  };

  return { acquire, release };
}

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
    Object.values(uploadStatus).forEach((value) => {
      totalUploaded = totalUploaded + Math.ceil((value.progress / 100) * value.size);
    });
    return Math.floor((totalUploaded / totalUploadSize) * 100);
  };

  const trackAndSubmitReactions = (sha256: string, submission: { path: string; size: number }, submitReactionsList: ReactionSelection[]) => {
    const allRunReactionsRes: ReactionSubmitResult[] = [];
    return submitReactions(sha256, submitReactionsList).then((submitRes: ReactionSubmitResult[]) => {
      let error = false;
      submitRes.forEach((value: ReactionSubmitResult, index: number) => {
        if (value.error !== '') {
          error = true;
        } else {
          setUploadReactionFailures((prev) => prev - 1);
        }
        setUploadReactionRes((prev) => {
          const existingIndex = prev.findIndex((result) => result.id === sha256 + value.pipeline);
          const entry: ReactionResultEntry = {
            id: sha256 + value.pipeline,
            sha256: sha256,
            result: value,
            submission: submission,
          };
          if (existingIndex === -1) {
            return [...prev, entry];
          }
          return prev.map((result, i) => (i === existingIndex ? entry : result));
        });
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

  const trackAndUploadFile = (form: FormData, selectedGroups: string[], associations: AssociationCreate[], reactionsList: ReactionSelection[]) => {
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

    return uploadFile(form, addUploadErrorMsg, uploadFileProgressHandler, controller).then(async (response: SampleSubmissionResponse | false) => {
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
        await createFileAssociations(response.sha256, selectedGroups, associations);
        setUploadReactionFailures((prev) => prev + reactionsList.length);
        await trackAndSubmitReactions(response.sha256, { path: filePath, size: submission.size }, reactionsList);
        setUploadFailures((prev) => {
          if (filePath in prev) {
            const { [filePath]: _, ...rest } = prev;
            return rest;
          }
          return prev;
        });
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
    reactionsList: ReactionSelection[],
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

    const sem = createSemaphore(PARALLEL_UPLOAD_LIMIT);
    const uploadPromises: Promise<void>[] = [];
    for (const submission of filesArray) {
      const newForm = new FormData();
      for (const [key, val] of formBase.entries()) {
        newForm.append(key, val);
      }
      if (submission) {
        newForm.set('data', submission);
      }

      uploadPromises.push(
        sem.acquire().then(async () => {
          try {
            await trackAndUploadFile(newForm, selectedGroups, associations, reactionsList);
          } finally {
            sem.release();
          }
        }),
      );
    }
    await Promise.all(uploadPromises);
    setUploadInProgress(false);
  };

  const retryFileUpload = async (fileName: string, selectedGroups: string[], associations: AssociationCreate[], reactionsList: ReactionSelection[]) => {
    setUploadInProgress(true);
    await trackAndUploadFile(uploadFailures[fileName], selectedGroups, associations, reactionsList);
    setUploadInProgress(false);
  };

  const retryAllFileUploads = async (selectedGroups: string[], associations: AssociationCreate[], reactionsList: ReactionSelection[]) => {
    setUploadInProgress(true);
    const failureSnapshot = { ...uploadFailures };
    setUploadFailures({});

    const sem = createSemaphore(PARALLEL_UPLOAD_LIMIT);
    const uploadPromises: Promise<void>[] = [];
    for (const form of Object.values(failureSnapshot)) {
      uploadPromises.push(
        sem.acquire().then(async () => {
          try {
            await trackAndUploadFile(form, selectedGroups, associations, reactionsList);
          } finally {
            sem.release();
          }
        }),
      );
    }
    await Promise.all(uploadPromises);
    setUploadInProgress(false);
  };

  const retrySubmitReaction = async (status: ReactionResultEntry, reactionsList: ReactionSelection[]) => {
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
    await trackAndSubmitReactions(
      status.sha256,
      failedReaction.submission,
      reactionsList.filter((reaction: ReactionSelection) => reaction.pipeline === failedReaction.result.pipeline),
    );
    setUploadInProgress(false);
  };

  const retryAllReactionSubmissions = async (reactionsList: ReactionSelection[]) => {
    setUploadInProgress(true);
    const submissionPromises: Promise<void>[] = [];
    uploadReactionRes.forEach((value) => {
      if (value.result.error !== '') {
        const failedReaction = uploadReactionRes.filter((failure) => failure.id === value.id)[0];
        submissionPromises.push(
          trackAndSubmitReactions(
            value.sha256,
            failedReaction.submission,
            reactionsList.filter((reaction: ReactionSelection) => reaction.pipeline === failedReaction.result.pipeline),
          ),
        );
      }
    });
    await Promise.all(submissionPromises);
    setUploadInProgress(false);
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
