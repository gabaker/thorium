import { Fragment, useEffect, useState } from 'react';
import { Accordion, Badge, Button, ButtonToolbar, ButtonGroup, Col, Form, Modal, Row } from 'react-bootstrap';
import AlertBanner from '@components/shared/alerts/AlertBanner';
import { FaQuestionCircle } from 'react-icons/fa';
import { default as MarkdownHtml } from 'react-markdown';
import remarkGfm from 'remark-gfm';

// project imports
import Page from '@components/pages/Page';
import Title from '@components/shared/titles/Title';
import FieldBadge from '@components/shared/badges/FieldBadge';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import SimpleSubtitle from '@components/shared/titles/SimpleSubtitle';
import ImagePipelineEditor from '@components/shared/inputs/code/ImagePipelineEditor';
import FormatToggle from '@components/shared/inputs/code/FormatToggle';
import { OverlayTipBottom, OverlayTipLeft, OverlayTipRight } from '@components/shared/overlay/tips';
import { orderComparePipeline } from '@components/pages/files/reactions/pipelines';
import { useAuth } from '@utilities/auth';
import { getThoriumRole, getGroupRole } from '@utilities/role';
import { fetchGroups } from '@utilities/fetch';
import { PipelineChecker } from '@utilities/rules/image';
import { pipelineToEditorObject, editorObjectToPipelineCreate, editorObjectToPipelineUpdate } from '@utilities/transforms/pipeline';
import { createPipeline, deletePipeline, listPipelines, updatePipeline } from '@thorpi/pipelines';

const pipelineChecker = new PipelineChecker();

const PIPELINE_CREATE_TEMPLATE = {
  group: '',
  name: '',
  order: [],
  sla: 604800,
  description: '',
};

const Pipelines = () => {
  const [loading, setLoading] = useState(false);
  const [pipelines, setPipelines] = useState([]);
  const [groups, setGroups] = useState({});
  const { userInfo, checkCookie } = useAuth();

  // get detailed pipeline info for pipelines in each group
  const fetchPipelines = async () => {
    setLoading(true);
    const allPipelines = [];
    // loop through each group to get owned pipelines
    for (const group of Object.keys(groups)) {
      const groupPipelines = await listPipelines(group, checkCookie, true, null, 1000);
      if (groupPipelines) {
        allPipelines.push(...groupPipelines);
      }
    }
    setPipelines(allPipelines);
    setLoading(false);
  };

  // need user's group roles to validate permissions to create/edit/delete pipelines
  useEffect(() => {
    fetchGroups(setGroups, null, true);
  }, []);

  // need groups to get a list of pipelines
  useEffect(() => {
    fetchPipelines();
  }, [groups]);

  async function handlePipelineUpdate(editorObj, originalPipeline, setUpdateError) {
    const result = editorObjectToPipelineUpdate(editorObj, originalPipeline);
    if (!result) {
      setUpdateError('Invalid pipeline data');
      return;
    }
    if (await updatePipeline(result.group, result.name, result.data, setUpdateError)) {
      fetchPipelines();
    }
  }

  // Display the delete pipeline button and implement deletion
  const DeletePipelineButton = ({ pipeline }) => {
    const [showDeleteModal, setShowDeleteModal] = useState(false);
    const [deleteError, setDeleteError] = useState('');
    const handleCloseDeleteModal = () => {
      setShowDeleteModal(false);
      setDeleteError('');
    };
    const handleShowDeleteModal = () => setShowDeleteModal(true);

    return (
      <ButtonGroup className="d-flex justify-content-center">
        <OverlayTipBottom
          tip={`Delete this pipeline. Only Thorium admins, group owners/managers,
            or the pipeline's creator can delete a pipeline.`}
        >
          <Button className="warning-btn" onClick={handleShowDeleteModal}>
            Delete
          </Button>
        </OverlayTipBottom>
        <Modal show={showDeleteModal} onHide={handleCloseDeleteModal} keyboard={false}>
          <Modal.Header closeButton>
            <Modal.Title>Confirm deletion?</Modal.Title>
          </Modal.Header>
          <Modal.Body>
            Do you really want to delete the <b>{pipeline.name}</b> pipeline?
            {deleteError != '' && <AlertBanner className="mt-4">{deleteError}</AlertBanner>}
          </Modal.Body>
          <Modal.Footer className="d-flex justify-content-center">
            <Button
              className="danger-btn"
              onClick={async () => {
                if (await deletePipeline(pipeline.group, pipeline.name, setDeleteError)) {
                  fetchPipelines();
                }
              }}
            >
              Confirm
            </Button>
          </Modal.Footer>
        </Modal>
      </ButtonGroup>
    );
  };

  const PipelineCountTipMessage =
    getThoriumRole(userInfo ? userInfo.role : '') == 'Admin'
      ? `There are a total of ${pipelines.length} Thorium pipelines.`
      : `There are a total of ${pipelines.length} Thorium pipelines owned by your groups.`;

  // Display pipeline accordion page headers
  const PipelineHeader = () => {
    return (
      <div className="d-flex justify-content-between ">
        <div>
          <h2>
            <OverlayTipRight tip={PipelineCountTipMessage}>
              <Badge bg="" className="count-badge">
                {pipelines.length}
              </Badge>
            </OverlayTipRight>
          </h2>
        </div>
        <Title>Pipelines</Title>
        <div>
          <h2>
            <CreatePipeline />
          </h2>
        </div>
      </div>
    );
  };

  const PipelineInfo = ({ pipeline }) => {
    const [updateError, setUpdateError] = useState('');
    const [inEditMode, setInEditMode] = useState(false);
    const [editorObj, setEditorObj] = useState(null);
    const [format, setFormat] = useState('yaml');
    const [parseValid, setParseValid] = useState(false);
    const pipelineTriggers = pipeline.triggers;

    const groupRole = getGroupRole(groups[pipeline.group], userInfo.username);
    const thoriumRole = getThoriumRole(userInfo.role);
    const userCanModify =
      (((userInfo !== null && pipeline.creator == userInfo.username) || ['Manager', 'Owner'].includes(groupRole)) &&
        thoriumRole == 'Developer') ||
      thoriumRole == 'Admin';
    const userCanDelete = pipeline.creator == userInfo.username || ['Manager', 'Owner'].includes(groupRole) || thoriumRole == 'Admin';

    const enterEditMode = () => {
      setEditorObj(pipelineToEditorObject(pipeline));
      setParseValid(true);
      setInEditMode(true);
    };

    const exitEditMode = () => {
      setInEditMode(false);
      setEditorObj(null);
      setUpdateError('');
    };

    const handleEditorChange = (obj) => {
      if (obj) {
        setEditorObj(obj);
        setParseValid(true);
      } else {
        setParseValid(false);
      }
    };

    const pipelineDescription = pipeline.description ? pipeline.description : '';
    const pipelineOrder = JSON.stringify(pipeline.order, null, '');

    return (
      <Form>
        {inEditMode ? (
          <>
            <Row className="mb-2">
              <Col>
                <FormatToggle format={format} onFormatChange={setFormat} />
              </Col>
            </Row>
            <ImagePipelineEditor
              key={format}
              value={editorObj || {}}
              onChange={handleEditorChange}
              checker={pipelineChecker}
              format={format}
              height="400px"
            />
          </>
        ) : (
          <>
            <Row>
              <Col className="pipeline-header-col">
                <SimpleSubtitle>
                  <b>Creator</b>
                </SimpleSubtitle>
              </Col>
              <Col className="pipeline-detail-col">
                <Badge bg="" className="bg-blue">
                  {pipeline.creator}
                </Badge>
              </Col>
            </Row>
            <Row className="mt-2">
              <Col className="pipeline-header-col">
                <SimpleSubtitle>
                  <b>Description</b>
                </SimpleSubtitle>
              </Col>
              <Col className="pipeline-detail-col">
                <MarkdownHtml remarkPlugins={[remarkGfm]}>{pipelineDescription}</MarkdownHtml>
              </Col>
            </Row>
            <Row className="mt-2">
              <Col className="pipeline-header-col">
                <OverlayTipRight
                  tip={`The order of images to run. Image order must be
                  formatted as a JSON array of strings and/or string arrays.`}
                >
                  <SimpleSubtitle>
                    <b>Order</b> <FaQuestionCircle />
                  </SimpleSubtitle>
                </OverlayTipRight>
              </Col>
              <Col className="pipeline-detail-col">
                <p>{pipelineOrder.toString()}</p>
              </Col>
            </Row>
            <Row className="mt-2">
              <Col className="pipeline-header-col">
                <OverlayTipRight tip={`The length of the SLA in seconds.`}>
                  <SimpleSubtitle>
                    <b>SLA</b> <FaQuestionCircle />
                  </SimpleSubtitle>
                </OverlayTipRight>
              </Col>
              <Col className="pipeline-detail-col">
                <p>{pipeline.sla}</p>
              </Col>
            </Row>
            <Row className="mt-2">
              <Col className="pipeline-header-col">
                <OverlayTipRight
                  tip={`Automatic triggers that will cause this pipeline to run.
                    Events can be configured to trigger when samples are initially uploaded or
                    upon the creation of metadata tags.`}
                >
                  <b>Event Triggers</b> <FaQuestionCircle />
                </OverlayTipRight>
              </Col>
              {Object.keys(pipelineTriggers).length == 0 && (
                <Col className="pipeline-detail-col">
                  <FieldBadge field={'None'} color={'#7e7c7c'} />
                </Col>
              )}
            </Row>
            {Object.keys(pipelineTriggers).length > 0 &&
              Object.keys(pipelineTriggers).map((triggerName, idx) => (
                <div key={triggerName}>
                  <Row>
                    <Col className="trigger-indent" />
                    <Col className="trigger-field">
                      <em>Trigger Name:</em>
                    </Col>
                    <Col className="trigger-value">
                      <FieldBadge field={triggerName} color={'#7e7c7c'} />
                    </Col>
                  </Row>
                  {Object.keys(pipelineTriggers[triggerName]).length > 0 &&
                    Object.keys(pipelineTriggers[triggerName]).includes('Tag') && (
                      <>
                        <Row>
                          <Col className="trigger-indent" />
                          <Col className="trigger-field">
                            <em>Trigger Type:</em>
                          </Col>
                          <Col className="trigger-value">
                            <FieldBadge field={'Tag'} color={'#7e7c7c'} />
                          </Col>
                        </Row>
                        <Row>
                          <Col className="trigger-indent" />
                          <Col className="trigger-field">
                            <em>Tag Types:</em>
                          </Col>
                          <Col className="trigger-value">
                            <FieldBadge field={pipelineTriggers[triggerName]['Tag']['tag_types']} color={'#7e7c7c'} />
                          </Col>
                        </Row>
                        <Row>
                          <Col className="trigger-indent" />
                          <Col className="trigger-field">
                            <em>Required:</em>
                          </Col>
                          <Col className="trigger-value">
                            {Object.keys(pipelineTriggers[triggerName]['Tag']['required']).length == 0 && (
                              <FieldBadge field={'None'} color={'#7e7c7c'} />
                            )}
                            {Object.keys(pipelineTriggers[triggerName]['Tag']['required'])
                              .sort()
                              .map((key) =>
                                pipelineTriggers[triggerName]['Tag']['required'][key].map((value) => (
                                  <FieldBadge key={key} field={`${key}: ${value}`} color={'#7e7c7c'} />
                                )),
                              )}
                          </Col>
                        </Row>
                        <Row>
                          <Col className="trigger-indent" />
                          <Col className="trigger-field">
                            <em>Not:</em>
                          </Col>
                          <Col className="trigger-value">
                            {Object.keys(pipelineTriggers[triggerName]['Tag']['not']).length == 0 && (
                              <FieldBadge field={'None'} color={'#7e7c7c'} />
                            )}
                            {Object.keys(pipelineTriggers[triggerName]['Tag']['not'])
                              .sort()
                              .map((key) =>
                                pipelineTriggers[triggerName]['Tag']['not'][key].map((value) => (
                                  <FieldBadge key={key} field={`${key}: ${value}`} color={'#7e7c7c'} />
                                )),
                              )}
                          </Col>
                        </Row>
                      </>
                    )}
                  {pipelineTriggers[triggerName] == 'NewSample' && (
                    <Row>
                      <Col className="trigger-indent" />
                      <Col className="trigger-field">
                        <em>Trigger Type:</em>
                      </Col>
                      <Col className="trigger-value">
                        <FieldBadge field={'NewSample'} color={'#7e7c7c'} />
                      </Col>
                    </Row>
                  )}
                  {Object.keys(pipelineTriggers).length - 1 != idx && <hr className="tagshr" />}
                </div>
              ))}
          </>
        )}
        {userCanDelete && (
          <Row className="mt-2">
            {updateError != '' && <AlertBanner>{updateError}</AlertBanner>}
            <Col>
              {userCanModify && (
                <ButtonToolbar className="d-flex justify-content-center">
                  <ButtonGroup>
                    <OverlayTipBottom
                      tip={
                        inEditMode
                          ? `Cancel editing this pipeline.`
                          : `Edit this pipeline. Only Thorium admins or
                            developers with group permissions may edit pipelines.`
                      }
                    >
                      <Button className="secondary-btn me-1" onClick={inEditMode ? exitEditMode : enterEditMode}>
                        {inEditMode ? 'Cancel' : 'Edit'}
                      </Button>
                    </OverlayTipBottom>
                    {inEditMode ? (
                      <OverlayTipBottom
                        tip={`Update this pipeline. Only Thorium admins or
                          developers with group permissions may update pipelines.`}
                      >
                        <Button
                          className="ok-btn"
                          disabled={!parseValid}
                          onClick={() => handlePipelineUpdate(editorObj, pipeline, setUpdateError)}
                        >
                          Update
                        </Button>
                      </OverlayTipBottom>
                    ) : (
                      <DeletePipelineButton pipeline={pipeline} />
                    )}
                  </ButtonGroup>
                </ButtonToolbar>
              )}
            </Col>
          </Row>
        )}
      </Form>
    );
  };

  const CreatePipeline = () => {
    const [showCreateModal, setShowCreateModal] = useState(false);
    const [pipelineObj, setPipelineObj] = useState(PIPELINE_CREATE_TEMPLATE);
    const [format, setFormat] = useState('yaml');
    const [parseValid, setParseValid] = useState(false);
    const [createError, setCreateError] = useState('');

    const handleCloseCreateModal = () => {
      setShowCreateModal(false);
      setCreateError('');
      setPipelineObj(PIPELINE_CREATE_TEMPLATE);
      setParseValid(false);
    };

    const handleEditorChange = (obj) => {
      if (obj) {
        setPipelineObj(obj);
        setParseValid(true);
      } else {
        setParseValid(false);
      }
    };

    async function handlePipelineCreate() {
      const data = editorObjectToPipelineCreate(pipelineObj);
      if (!data) {
        setCreateError('Pipeline group, name, and order are required');
        return;
      }
      if (await createPipeline(data, setCreateError)) {
        handleCloseCreateModal();
        fetchPipelines();
      }
    }

    const canCreatePipeline = ['Developer', 'Analyst', 'Admin'].includes(getThoriumRole(userInfo.role));
    const CreatePipelineTipMessage = canCreatePipeline
      ? `Create a new pipeline. You must be a
      Thorium developer, analyst, or admin to create a pipeline.`
      : `You must be a Thorium developer or
      admin to create a pipeline.`;

    return (
      <Fragment>
        <OverlayTipLeft tip={CreatePipelineTipMessage}>
          <Button className="ok-btn" onClick={() => setShowCreateModal(true)} disabled={!canCreatePipeline}>
            +
          </Button>
        </OverlayTipLeft>
        <Modal show={showCreateModal} onHide={handleCloseCreateModal} backdrop="static" keyboard={false} size="lg">
          <Modal.Header closeButton>
            <Modal.Title>Create New Pipeline</Modal.Title>
          </Modal.Header>
          <Modal.Body>
            <div className="mb-3">
              <FormatToggle format={format} onFormatChange={setFormat} />
            </div>
            <ImagePipelineEditor
              key={format}
              value={pipelineObj}
              onChange={handleEditorChange}
              checker={pipelineChecker}
              format={format}
              height="350px"
            />
            {createError != '' && <AlertBanner className="mt-4">{createError}</AlertBanner>}
          </Modal.Body>
          <Modal.Footer className="d-flex justify-content-center">
            <Button className="ok-btn" disabled={!parseValid} onClick={() => handlePipelineCreate()}>
              Create
            </Button>
          </Modal.Footer>
        </Modal>
      </Fragment>
    );
  };

  return (
    <Page title="Pipelines · Thorium">
      <PipelineHeader />
      <LoadingSpinner loading={loading}></LoadingSpinner>
      <Accordion alwaysOpen>
        {pipelines
          .sort((a, b) => orderComparePipeline(a, b))
          .map((pipeline) => (
            <Accordion.Item key={`${pipeline.name}_${pipeline.group}`} eventKey={`${pipeline.name}_${pipeline.group}`}>
              <Accordion.Header>
                <Col className="accordion-item-name">
                  <div className="text">{pipeline.name}</div>
                </Col>
                <Col className="accordion-item-relation" />
                <Col className="accordion-item-ownership">
                  <OverlayTipLeft tip={`This pipeline is owned by the ${pipeline.group} group.`}>
                    <small>
                      <i>{pipeline.group}</i>
                    </small>
                  </OverlayTipLeft>
                </Col>
              </Accordion.Header>
              <Accordion.Body>
                <PipelineInfo pipeline={pipeline} />
              </Accordion.Body>
            </Accordion.Item>
          ))}
      </Accordion>
    </Page>
  );
};

export default Pipelines;
