//@ts-nocheck
import { uploadFile, createAssociation } from '@thorpi';
import { submitReactions } from '@components';
import { useUploadForm } from './context';
import { appendOriginToForm } from './origin_form_builder';
import { FileUploadStatus } from './types';

/** Maximum number of concurrent file uploads */
const PARALLEL_UPLOAD_LIMIT = 5;

/** Creates associations between a newly uploaded file and existing entities */
async function createFileAssociations(sha256: string, groups: string[], associations: any[]): Promise<void> {
  for (const association of associations) {
    association.groups = groups;
    association.targets = [{ File: sha256 }];
    await createAssociation(association, console.log);
  }
}

/** Derives TLP designation tags from the TLP checkbox state */
function deriveTlpTags(tlp: Record<string, boolean>): Array<{ key: string; value: string }> {
  return Object.keys(tlp)
    .filter((key) => tlp[key])
    .map((key) => ({ key: 'TLP', value: key }));
}

/**
 * Custom hook providing file upload orchestration, progress tracking, and retry logic.
 * Encapsulates the full upload lifecycle: validation, parallel uploading,
 * reaction submission, and error recovery.
 */
export function useUploadActions() {
  const { state, dispatch } = useUploadForm();

  /** Computes overall upload progress as a percentage across all files */
  const computeTotalProgress = (): number => {
    let totalBytesUploaded = 0;
    Object.values(state.uploadStatus).forEach((status) => {
      totalBytesUploaded += Math.ceil((status.progress / 100) * status.size);
    });
    return Math.floor((totalBytesUploaded / state.totalUploadSize) * 100);
  };

  /** Dispatches an upload status update for a single file */
  const updateFileStatus = (filePath: string, status: FileUploadStatus) => {
    dispatch({ type: 'UPDATE_UPLOAD_STATUS', payload: { key: filePath, value: status } });
  };

  /** Submits reactions for an uploaded file and tracks per-reaction results */
  const trackAndSubmitReactions = (uploadSHA256: string, submission: any, reactionsList: any[]) => {
    return submitReactions(uploadSHA256, reactionsList).then((reactionResults: any) => {
      let hasReactionError = false;

      Object.values(reactionResults).forEach((value: any, index: number) => {
        if (value.error !== '') {
          hasReactionError = true;
        } else {
          dispatch({ type: 'DECREMENT_UPLOAD_REACTION_FAILURES' });
        }

        const reactionPayload = {
          id: uploadSHA256 + value.pipeline,
          sha256: uploadSHA256,
          result: value,
          submission,
        };

        const existingResult = state.uploadReactionRes.filter((result) => result.id === reactionPayload.id);
        dispatch({
          type: existingResult.length === 0 ? 'ADD_UPLOAD_REACTION_RES' : 'UPDATE_UPLOAD_REACTION_RES',
          payload: reactionPayload,
        });

        reactionResults[index] = {
          ...reactionResults[index],
          path: submission.path,
          size: submission.size,
          sha256: uploadSHA256,
        };
      });

      dispatch({ type: 'UPDATE_UPLOAD_REACTIONS', payload: { key: submission.path, value: reactionResults } });

      updateFileStatus(submission.path, {
        progress: 100,
        size: submission.size,
        type: hasReactionError ? 'warning' : 'success',
        msg: hasReactionError ? 'Error submitting reactions' : 'Upload successful!',
        sha256: uploadSHA256,
        fileFail: false,
        reactionFail: hasReactionError,
      });
    });
  };

  /** Uploads a single file with progress tracking, then submits its reactions */
  const trackAndUploadFile = (fileForm: FormData) => {
    const submission = fileForm.get('data');
    dispatch({ type: 'ADD_ACTIVE_UPLOAD', payload: submission.path });

    const handleUploadProgress = (progress: number) => {
      if (progress < 1) {
        updateFileStatus(submission.path, {
          progress: Math.floor(progress * 100),
          size: submission.size,
          type: 'info',
          msg: 'Upload in progress',
          sha256: '',
          fileFail: false,
          reactionFail: false,
        });
      }
    };

    const handleUploadError = (errorMessage: string) => {
      dispatch({ type: 'ADD_UPLOAD_ERROR', payload: [errorMessage] });
      updateFileStatus(submission.path, {
        progress: 100,
        size: submission.size,
        type: 'danger',
        msg: errorMessage,
        sha256: '',
        fileFail: true,
        reactionFail: true,
      });
    };

    return uploadFile(fileForm, (msg: string) => handleUploadError(msg), handleUploadProgress, state.controller).then((response: any) => {
      if (response) {
        dispatch({ type: 'SET_UPLOAD_SHA256', payload: [response.sha256] });

        updateFileStatus(submission.path, {
          progress: 99,
          size: submission.size,
          type: 'info',
          msg: 'Submitting reactions',
          sha256: response.sha256,
          fileFail: false,
          reactionFail: false,
        });

        createFileAssociations(response.sha256, state.groups, state.associations);

        dispatch({ type: 'INCREMENT_UPLOAD_REACTION_FAILURES', payload: state.reactionsList.length });
        trackAndSubmitReactions(response.sha256, submission, state.reactionsList);

        if (Object.keys(state.uploadFailures).length > 0 && state.uploadFailures[submission.path]) {
          dispatch({ type: 'UPDATE_UPLOAD_FAILURES', payload: { key: submission.path } });
        }
      } else {
        dispatch({ type: 'UPDATE_UPLOAD_FAILURES', payload: { key: submission.path, value: fileForm } });
      }

      dispatch({ type: 'REMOVE_ACTIVE_UPLOAD', payload: submission.path });
    });
  };

  /** Executes parallel file uploads with a concurrency limit */
  const executeParallelUploads = async (fileForms: FormData[]) => {
    const uploadPromises: Promise<void>[] = [];
    let activeUploadCount = 0;

    for (const form of fileForms) {
      while (activeUploadCount >= PARALLEL_UPLOAD_LIMIT) {
        await new Promise((resolve) => setTimeout(resolve, 1000));
      }

      activeUploadCount++;
      uploadPromises.push(
        trackAndUploadFile(form).then(() => {
          activeUploadCount--;
        }),
      );
    }

    return Promise.all(uploadPromises);
  };

  /** Validates form inputs, builds per-file FormData payloads, and initiates parallel uploads */
  const startUpload = async () => {
    // Reset upload tracking state
    dispatch({ type: 'SET_UPLOAD_STATUS', payload: {} });
    dispatch({ type: 'SET_UPLOAD_REACTIONS', payload: {} });
    dispatch({ type: 'SET_UPLOAD_REACTION_RES', payload: [] });
    dispatch({ type: 'SET_TOTAL_UPLOAD_SIZE', payload: 0 });
    dispatch({ type: 'SET_UPLOAD_REACTION_FAILURES', payload: 0 });

    // Validate required fields
    if (state.files.length === 0) {
      dispatch({ type: 'SET_UPLOAD_ERROR', payload: ['Please select a file to upload'] });
      return;
    }

    if (state.groups.length === 0) {
      dispatch({ type: 'SET_UPLOAD_ERROR', payload: ['At least one group must be selected to submit a file'] });
      return;
    }

    // Build shared form fields common to all file uploads
    const sharedFormData = new FormData();

    if (state.description) {
      sharedFormData.append('description', state.description);
    }

    state.groups.forEach((group) => sharedFormData.append('groups', group));

    deriveTlpTags(state.tlp).forEach((tag) => {
      if (tag.key && tag.value) {
        sharedFormData.append(`tags[${tag.key}]`, tag.value);
      }
    });

    if (state.tags) {
      state.tags.forEach((tag) => {
        if (tag.key && tag.value) {
          sharedFormData.append(`tags[${tag.key}]`, tag.value);
        }
      });
    }

    // Validate and append origin fields
    const originValidationError = appendOriginToForm(sharedFormData, state.origin);
    if (originValidationError) {
      dispatch({ type: 'SET_UPLOAD_ERROR', payload: [originValidationError] });
      return;
    }

    sharedFormData.append('trigger_depth', String(state.trigger_depth));

    if (state.files.length > 1) {
      dispatch({ type: 'SET_SHOW_UPLOAD_STATUS', payload: true });
    }

    // Initialize per-file upload tracking entries
    const initialFileStatuses: Record<string, FileUploadStatus> = {};
    const initialDropdownStates: Record<string, boolean> = {};
    let totalUploadSize = 0;

    for (const file of state.files) {
      if (file) {
        totalUploadSize += file.size;
        initialFileStatuses[file.path] = {
          progress: 0,
          size: file.size,
          type: 'info',
          msg: 'Queued',
          sha256: '',
          fileFail: false,
          reactionFail: false,
        };
        initialDropdownStates[file.path] = false;
        dispatch({ type: 'UPDATE_UPLOAD_REACTIONS', payload: { key: file.path, value: [] } });
      }
    }

    dispatch({ type: 'SET_TOTAL_UPLOAD_SIZE', payload: totalUploadSize });
    dispatch({ type: 'SET_UPLOAD_STATUS', payload: initialFileStatuses });
    dispatch({ type: 'SET_UPLOAD_STATUS_DROPDOWN', payload: initialDropdownStates });
    dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: true });

    // Clone shared form fields into per-file FormData instances
    const perFileForms: FormData[] = state.files
      .filter((file: any) => file != null)
      .map((file: any) => {
        const fileForm = new FormData();
        for (const [key, val] of sharedFormData.entries()) {
          fileForm.append(key, val);
        }
        fileForm.set('data', file);
        return fileForm;
      });

    executeParallelUploads(perFileForms).then(() => {
      dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: false });
    });
  };

  /** Retries upload for a single previously failed file */
  const retryFileUpload = (fileName: string) => {
    dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: true });
    trackAndUploadFile(state.uploadFailures[fileName]).then(() => {
      dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: false });
    });
  };

  /** Retries upload for all previously failed files */
  const retryAllFileUploads = async () => {
    dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: true });
    dispatch({ type: 'RESET_UPLOAD_FAILURES' });

    const failedFileForms = Object.values(state.uploadFailures) as FormData[];
    executeParallelUploads(failedFileForms).then(() => {
      dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: false });
    });
  };

  /** Retries reaction submission for a single failed reaction */
  const retrySubmitReaction = (failedStatus: any) => {
    dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: true });
    updateFileStatus(failedStatus.submission.path, {
      progress: 99,
      size: failedStatus.submission.size,
      type: 'info',
      msg: 'Submitting reactions',
      sha256: failedStatus.sha256,
      fileFail: false,
      reactionFail: false,
    });

    const failedReaction = state.uploadReactionRes.filter((failure) => failure.id === failedStatus.id)[0];
    trackAndSubmitReactions(
      failedStatus.sha256,
      failedReaction.submission,
      state.reactionsList.filter((reaction: any) => reaction.pipeline === failedReaction.result.pipeline),
    ).then(() => {
      dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: false });
    });
  };

  /** Retries all failed reaction submissions */
  const retryAllReactionSubmissions = () => {
    dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: true });
    const reactionRetryPromises: Promise<void>[] = [];

    state.uploadReactionRes.forEach((value) => {
      if (value.result.error !== '') {
        const failedReaction = state.uploadReactionRes.filter((failure) => failure.id === value.id)[0];
        reactionRetryPromises.push(
          trackAndSubmitReactions(
            value.sha256,
            failedReaction.submission,
            state.reactionsList.filter((reaction: any) => reaction.pipeline === failedReaction.result.pipeline),
          ),
        );
      }
    });

    Promise.all(reactionRetryPromises).then(() => {
      dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: false });
    });
  };

  /** Aborts all in-progress uploads and creates a fresh abort controller */
  const cancelUpload = () => {
    state.controller.abort();
    dispatch({ type: 'SET_CONTROLLER', payload: new AbortController() });
  };

  return {
    startUpload,
    retryFileUpload,
    retryAllFileUploads,
    retrySubmitReaction,
    retryAllReactionSubmissions,
    cancelUpload,
    computeTotalProgress,
  };
}
