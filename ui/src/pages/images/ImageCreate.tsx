import { useEffect, useState } from 'react';
import { useNavigate, useLocation } from 'react-router-dom';
import { Button, Row, Col } from 'react-bootstrap';
import AlertBanner from '@components/shared/alerts/AlertBanner';
import { FaAngleDown, FaAngleUp } from 'react-icons/fa';

// project imports
import {
  Fields,
  Arguments,
  Dependencies,
  Resources,
  EnvironmentVariables,
  Volumes,
  NetworkPolicies,
  SecurityContext,
  OutputCollection,
} from '@components/pages/images';
import Page from '@components/pages/Page';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import ImagePipelineEditor from '@components/shared/inputs/code/CodeEditor/ImagePipelineEditor';
import FormatToggle from '@components/shared/inputs/code/CodeEditor/FormatToggle';
import ViewModeToggle, { ViewMode } from '@components/shared/inputs/code/CodeEditor/ViewModeToggle';
import { OverlayTipRight } from '@components/shared/overlay/tips';
import { useAuth } from '@utilities/auth';
import { fetchImages, fetchGroups } from '@utilities/fetch';
import { ImageChecker } from '@utilities/rules/image';
import { imageToEditorObject, editorObjectToImageCreate } from '@utilities/transforms/image';
import { createImage } from '@thorpi/images';
import type { Image } from '@models/images';
import type { Group } from '@models/groups';
import { RoleKey } from '@models/users';
import { getThoriumRole } from '@utilities/role';
import { FormatType } from '@utilities/rules/types';

const imageChecker = new ImageChecker();

const IMAGE_CREATE_TEMPLATE: Record<string, unknown> = {
  group: '',
  name: '',
  scaler: 'K8s',
  image: '',
  timeout: 300,
  display_type: 'JSON',
};

const ImageCreate: React.FC = () => {
  const [hideAdvanced, setHideAdvanced] = useState(true);
  const [groups, setGroups] = useState<string[]>([]);
  const [images, setImages] = useState<any[]>([]);
  const [imageFields, setImageFields] = useState<Record<string, any>>({});
  const [volumes, setVolumes] = useState<any>({});
  const [environmentVars, setEnvironmentVars] = useState<{ key: string; value: string }[]>([{ key: '', value: '' }]);
  const [securityContext, setSecurityContext] = useState<Record<string, any>>({});
  const [resources, setResources] = useState<Record<string, any>>({});
  const [args, setArgs] = useState<Record<string, any>>({});
  const [argErrors, setArgErrors] = useState(false);
  const [dependencies, setDependencies] = useState<Record<string, any>>({});
  const [outputCollection, setOutputCollection] = useState<Record<string, any>>({});
  const [networkPolicies, setNetworkPolicies] = useState<any[]>([]);
  const [displayErrors, setDisplayErrors] = useState(false);
  const [imageFieldErrors, setImageFieldErrors] = useState(true);
  const [resourceErrors, setResourceErrors] = useState(false);
  const [createImageErrors, setCreateImageErrors] = useState('');
  const [dependencyErrors, setDependencyErrors] = useState(false);
  const [volumeErrors, setVolumeErrors] = useState(false);
  const [outputCollectionErrors, setOutputCollectionErrors] = useState(false);
  const navigate = useNavigate();
  const { state } = useLocation() as { state: Image | null };
  const { userInfo, checkCookie } = useAuth();
  const [loading, setLoading] = useState(false);
  let cancelUpdate = false;

  const [viewMode, setViewMode] = useState<ViewMode>(ViewMode.Form);
  const [editorObj, setEditorObj] = useState<Record<string, unknown>>(
    state ? imageToEditorObject(state as unknown as Record<string, unknown>) : IMAGE_CREATE_TEMPLATE,
  );
  const [editorFormat, setEditorFormat] = useState<FormatType>(FormatType.YAML);
  const [editorParseValid, setEditorParseValid] = useState(false);

  const handleViewModeChange = (mode: ViewMode) => {
    if (mode === viewMode) return;
    if (mode === ViewMode.Editor && viewMode === ViewMode.Form) {
      setViewMode(ViewMode.Editor);
    } else if (mode === ViewMode.Form && viewMode === ViewMode.Editor) {
      if (window.confirm('Switching to form view will reset any unsaved changes from the editor. Continue?')) {
        setViewMode(ViewMode.Form);
      }
    }
  };

  const handleEditorChange = (obj: Record<string, unknown> | null) => {
    if (obj) {
      setEditorObj(obj);
      setEditorParseValid(true);
    } else {
      setEditorParseValid(false);
    }
  };

  useEffect(() => {
    fetchGroups(setGroups as (groups: { [name: string]: Group } | Group[] | string[]) => void, null as any, false);
  }, []);

  useEffect(() => {
    const group = state && state.group ? state.group : imageFields.group;
    if (group) fetchImages([group], setImages, cancelUpdate, checkCookie, setLoading, false);
    return () => {
      cancelUpdate = true;
    };
  }, [imageFields.group]);

  useEffect(() => {
    if (!(imageFieldErrors || volumeErrors || dependencyErrors || outputCollectionErrors || resourceErrors)) {
      setCreateImageErrors('');
    }
  }, [imageFieldErrors, volumeErrors, dependencyErrors, outputCollectionErrors, resourceErrors]);

  async function handleImageCreate() {
    if (viewMode === ViewMode.Editor) {
      const data = editorObjectToImageCreate(editorObj);
      if (!data) {
        setCreateImageErrors('Image group and name are required');
        return;
      }
      if (await createImage(data, setCreateImageErrors)) {
        navigate('/images');
      }
      return;
    }

    let data: Record<string, any> = {};

    if (Object.keys(imageFields).length) {
      data = structuredClone(imageFields);
    }

    if (Object.keys(resources).length && data.scaler != 'External') {
      data['resources'] = resources;
    }

    if (Object.keys(networkPolicies).length && data.scaler == 'K8s') {
      data['network_policies'] = networkPolicies;
    }

    if (Object.keys(args).length) {
      data['args'] = args;
    }

    if (volumes && volumes.length && data.scaler == 'K8s') {
      data['volumes'] = volumes;
    }

    if (Object.keys(outputCollection).length && data.scaler != 'External') {
      data['output_collection'] = outputCollection;
    }

    if (Object.keys(dependencies).length) {
      data['dependencies'] = dependencies;
    }

    if (Object.keys(securityContext).length && userInfo && getThoriumRole(userInfo.role) == RoleKey.Admin && data.scaler != 'External') {
      data['security_context'] = securityContext;
    }

    const environmentVarsJson: Record<string, string | null> = {};
    if (environmentVars) {
      environmentVars.map((variable) => {
        if (variable['key']) {
          environmentVarsJson[variable['key']] = variable['value'] == '' ? null : variable['value'];
        }
      });
      if (Object.keys(environmentVarsJson).length && (data.scaler == 'K8s' || data.scaler == 'BareMetal')) {
        data['env'] = environmentVarsJson;
      }
    }

    if (imageFieldErrors || resourceErrors || argErrors || outputCollectionErrors || dependencyErrors || volumeErrors) {
      setCreateImageErrors('Please resolve missing fields or invalid entries');
      setDisplayErrors(true);
      return;
    } else {
      setCreateImageErrors('');
    }

    if (await createImage(data, setCreateImageErrors)) {
      navigate('/images');
    } else {
      setDisplayErrors(true);
    }
  }

  return (
    <Page className="image-create" title="Create Image">
      <Row>
        <center>
          <h3>Create An Image</h3>
        </center>
      </Row>
      <Row className="mt-2 mb-3">
        <Col className="d-flex justify-content-center">
          <ViewModeToggle viewMode={viewMode} onViewModeChange={handleViewModeChange} />
        </Col>
      </Row>
      {viewMode === ViewMode.Editor ? (
        <>
          <Row className="mb-2">
            <Col>
              <FormatToggle format={editorFormat} onFormatChange={setEditorFormat} />
            </Col>
          </Row>
          <ImagePipelineEditor
            key={editorFormat}
            value={editorObj}
            onChange={handleEditorChange}
            checker={imageChecker}
            format={editorFormat}
            height="600px"
          />
        </>
      ) : (
        <>
          <Row className="mt-4">
            <Col className="title-col">
              <h5>Image</h5>
            </Col>
            <Col className="field-col">
              <Fields
                image={state ? state : imageFields}
                groups={groups ? groups : []}
                setRequestImageFields={setImageFields}
                setHasErrors={setImageFieldErrors}
                showErrors={state ? true : displayErrors}
                mode={state ? 'Copy' : 'Create'}
              />
            </Col>
          </Row>
          <Row>
            <Col className="d-flex justify-content-center">
              <OverlayTipRight tip={`${hideAdvanced ? 'Expand' : 'Hide'} optional fields`}>
                <div className="icon-btn" onClick={() => setHideAdvanced(!hideAdvanced)}>
                  {hideAdvanced ? <FaAngleDown size="36" /> : <FaAngleUp size="36" />}
                </div>
              </OverlayTipRight>
            </Col>
          </Row>
          <hr className="mt-0" />
          <div className={`${hideAdvanced ? 'advanced-hidden' : ''}`}>
            <Resources
              resources={state && state.resources ? state.resources : {}}
              setRequestResources={setResources}
              setHasErrors={setResourceErrors}
              mode={state ? 'Copy' : 'Create'}
            />
            <hr />
            <Arguments
              args={state && state.args ? state.args : {}}
              setRequestArguments={setArgs}
              setHasErrors={setArgErrors}
              mode={state ? 'Copy' : 'Create'}
            />
            <hr />
            <OutputCollection
              outputCollection={state && state.output_collection ? state.output_collection : {}}
              setRequestOutputCollection={setOutputCollection}
              groups={userInfo && userInfo.groups ? userInfo.groups : []}
              mode={state ? 'Copy' : 'Create'}
              setHasErrors={setOutputCollectionErrors}
              disabled={imageFields['scaler'] && imageFields.scaler == 'External'}
            />
            <hr />
            <Dependencies
              images={images}
              dependencies={state && state.dependencies ? state.dependencies : {}}
              setRequestDependencies={setDependencies}
              mode={state ? 'Copy' : 'Create'}
              disabled={imageFields['scaler'] && imageFields.scaler == 'External'}
            />
            <hr />
            <EnvironmentVariables
              environmentVars={state && state.env ? state.env : {}}
              setRequestEnvironmentVars={setEnvironmentVars}
              mode={state ? 'Copy' : 'Create'}
            />
            <hr />
            <Volumes
              volumes={state && state.volumes ? state.volumes : []}
              setRequestVolumes={setVolumes}
              mode={state ? 'Copy' : 'Create'}
              setHasErrors={setVolumeErrors}
              disabled={imageFields['scaler'] && imageFields.scaler != 'K8s'}
            />
            <hr />
            <NetworkPolicies
              policies={state && state.network_policies ? state.network_policies : {}}
              setRequestNetworkPolicies={setNetworkPolicies}
              mode={state ? 'Copy' : 'Create'}
            />
            <hr />
            <SecurityContext
              securityContext={state && state.security_context ? state.security_context : {}}
              setRequestSecurityContext={setSecurityContext}
              mode={state ? 'Copy' : 'Create'}
              disabled={
                (imageFields['scaler'] && imageFields.scaler == 'External') || !userInfo || getThoriumRole(userInfo.role) != RoleKey.Admin
              }
            />
          </div>
        </>
      )}
      <Row className="d-flex justify-content-center">
        <Col>{createImageErrors && <AlertBanner className="m-2">{createImageErrors}</AlertBanner>}</Col>
      </Row>
      <Row>
        <LoadingSpinner loading={loading}></LoadingSpinner>
      </Row>
      <Row className="mt-3">
        <Col className="d-flex justify-content-center">
          <Button className="secondary-btn" onClick={() => navigate(-1)}>
            Cancel
          </Button>
          <Button className="ok-btn" disabled={viewMode === ViewMode.Editor && !editorParseValid} onClick={() => handleImageCreate()}>
            Create
          </Button>
        </Col>
      </Row>
    </Page>
  );
};

export default ImageCreate;
