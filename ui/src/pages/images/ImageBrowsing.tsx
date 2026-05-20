import { useEffect, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { Accordion, Badge, Button, ButtonToolbar, Col, Form, Modal, Row } from 'react-bootstrap';
import AlertBanner from '@components/shared/alerts/AlertBanner';

// project imports
import {
  Fields,
  NetworkPolicies,
  Resources,
  Arguments,
  OutputCollection,
  Dependencies,
  EnvironmentVariables,
  Volumes,
  SecurityContext,
} from '@components/pages/images';
import Page from '@components/pages/Page';
import Title from '@components/shared/titles/Title';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import ImagePipelineEditor from '@components/shared/inputs/code/CodeEditor/ImagePipelineEditor';
import FormatToggle from '@components/shared/inputs/code/CodeEditor/FormatToggle';
import ViewModeToggle from '@components/shared/inputs/code/CodeEditor/ViewModeToggle';
import type { ViewMode } from '@components/shared/inputs/code/CodeEditor/ViewModeToggle';
import { OverlayTipRight, OverlayTipLeft, OverlayTipBottom } from '@components/shared/overlay/tips';
import { getGroupRole, getThoriumRole } from '@utilities/role';
import { fetchImages, fetchSingleImage, fetchGroups } from '@utilities/fetch';
import { useAuth } from '@utilities/auth';
import { ImageChecker } from '@utilities/rules/image';
import { imageToEditorObject, editorObjectToImageUpdate } from '@utilities/transforms/image';
import { deleteImage, updateImage } from '@thorpi/images';
import type { Image } from '@models/images';
import type { Group } from '@models/groups';
import { RoleKey } from '@models/users';
import { FormatType } from '@utilities/rules/types';

const imageChecker = new ImageChecker();

const ImageBrowsing: React.FC = () => {
  const [loading, setLoading] = useState(false);
  const [images, setImages] = useState<Image[]>([]);
  const [groups, setGroups] = useState<Record<string, Group>>({});
  const { userInfo, checkCookie } = useAuth();
  let cancelUpdate = false;

  useEffect(() => {
    fetchGroups(setGroups as (groups: { [name: string]: Group } | Group[] | string[]) => void, null as any, true);
  }, []);

  useEffect(() => {
    if (groups && Object.keys(groups).length) {
      fetchImages(Object.keys(groups), setImages, cancelUpdate, checkCookie, setLoading, true);
    }
    return () => {
      cancelUpdate = true;
    };
  }, [groups]);

  const CreateImage: React.FC = () => {
    const navigate = useNavigate();
    const userCanCreateImage = userInfo
      ? ([RoleKey.Developer, RoleKey.Analyst, RoleKey.Admin] as string[]).includes(getThoriumRole(userInfo.role))
      : false;
    const CreateImageMessage = userCanCreateImage
      ? `Create a new Image. You must be a
    Thorium developer, analyst, or admin to create an image.`
      : `You must be a Thorium developer or
    admin to create an image.`;

    return (
      <OverlayTipBottom tip={CreateImageMessage}>
        <Button
          className="ok-btn m-1 d-flex justify-content-center"
          disabled={!userCanCreateImage}
          onClick={() => navigate('/create/image')}
        >
          <b>+</b>
        </Button>
      </OverlayTipBottom>
    );
  };

  const ImageCountTipMessage =
    userInfo && getThoriumRole(userInfo.role) == RoleKey.Admin
      ? `There are a total of ${images.length} Thorium images.`
      : `There are a total of ${images.length} Thorium images owned by your groups.`;

  return (
    <Page title="Images · Thorium">
      <Row>
        <Col>
          <h2>
            <OverlayTipRight tip={ImageCountTipMessage}>
              <Badge bg="" className="count-badge">
                {images.length}
              </Badge>
            </OverlayTipRight>
          </h2>
        </Col>
        <Col className="d-flex justify-content-center">
          <Title>Images</Title>
        </Col>
        <Col className="d-flex justify-content-end">
          <CreateImage />
        </Col>
      </Row>
      <LoadingSpinner loading={loading}></LoadingSpinner>
      <Accordion alwaysOpen>
        {images.map((image) => (
          <Accordion.Item key={`${image.name}_${image.group}`} eventKey={`${image.name}_${image.group}`}>
            <Accordion.Header className="d-flex">
              <Col className="accordion-item-name">
                <div className="text">{image.name}</div>
              </Col>
              <Col className="accordion-item-relation" />
              <Col className="accordion-item-ownership">
                <OverlayTipLeft tip={`This image is owned by the ${image.group} group.`}>
                  <small>
                    <i>{image.group}</i>
                  </small>
                </OverlayTipLeft>
              </Col>
            </Accordion.Header>
            <Accordion.Body>
              <ImageInfo images={images} image={image} groups={groups} setImages={setImages} />
            </Accordion.Body>
          </Accordion.Item>
        ))}
      </Accordion>
    </Page>
  );
};

interface ImageInfoProps {
  images: Image[];
  image: Image;
  groups: Record<string, Group>;
  setImages: (images: Image[]) => void;
}

const ImageInfo: React.FC<ImageInfoProps> = ({ images, image, groups, setImages }) => {
  const [inEditMode, setEditMode] = useState(false);
  const [updateError, setUpdateError] = useState('');
  const [loading, setLoading] = useState(false);
  const [imageFields, setImageFields] = useState<Record<string, any>>({});
  const [stringFieldsError, setStringFieldsError] = useState(false);
  const [resources, setResources] = useState<Record<string, any>>({});
  const [resourceError, setResourceError] = useState(false);
  const [args, setArgs] = useState<Record<string, any>>({});
  const [argError, setArgError] = useState(false);
  const [volumes, setVolumes] = useState<any>([]);
  const [volumesFieldsError, setVolumeFieldsError] = useState(false);
  const [dependencies, setDependencies] = useState<Record<string, any>>({});
  const [dependenciesFieldsError, setDependenciesFieldError] = useState(false);
  const [outputCollection, setOutputCollection] = useState<Record<string, any>>({});
  const [outputCollectionError, setOutputCollectionError] = useState(false);
  const [currentImage, setCurrentImage] = useState<Image>(image);
  const [securityContext, setSecurityContext] = useState<Record<string, any>>({});
  const [environmentVars, setEnvironmentVars] = useState<any>([{ key: '', value: '' }]);
  const [networkPolicies, setNetworkPolicies] = useState<any[]>([]);
  const { userInfo, checkCookie } = useAuth();

  const [viewMode, setViewMode] = useState<ViewMode>('form');
  const [editorObj, setEditorObj] = useState<Record<string, unknown> | null>(null);
  const [editorFormat, setEditorFormat] = useState<FormatType>(FormatType.YAML);
  const [editorParseValid, setEditorParseValid] = useState(false);

  const handleViewModeChange = (mode: ViewMode) => {
    if (mode === viewMode) return;
    if (mode === 'editor') {
      setEditorObj(imageToEditorObject(currentImage as unknown as Record<string, unknown>));
      setEditorParseValid(true);
      setViewMode('editor');
    } else {
      if (window.confirm('Switching to form view will reset any unsaved changes from the editor. Continue?')) {
        setViewMode('form');
        setEditorObj(null);
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

  const thoriumRole = userInfo ? getThoriumRole(userInfo.role) : ('' as RoleKey);
  const groupRole = userInfo ? getGroupRole(groups[image.group], userInfo.username) : '';
  const userCanModify =
    ((userInfo !== null && image.creator == userInfo.username) || ['Manager', 'Owner'].includes(groupRole)) &&
    thoriumRole == RoleKey.Developer
      ? true
      : thoriumRole == RoleKey.Admin;
  const userCanDelete =
    (userInfo !== null && image.creator == userInfo.username) || ['Manager', 'Owner'].includes(groupRole) || thoriumRole == RoleKey.Admin;
  const userCanCreateImage =
    (['User', 'Manager', 'Owner'].includes(groupRole) && thoriumRole == RoleKey.Developer) || thoriumRole == RoleKey.Admin;

  useEffect(() => {
    if (!(stringFieldsError || resourceError || argError || volumesFieldsError || dependenciesFieldsError || outputCollectionError)) {
      setUpdateError('');
    }
  }, [stringFieldsError, resourceError, argError, volumesFieldsError, dependenciesFieldsError, outputCollectionError]);

  const sendFieldsUpdate = async (image: Image) => {
    if (viewMode === 'editor') {
      const result = editorObjectToImageUpdate(editorObj!, currentImage as unknown as Record<string, unknown>);
      if (!result) {
        setUpdateError('Invalid image data');
        return;
      }
      if (await updateImage(result.group, result.name, result.data, setUpdateError)) {
        fetchSingleImage(image, setCurrentImage, setLoading);
        setEditMode(false);
        setViewMode('form');
        setEditorObj(null);
        setUpdateError('');
      }
      return;
    }

    let newFields: Record<string, any> = {};

    if (stringFieldsError || resourceError || argError || volumesFieldsError || dependenciesFieldsError || outputCollectionError) {
      setUpdateError('Please resolve missing fields or invalid entries');
      return;
    } else setUpdateError('');

    if (Object.keys(imageFields).length) {
      newFields = { ...imageFields };
    }
    if (Object.keys(resources).length) {
      newFields['resources'] = resources;
    }
    if (Object.keys(args).length) {
      newFields['args'] = args;
    }
    if (volumes.remove_volumes.length > 0) {
      newFields = { ...newFields, remove_volumes: volumes.remove_volumes };
    }
    if (volumes.add_volumes.length > 0) {
      newFields = { ...newFields, add_volumes: volumes.add_volumes };
    }
    if (Object.keys(dependencies).length) {
      const tempdepend = { dependencies: dependencies };
      newFields = { ...newFields, ...tempdepend };
    }
    if (Object.keys(outputCollection).length) {
      const output = { output_collection: outputCollection };
      newFields = { ...newFields, ...output };
    }
    if (Object.keys(securityContext).length && userInfo && getThoriumRole(userInfo.role) == RoleKey.Admin) {
      const tempSecContext = { security_context: securityContext };
      newFields = { ...newFields, ...tempSecContext };
    }
    if (Object.keys(environmentVars).length) {
      newFields = { ...newFields, ...environmentVars };
    }
    if (!Array.isArray(networkPolicies)) {
      newFields['network_policies'] = networkPolicies;
    }

    if (Object.keys(newFields).length) {
      if (await updateImage(image.group, image.name, newFields, setUpdateError)) {
        fetchSingleImage(image, setCurrentImage, setLoading);
        setEditMode(false);
        setUpdateError('');
      }
    }
  };

  const DeleteImageButton: React.FC<{ image: Image }> = ({ image }) => {
    const [showDeleteModal, setShowDeleteModal] = useState(false);
    const [deleteError, setDeleteError] = useState('');
    const handleCloseDeleteModal = () => {
      setShowDeleteModal(false);
      setDeleteError('');
    };
    const handleShowDeleteModal = () => setShowDeleteModal(true);
    return (
      <>
        <OverlayTipLeft
          tip={`Delete this Thorium image. Only admins, group owners/managers, or the image
          creator can delete an image.`}
        >
          <Button className="warning-btn" onClick={handleShowDeleteModal}>
            Delete
          </Button>
        </OverlayTipLeft>
        <Modal show={showDeleteModal} backdrop="static" onHide={handleCloseDeleteModal}>
          <Modal.Header closeButton>
            <Modal.Title>Confirm deletion?</Modal.Title>
          </Modal.Header>
          <Modal.Body>
            Do you really want to delete the <b>{image.name}</b> image?
            {deleteError != '' && <AlertBanner className="mt-4">{deleteError}</AlertBanner>}
          </Modal.Body>
          <Modal.Footer className="d-flex justify-content-center">
            <Button
              className="danger-btn"
              onClick={async () => {
                if (await deleteImage(image.group, image.name, setDeleteError)) {
                  fetchImages(Object.keys(groups), setImages, false, checkCookie, null as any, true);
                  handleCloseDeleteModal();
                }
              }}
            >
              Confirm
            </Button>
          </Modal.Footer>
        </Modal>
      </>
    );
  };

  const CopyImageButton: React.FC<{ image: Image }> = ({ image }) => {
    const navigate = useNavigate();
    return (
      <OverlayTipBottom tip={`Click to create a new image using ${image.name} as a template.`}>
        <Button
          className="ok-btn d-flex justify-content-center"
          disabled={!userCanCreateImage}
          onClick={() => navigate('/create/image', { state: image })}
        >
          Copy
        </Button>
      </OverlayTipBottom>
    );
  };

  const groupImages = images.filter((someImage) => currentImage.group == someImage.group);
  let imageNames: string[] = groupImages.map((image) => {
    return image.name;
  });
  imageNames = [...new Set(imageNames)];

  return (
    <Form>
      {inEditMode && (
        <Row className="mb-3">
          <Col className="d-flex justify-content-center">
            <ViewModeToggle viewMode={viewMode} onViewModeChange={handleViewModeChange} />
          </Col>
        </Row>
      )}
      {inEditMode && viewMode === 'editor' ? (
        <>
          <Row className="mb-2">
            <Col>
              <FormatToggle format={editorFormat} onFormatChange={setEditorFormat} />
            </Col>
          </Row>
          <ImagePipelineEditor
            key={editorFormat}
            value={editorObj || {}}
            onChange={handleEditorChange}
            checker={imageChecker}
            format={editorFormat}
            height="600px"
          />
        </>
      ) : (
        <>
          <Fields
            image={currentImage}
            groups={[]}
            setRequestImageFields={setImageFields}
            showErrors={true}
            setHasErrors={setStringFieldsError}
            mode={inEditMode ? 'Edit' : 'View'}
          />
          <Resources
            resources={currentImage.resources ? currentImage.resources : {}}
            setRequestResources={setResources}
            setHasErrors={setResourceError}
            mode={inEditMode ? 'Edit' : 'View'}
          />
          <Arguments
            args={currentImage.args ? currentImage.args : {}}
            setRequestArguments={setArgs}
            setHasErrors={setArgError}
            mode={inEditMode ? 'Edit' : 'View'}
          />
          <OutputCollection
            outputCollection={currentImage.output_collection}
            setRequestOutputCollection={setOutputCollection}
            groups={userInfo && userInfo.groups ? userInfo.groups : []}
            mode={inEditMode ? 'Edit' : 'View'}
            setHasErrors={setOutputCollectionError}
            disabled={currentImage.scaler == 'External'}
          />
          <Dependencies
            dependencies={currentImage.dependencies ? currentImage.dependencies : {}}
            images={imageNames}
            mode={inEditMode ? 'Edit' : 'View'}
            setRequestDependencies={setDependencies}
            disabled={currentImage.scaler == 'External'}
          />
          <EnvironmentVariables
            environmentVars={currentImage.env ? currentImage.env : {}}
            setRequestEnvironmentVars={setEnvironmentVars}
            mode={inEditMode ? 'Edit' : 'View'}
          />
          <Volumes
            volumes={currentImage.volumes}
            setRequestVolumes={setVolumes}
            mode={inEditMode ? 'Edit' : 'View'}
            setHasErrors={setVolumeFieldsError}
            disabled={currentImage.scaler != 'K8s'}
          />
          {currentImage['scaler'] == 'K8s' && (
            <NetworkPolicies
              policies={currentImage.network_policies ? currentImage.network_policies : []}
              setRequestNetworkPolicies={setNetworkPolicies}
              mode={inEditMode ? 'Edit' : 'View'}
            />
          )}
          <SecurityContext
            securityContext={currentImage.security_context ? currentImage.security_context : {}}
            setRequestSecurityContext={setSecurityContext}
            mode={inEditMode && userInfo && getThoriumRole(userInfo.role) == RoleKey.Admin ? 'Edit' : 'View'}
            disabled={
              (Object.keys(imageFields).includes('scaler') && imageFields.scaler == 'External') ||
              !userInfo ||
              getThoriumRole(userInfo.role) != RoleKey.Admin
            }
          />
        </>
      )}
      <Row className="d-flex justify-content-center">
        <Col>{updateError && <AlertBanner className="m-2">{updateError}</AlertBanner>}</Col>
      </Row>
      <Row>
        <Col>
          <LoadingSpinner loading={loading}></LoadingSpinner>
          <ButtonToolbar className="d-flex justify-content-center">
            {userCanCreateImage && !inEditMode && <CopyImageButton image={currentImage} />}
            {userCanModify && !inEditMode && (
              <OverlayTipRight
                tip={`Update this image. Only Thorium admins or
              developers with group permissions may edit images.`}
              >
                <Button className="secondary-btn" onClick={() => setEditMode(true)}>
                  Edit
                </Button>
              </OverlayTipRight>
            )}
            {userCanDelete && !inEditMode && <DeleteImageButton image={image} />}
            {userCanModify && inEditMode && (
              <OverlayTipLeft tip={`Cancel pending updates.`}>
                <Button
                  className="primary-btn"
                  onClick={() => {
                    setEditMode(false);
                    setViewMode('form');
                    setEditorObj(null);
                    setUpdateError('');
                  }}
                >
                  Cancel
                </Button>
              </OverlayTipLeft>
            )}
            {userCanModify && inEditMode && (
              <OverlayTipRight tip={`Submit pending updates.`}>
                <Button className="ok-btn" disabled={viewMode === 'editor' && !editorParseValid} onClick={() => sendFieldsUpdate(image)}>
                  Update
                </Button>
              </OverlayTipRight>
            )}
          </ButtonToolbar>
        </Col>
      </Row>
    </Form>
  );
};

export default ImageBrowsing;
