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
import ImagePipelineEditor from '@components/shared/inputs/code/ImagePipelineEditor';
import FormatToggle from '@components/shared/inputs/code/FormatToggle';
import ViewModeToggle from '@components/shared/inputs/code/ViewModeToggle';
import { OverlayTipRight } from '@components/shared/overlay/tips';
import { useAuth } from '@utilities/auth';
import { fetchImages, fetchGroups } from '@utilities/fetch';
import { ImageChecker } from '@utilities/rules/image';
import { imageToEditorObject, editorObjectToImageCreate } from '@utilities/transforms/image';
import { createImage } from '@thorpi/images';

const imageChecker = new ImageChecker();

const IMAGE_CREATE_TEMPLATE = {
  group: '',
  name: '',
  scaler: 'K8s',
  image: '',
  timeout: 300,
  display_type: 'JSON',
};

const ImageCreate = () => {
  const [hideAdvanced, setHideAdvanced] = useState(true);
  // Image set state functions
  const [groups, setGroups] = useState([]);
  const [images, setImages] = useState([]);
  const [imageFields, setImageFields] = useState({});
  const [volumes, setVolumes] = useState({}); // optional
  const [environmentVars, setEnvironmentVars] = useState([{ key: '', value: '' }]); // optional
  const [securityContext, setSecurityContext] = useState({}); // optional
  const [resources, setResources] = useState({}); // optional
  const [args, setArgs] = useState({}); // optional
  const [argErrors, setArgErrors] = useState(false);
  const [dependencies, setDependencies] = useState({});
  const [outputCollection, setOutputCollection] = useState({});
  const [networkPolicies, setNetworkPolicies] = useState([]); // optional
  // Set error state functions
  const [displayErrors, setDisplayErrors] = useState(false);
  // required fields are blank at start
  const [imageFieldErrors, setImageFieldErrors] = useState(true); // this is true at start
  const [resourceErrors, setResourceErrors] = useState(false);
  const [createImageErrors, setCreateImageErrors] = useState('');
  const [dependencyErrors, setDependencyErrors] = useState(false);
  const [volumeErrors, setVolumeErrors] = useState(false);
  const [outputCollectionErrors, setOutputCollectionErrors] = useState(false);
  const navigate = useNavigate();
  const { state } = useLocation();
  const { userInfo, checkCookie } = useAuth();
  const [loading, setLoading] = useState(false);
  let cancelUpdate = false;

  // Editor mode state
  const [viewMode, setViewMode] = useState('form');
  const [editorObj, setEditorObj] = useState(state ? imageToEditorObject(state) : IMAGE_CREATE_TEMPLATE);
  const [editorFormat, setEditorFormat] = useState('yaml');
  const [editorParseValid, setEditorParseValid] = useState(false);

  const handleViewModeChange = (mode) => {
    if (mode === viewMode) return;
    if (mode === 'editor' && viewMode === 'form') {
      setViewMode('editor');
    } else if (mode === 'form' && viewMode === 'editor') {
      if (window.confirm('Switching to form view will reset any unsaved changes from the editor. Continue?')) {
        setViewMode('form');
      }
    }
  };

  const handleEditorChange = (obj) => {
    if (obj) {
      setEditorObj(obj);
      setEditorParseValid(true);
    } else {
      setEditorParseValid(false);
    }
  };

  // need user's group roles to validate permissions to create/edit/delete pipelines
  useEffect(() => {
    fetchGroups(setGroups, null, false);
  }, []);

  // get list of images for the selected group for use in dependencies drop downs
  useEffect(() => {
    // if state is passed, use that
    const group = state && state.group ? state.group : imageFields.group;
    if (group) fetchImages([group], setImages, cancelUpdate, checkCookie, setLoading, false);
    return () => {
      cancelUpdate = true;
    };
  }, [imageFields.group]);

  // clear the create error if all others have been resolved
  useEffect(() => {
    if (!(imageFieldErrors || volumeErrors || dependencyErrors || outputCollectionErrors || resourceErrors)) {
      setCreateImageErrors('');
    }
  }, [imageFieldErrors, volumeErrors, dependencyErrors, outputCollectionErrors, resourceErrors]);

  async function handleImageCreate() {
    // Editor mode: submit from editor object
    if (viewMode === 'editor') {
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

    // Form mode: existing behavior
    let data = {};

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

    if (Object.keys(securityContext).length && userInfo && userInfo.role == 'Admin' && data.scaler != 'External') {
      data['security_context'] = securityContext;
    }

    const environmentVarsJson = {};
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
      {viewMode === 'editor' ? (
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
              setErrors={setDependencyErrors}
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
              disabled={(imageFields['scaler'] && imageFields.scaler == 'External') || !userInfo || userInfo.role != 'Admin'}
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
          <Button
            className="ok-btn"
            disabled={viewMode === 'editor' && !editorParseValid}
            onClick={() => handleImageCreate()}
          >
            Create
          </Button>
        </Col>
      </Row>
    </Page>
  );
};

export default ImageCreate;
