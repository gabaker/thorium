//@ts-nocheck
import React, { useEffect } from 'react';

// project imports
import { AssociationKind, BlankAssociationCreate, Entity } from '@models';
import { uploadFile, createAssociation } from '@thorpi';
import { submitReactions } from '@components';
import { OriginType } from './types';

// Context and components
import { UploadFormProvider, useUploadForm } from './upload_context';
import { FileUploadSection } from './file_upload_section';
import { GroupsSelectionSection } from './groups_selection_section';
import { DescriptionSection } from './description_section';
import { TagsSection } from './tags_section';
import { TLPSelectionSection } from './tlp_selection';
import { OriginFormTabsSection } from './origin_form_tabs';
import { PipelinesSection } from './pipelines_section';
import { UploadFormFooter } from './upload_form_footer';
import { UploadStatusPanel } from './upload_status_panel';

const PARALLELUPLOADLIMIT = 5;

async function createFileAssociations(sha256: string, groups: string[], associations: any[]): Promise<void> {
  for (let i = 0; i < associations.length; i++) {
    associations[i].groups = groups;
    associations[i].targets = [{ File: sha256 }];
    await createAssociation(associations[i], console.log);
  }
}

type UploadProps = {
  entity: Entity | undefined;
};

const UploadInner: React.FC<{ entity: Entity | undefined }> = ({ entity }) => {
  const { state, dispatch } = useUploadForm();

  useEffect(() => {
    if (entity) {
      const newAssociation = structuredClone(BlankAssociationCreate);
      newAssociation.kind = AssociationKind.AssociatedWith;
      newAssociation.source = { Entity: { id: entity.id, name: entity.name } };
      newAssociation.groups = state.groups;
      dispatch({ type: 'SET_ASSOCIATIONS', payload: [newAssociation] });
    }
  }, []);

  const tlpTags = () => {
    const desiredTags = Object.keys(state.tlp).filter((tlp) => state.tlp[tlp]);
    return desiredTags.map((tlp) => ({ key: 'TLP', value: tlp }));
  };

  const computeTotal = () => {
    let totalUploaded = 0;
    Object.values(state.uploadStatus).map((value) => {
      totalUploaded = totalUploaded + Math.ceil((value.progress / 100) * value.size);
    });
    return Math.floor((totalUploaded / state.totalUploadSize) * 100);
  };

  const trackAndUploadFile = (form: FormData) => {
    const submission = form.get('data');
    dispatch({ type: 'ADD_ACTIVE_UPLOAD', payload: submission.path });

    const uploadFileProgressHandler = (progress: number) => {
      if (progress < 1) {
        dispatch({
          type: 'UPDATE_UPLOAD_STATUS',
          payload: {
            key: submission.path,
            value: {
              progress: Math.floor(progress * 100),
              size: submission.size,
              type: 'info',
              msg: 'Upload in progress',
              sha256: '',
              fileFail: false,
              reactionFail: false,
            },
          },
        });
      }
    };

    const addUploadErrorMsg = (file: any, error: string) => {
      dispatch({ type: 'ADD_UPLOAD_ERROR', payload: [error] });
      dispatch({
        type: 'UPDATE_UPLOAD_STATUS',
        payload: {
          key: file.path,
          value: {
            progress: 100,
            size: file.size,
            type: 'danger',
            msg: error,
            sha256: '',
            fileFail: true,
            reactionFail: true,
          },
        },
      });
    };

    return uploadFile(form, (msg: string) => addUploadErrorMsg(submission, msg), uploadFileProgressHandler, state.controller).then(
      (response: any) => {
        if (response) {
          dispatch({ type: 'SET_UPLOAD_SHA256', payload: [response.sha256] });

          dispatch({
            type: 'UPDATE_UPLOAD_STATUS',
            payload: {
              key: submission.path,
              value: {
                progress: 99,
                size: submission.size,
                type: 'info',
                msg: 'Submitting reactions',
                sha256: response.sha256,
                fileFail: false,
                reactionFail: false,
              },
            },
          });

          createFileAssociations(response.sha256, state.groups, state.associations);

          dispatch({ type: 'INCREMENT_UPLOAD_REACTION_FAILURES', payload: state.reactionsList.length });
          trackAndSubmitReactions(response.sha256, submission, state.reactionsList);

          if (Object.keys(state.uploadFailures).length > 0 && state.uploadFailures[submission.path]) {
            dispatch({ type: 'UPDATE_UPLOAD_FAILURES', payload: { key: submission.path } });
          }
        } else {
          dispatch({ type: 'UPDATE_UPLOAD_FAILURES', payload: { key: submission.path, value: form } });
        }

        dispatch({ type: 'REMOVE_ACTIVE_UPLOAD', payload: submission.path });
      },
    );
  };

  const trackAndSubmitReactions = (uploadSHA256: string, submission: any, submitReactionsList: any[]) => {
    return submitReactions(uploadSHA256, submitReactionsList).then((submitRes) => {
      let error = false;

      Object.values(submitRes).map((value, index) => {
        if (value.error !== '') {
          error = true;
        } else {
          dispatch({ type: 'DECREMENT_UPLOAD_REACTION_FAILURES' });
        }

        const reactionPayload = {
          id: uploadSHA256 + value.pipeline,
          sha256: uploadSHA256,
          result: value,
          submission: submission,
        };

        const existingResult = state.uploadReactionRes.filter((result) => result.id === reactionPayload.id);
        dispatch({
          type: existingResult.length === 0 ? 'ADD_UPLOAD_REACTION_RES' : 'UPDATE_UPLOAD_REACTION_RES',
          payload: reactionPayload,
        });

        submitRes[index] = {
          ...submitRes[index],
          path: submission.path,
          size: submission.size,
          sha256: uploadSHA256,
        };
      });

      dispatch({ type: 'UPDATE_UPLOAD_REACTIONS', payload: { key: submission.path, value: submitRes } });

      dispatch({
        type: 'UPDATE_UPLOAD_STATUS',
        payload: {
          key: submission.path,
          value: {
            progress: 100,
            size: submission.size,
            type: error ? 'warning' : 'success',
            msg: error ? 'Error submitting reactions' : 'Upload successful!',
            sha256: uploadSHA256,
            fileFail: false,
            reactionFail: error,
          },
        },
      });
    });
  };

  /** Appends origin fields to the form based on the active origin type. Returns false if validation fails. */
  const addOriginToForm = (formBase: FormData): boolean => {
    const { origin } = state;
    const appendOrigin = (field: string, value: any) => formBase.append(`origin[${field}]`, value);
    const appendOriginList = (field: string, values: string[]) => values.forEach((v) => formBase.append(`origin[${field}]`, v));

    if (origin.origin_type === OriginType.Downloaded) {
      if (origin.url) {
        appendOrigin('origin_type', origin.origin_type);
        appendOrigin('url', origin.url);
        if (origin.name) appendOrigin('name', origin.name);
      } else if (origin.name) {
        dispatch({ type: 'SET_UPLOAD_ERROR', payload: ['ORIGIN field "SITE NAME" set while necessary field "URL" is blank'] });
        return false;
      }
    } else if (origin.origin_type === OriginType.Transformed || origin.origin_type === OriginType.Unpacked) {
      if (origin.parent) {
        appendOrigin('origin_type', origin.origin_type);
        appendOrigin('parent', origin.parent);
        if (origin.tool) appendOrigin('tool', origin.tool);
        if (origin.flags.length > 0) appendOriginList('flags', origin.flags);
        if (origin.cmd) appendOrigin('cmd', origin.cmd);
      } else if (origin.tool || origin.flags.length > 0) {
        dispatch({ type: 'SET_UPLOAD_ERROR', payload: ['ORIGIN field set while necessary field "PARENT" is blank'] });
        return false;
      }
    } else if (origin.origin_type === OriginType.Carved) {
      if (origin.parent) {
        if (!origin.carved_type) {
          dispatch({ type: 'SET_UPLOAD_ERROR', payload: ['ORIGIN "Carved" needs a specified type'] });
          return false;
        }
        appendOrigin('parent', origin.parent);
        if (origin.tool) appendOrigin('tool', origin.tool);
        const fullType = origin.origin_type + origin.carved_type;
        appendOrigin('origin_type', fullType);

        if (fullType === 'CarvedPcap') {
          if (origin.src_ip) appendOrigin('src_ip', origin.src_ip);
          if (origin.dest_ip) appendOrigin('dest_ip', origin.dest_ip);
          if (origin.src_port) appendOrigin('src_port', origin.src_port);
          if (origin.dest_port) appendOrigin('dest_port', origin.dest_port);
          if (origin.proto) appendOrigin('proto', origin.proto);
          if (origin.pcap_url) appendOrigin('url', origin.pcap_url);
        }
      } else if (origin.tool) {
        dispatch({ type: 'SET_UPLOAD_ERROR', payload: ['ORIGIN field "TOOL" set while necessary field "PARENT" is blank'] });
        return false;
      }
    } else if (origin.origin_type === OriginType.Wire) {
      if (origin.sniffer) {
        appendOrigin('origin_type', origin.origin_type);
        appendOrigin('sniffer', origin.sniffer);
        if (origin.source) appendOrigin('source', origin.source);
        if (origin.destination) appendOrigin('destination', origin.destination);
      } else if (origin.source || origin.destination) {
        dispatch({ type: 'SET_UPLOAD_ERROR', payload: ['ORIGIN field set while necessary field "SNIFFER" is blank'] });
        return false;
      }
    } else if (origin.origin_type === OriginType.Incident) {
      if (origin.incident) {
        appendOrigin('origin_type', origin.origin_type);
        appendOrigin('incident', origin.incident);
        if (origin.mission_team) appendOrigin('mission_team', origin.mission_team);
        if (origin.cover_term) appendOrigin('cover_term', origin.cover_term);
        if (origin.network) appendOrigin('network', origin.network);
        if (origin.machine) appendOrigin('machine', origin.machine);
        if (origin.location) appendOrigin('location', origin.location);
      } else if (origin.cover_term || origin.mission_team || origin.network || origin.machine || origin.location) {
        dispatch({ type: 'SET_UPLOAD_ERROR', payload: ['ORIGIN field set while necessary field "INCIDENT ID" is blank'] });
        return false;
      }
    } else if (origin.origin_type === OriginType.MemoryDump) {
      if (origin.memory_type) {
        appendOrigin('origin_type', origin.origin_type);
        appendOrigin('memory_type', origin.memory_type);
        if (origin.parent) appendOrigin('parent', origin.parent);
        if (origin.reconstructed.length > 0) appendOriginList('reconstructed', origin.reconstructed);
        if (origin.base_addr) appendOrigin('base_addr', origin.base_addr);
      } else if (origin.parent || origin.reconstructed.length > 0 || origin.base_addr) {
        dispatch({ type: 'SET_UPLOAD_ERROR', payload: ['ORIGIN field set while necessary field "MEMORY TYPE" is blank'] });
        return false;
      }
    } else if (origin.origin_type === OriginType.Source) {
      if (origin.repo) {
        appendOrigin('origin_type', origin.origin_type);
        appendOrigin('repo', origin.repo);
        if (origin.commitish) appendOrigin('commitish', origin.commitish);
        if (origin.commit) appendOrigin('commit', origin.commit);
        if (origin.system) appendOrigin('system', origin.system);
        if (origin.flags.length > 0) appendOriginList('flags', origin.flags);
        if (origin.supporting !== undefined) appendOrigin('supporting', String(origin.supporting));
      } else if (origin.commit || origin.system || origin.commitish || origin.flags.length > 0) {
        dispatch({
          type: 'SET_UPLOAD_ERROR',
          payload: ['ORIGIN field set while necessary field "REPOSITORY" is blank'],
        });
        return false;
      }
    }

    if (origin.result_ids.length > 0) {
      appendOriginList('result_ids', origin.result_ids);
    }

    return true;
  };

  const upload = async () => {
    dispatch({ type: 'SET_UPLOAD_STATUS', payload: {} });
    dispatch({ type: 'SET_UPLOAD_REACTIONS', payload: {} });
    dispatch({ type: 'SET_UPLOAD_REACTION_RES', payload: [] });
    dispatch({ type: 'SET_TOTAL_UPLOAD_SIZE', payload: 0 });
    dispatch({ type: 'SET_UPLOAD_REACTION_FAILURES', payload: 0 });

    if (state.files.length === 0) {
      dispatch({ type: 'SET_UPLOAD_ERROR', payload: ['Please select a file to upload'] });
      return;
    }

    const formBase = new FormData();

    if (state.description) {
      formBase.append('description', state.description);
    }

    if (state.groups.length > 0) {
      state.groups.map((group) => formBase.append('groups', group));
    } else {
      dispatch({ type: 'SET_UPLOAD_ERROR', payload: ['At least one group must be selected to submit a file'] });
      return;
    }

    const filteredTLPTags = tlpTags();
    if (filteredTLPTags) {
      filteredTLPTags.map((tag) => {
        if (tag.key && tag.value) {
          formBase.append(`tags[${tag.key}]`, tag.value);
        }
      });
    }

    if (state.tags) {
      state.tags.map((tag) => {
        if (tag.key && tag.value) {
          formBase.append(`tags[${tag.key}]`, tag.value);
        }
      });
    }

    if (!addOriginToForm(formBase)) {
      return;
    }

    formBase.append('trigger_depth', String(state.trigger_depth));

    if (state.files.length > 1) {
      dispatch({ type: 'SET_SHOW_UPLOAD_STATUS', payload: true });
    }

    const filesUploadProgress = {};
    const statusDropdown = {};
    let uploadSize = 0;

    for (const submission of state.files) {
      if (submission) {
        uploadSize += submission.size;
        filesUploadProgress[submission.path] = {
          progress: 0,
          size: submission.size,
          type: 'info',
          msg: 'Queued',
          sha256: '',
          fileFail: false,
          reactionFail: false,
        };
        statusDropdown[submission.path] = false;
        dispatch({ type: 'UPDATE_UPLOAD_REACTIONS', payload: { key: submission.path, value: [] } });
      }
    }

    dispatch({ type: 'SET_TOTAL_UPLOAD_SIZE', payload: uploadSize });
    dispatch({ type: 'SET_UPLOAD_STATUS', payload: filesUploadProgress });
    dispatch({ type: 'SET_UPLOAD_STATUS_DROPDOWN', payload: statusDropdown });
    dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: true });

    const uploadPromises = [];
    let currentUploadCount = 0;

    for (const submission of state.files) {
      const newForm = new FormData();
      for (const [key, val] of formBase.entries()) {
        newForm.append(key, val);
      }
      if (submission) {
        newForm.set('data', submission);
      }

      while (currentUploadCount >= PARALLELUPLOADLIMIT) {
        await new Promise((f) => setTimeout(f, 1000));
      }

      currentUploadCount++;
      uploadPromises.push(
        trackAndUploadFile(newForm).then(() => {
          currentUploadCount--;
        }),
      );
    }

    Promise.all(uploadPromises).then(() => {
      dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: false });
    });
  };

  const retryFileUpload = (fileName: string) => {
    dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: true });
    trackAndUploadFile(state.uploadFailures[fileName]).then(() => {
      dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: false });
    });
  };

  const retryAllFileUploads = async () => {
    dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: true });
    dispatch({ type: 'RESET_UPLOAD_FAILURES' });

    const uploadPromises = [];
    let currentUploadCount = 0;

    Object.entries(state.uploadFailures).map(async ([key, form]) => {
      while (currentUploadCount >= PARALLELUPLOADLIMIT) {
        await new Promise((f) => setTimeout(f, 1000));
      }
      currentUploadCount++;
      uploadPromises.push(
        trackAndUploadFile(form).then(() => {
          currentUploadCount--;
        }),
      );
    });

    Promise.all(uploadPromises).then(() => {
      dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: false });
    });
  };

  const retrySubmitReaction = (status: any) => {
    dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: true });
    dispatch({
      type: 'UPDATE_UPLOAD_STATUS',
      payload: {
        key: status.submission.path,
        value: {
          progress: 99,
          size: status.submission.size,
          type: 'info',
          msg: 'Submitting reactions',
          sha256: status.sha256,
          fileFail: false,
          reactionFail: false,
        },
      },
    });

    const failedReaction = state.uploadReactionRes.filter((failure) => failure.id === status.id)[0];
    trackAndSubmitReactions(
      status.sha256,
      failedReaction.submission,
      state.reactionsList.filter((reaction) => reaction.pipeline === failedReaction.result.pipeline),
    ).then(() => {
      dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: false });
    });
  };

  const retryAllReactionSubmissions = () => {
    dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: true });
    const submissionPromises = [];

    state.uploadReactionRes.map((value) => {
      if (value.result.error !== '') {
        const failedReaction = state.uploadReactionRes.filter((failure) => failure.id === value.id)[0];
        submissionPromises.push(
          trackAndSubmitReactions(
            value.sha256,
            failedReaction.submission,
            state.reactionsList.filter((reaction) => reaction.pipeline === failedReaction.result.pipeline),
          ),
        );
      }
    });

    Promise.all(submissionPromises).then(() => {
      dispatch({ type: 'SET_UPLOAD_IN_PROGRESS', payload: false });
    });
  };

  const cancelUpload = () => {
    state.controller.abort();
    dispatch({ type: 'SET_CONTROLLER', payload: new AbortController() });
  };

  return (
    <>
      {state.showUploadStatus && (
        <UploadStatusPanel
          computeTotal={computeTotal}
          retryFileUpload={retryFileUpload}
          retryAllFileUploads={retryAllFileUploads}
          retrySubmitReaction={retrySubmitReaction}
          retryAllReactionSubmissions={retryAllReactionSubmissions}
          cancelUpload={cancelUpload}
        />
      )}
      {!state.showUploadStatus && (
        <>
          <FileUploadSection />
          <GroupsSelectionSection />
          <DescriptionSection />
          <TagsSection />
          <TLPSelectionSection />
          <OriginFormTabsSection />
          <PipelinesSection />
          <UploadFormFooter onUpload={upload} />
        </>
      )}
    </>
  );
};

export const Upload: React.FC<UploadProps> = ({ entity }) => {
  return (
    <UploadFormProvider entity={entity}>
      <UploadInner entity={entity} />
    </UploadFormProvider>
  );
};
