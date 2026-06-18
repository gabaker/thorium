// spec: ./SPEC.md
import { useCallback, useEffect, useState } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';
import { Button, Col, Row } from 'react-bootstrap';

// project imports
import { Fields, OrderField, PipelineFormMode, Triggers } from '@components/pages/pipelines';
import type { PipelineFieldsValue } from '@components/pages/pipelines';
import { pipelineChecker } from '@components/pages/pipelines/checker';
import { PipelineCreateWrapper, SectionRow, TitleCol, FieldCol } from '@components/pages/pipelines/shared.styled';
import Page from '@components/pages/Page';
import AlertBanner from '@components/shared/alerts/AlertBanner';
import { FieldError } from '@components/shared/inputs/FieldError';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import FormatToggle from '@components/shared/inputs/code/CodeEditor/FormatToggle';
import ImagePipelineEditor from '@components/shared/inputs/code/CodeEditor/ImagePipelineEditor';
import ViewModeToggle, { ViewMode } from '@components/shared/inputs/code/CodeEditor/ViewModeToggle';
import { OverlayTipRight } from '@components/shared/overlay/tips';
import { listImages } from '@thorpi/images';
import { createPipeline } from '@thorpi/pipelines';
import { useAuth } from '@utilities/auth';
import { fetchGroups } from '@utilities/fetch';
import { FormatType } from '@utilities/rules/types';
import { useDebouncedValue } from '@utilities/useDebouncedValue';
import { editorObjectToPipelineCreate, pipelineToEditorObject } from '@utilities/transforms/pipeline';
import type { EventTrigger, Pipeline, PipelineCreate as PipelineCreateType } from '@models/pipelines';
import type { Group } from '@models/groups';

/// The blank editor object used to seed a new pipeline
export const PIPELINE_CREATE_TEMPLATE: Record<string, unknown> = {
  group: '',
  name: '',
  order: [],
  sla: 604800,
  description: '',
};

/// Top-level editor keys managed by the Fields form
const FIELDS_KEYS = new Set(['name', 'group', 'description', 'sla']);

const PipelineCreate: React.FC = () => {
  const navigate = useNavigate();
  const { state } = useLocation() as { state: Pipeline | null };
  const { checkCookie } = useAuth();
  const [groups, setGroups] = useState<string[]>([]);
  const [loading] = useState(false);
  const [displayErrors, setDisplayErrors] = useState(false);
  const [createError, setCreateError] = useState('');
  // Single source of truth: both form and editor views read/write this object
  const [editorObj, setEditorObj] = useState<Record<string, unknown>>(state ? pipelineToEditorObject(state) : PIPELINE_CREATE_TEMPLATE);
  const [viewMode, setViewMode] = useState<ViewMode>(ViewMode.Form);
  const [editorFormat, setEditorFormat] = useState<FormatType>(FormatType.YAML);
  const [editorParseValid, setEditorParseValid] = useState(false);
  const [fieldErrors, setFieldErrors] = useState(true);
  const [triggerErrors, setTriggerErrors] = useState(false);

  const formMode = state ? PipelineFormMode.Copy : PipelineFormMode.Create;
  const group = typeof editorObj.group === 'string' ? editorObj.group : '';
  const order = Array.isArray(editorObj.order) ? (editorObj.order as (string | string[])[]) : [];
  const triggers = (editorObj.triggers ?? {}) as Record<string, EventTrigger>;
  // an order with at least one image is required to create a pipeline
  const orderError = order.length === 0;

  // Lossless view switching — no confirmation needed since both views share editorObj
  const handleViewModeChange = (mode: ViewMode) => {
    if (mode !== viewMode) setViewMode(mode);
  };

  const handleEditorChange = (obj: Record<string, unknown> | null) => {
    if (obj) {
      setEditorObj(obj);
      setEditorParseValid(true);
    } else {
      setEditorParseValid(false);
    }
  };

  // Fields manages multiple top-level keys — remove old keys before merging new ones
  const handleFieldsChange = useCallback((fields: PipelineFieldsValue) => {
    setEditorObj((prev) => {
      const rest: Record<string, unknown> = {};
      for (const [k, v] of Object.entries(prev)) {
        if (!FIELDS_KEYS.has(k)) rest[k] = v;
      }
      return { ...rest, ...(fields as unknown as Record<string, unknown>) };
    });
  }, []);

  useEffect(() => {
    void fetchGroups(setGroups as (groups: { [name: string]: Group } | Group[] | string[]) => void, () => void checkCookie(), false);
  }, []);

  // Clear the bottom error banner once the form's field/trigger/order validation passes, so a
  // resolved set of "missing field" errors doesn't leave a stale alert behind.
  useEffect(() => {
    if (viewMode === ViewMode.Form && !fieldErrors && !triggerErrors && !orderError) {
      setCreateError('');
    }
  }, [viewMode, fieldErrors, triggerErrors, orderError]);

  // Debounce so we don't refetch the image list on every keystroke while the group is typed.
  const debouncedGroup = useDebouncedValue(group, 400);
  useEffect(() => {
    if (!debouncedGroup) return;
    let cancelled = false;
    void listImages(debouncedGroup, () => {}, false, null, 1000).then((result) => {
      if (cancelled) return;
      if (result && 'names' in result) pipelineChecker.setValidImageNames(debouncedGroup, result.names);
    });
    return () => {
      cancelled = true;
    };
  }, [debouncedGroup]);

  const handlePipelineCreate = async () => {
    if (viewMode === ViewMode.Form && (fieldErrors || triggerErrors || orderError)) {
      setCreateError('Please resolve missing fields or invalid entries');
      setDisplayErrors(true);
      return;
    }
    const data = editorObjectToPipelineCreate(editorObj);
    if (!data) {
      setCreateError('Pipeline group, name, and order are required');
      setDisplayErrors(true);
      return;
    }
    if (await createPipeline(data as PipelineCreateType, setCreateError)) {
      void navigate('/pipelines');
    } else {
      setDisplayErrors(true);
    }
  };

  return (
    <Page title="Create Pipeline">
      <PipelineCreateWrapper>
        <Row>
          <center>
            <h3>Create A Pipeline</h3>
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
              value={editorObj}
              onChange={handleEditorChange}
              checker={pipelineChecker}
              format={editorFormat}
              height="600px"
            />
          </>
        ) : (
          <>
            <SectionRow className="mt-2">
              <TitleCol>
                <h5>Pipeline</h5>
              </TitleCol>
              <FieldCol>
                <Fields
                  value={editorObj as unknown as PipelineFieldsValue}
                  groups={groups}
                  onChange={handleFieldsChange}
                  onValidate={setFieldErrors}
                  showErrors={displayErrors}
                  mode={formMode}
                />
              </FieldCol>
            </SectionRow>
            <hr />
            <SectionRow>
              <TitleCol>
                <OverlayTipRight
                  tip={`The order of images to run. Sequential steps run one after another.
                  Parallel steps (stacked vertically) run simultaneously.`}
                >
                  <h5>Order</h5>
                </OverlayTipRight>
              </TitleCol>
              <FieldCol>
                <OrderField order={order} onChange={(o) => setEditorObj((prev) => ({ ...prev, order: o }))} group={group} />
                {displayErrors && orderError && <FieldError>At least one image is required.</FieldError>}
              </FieldCol>
            </SectionRow>
            <hr />
            <SectionRow>
              <TitleCol>
                <OverlayTipRight tip="Automatic triggers that cause this pipeline to run on new samples or matching tags.">
                  <h5>Triggers</h5>
                </OverlayTipRight>
              </TitleCol>
              <FieldCol>
                <Triggers
                  value={triggers}
                  onChange={(t) => setEditorObj((prev) => ({ ...prev, triggers: t }))}
                  onValidate={setTriggerErrors}
                  showErrors={displayErrors}
                  mode={formMode}
                />
              </FieldCol>
            </SectionRow>
          </>
        )}
        <Row className="d-flex justify-content-center">
          <Col>{createError && <AlertBanner className="m-2">{createError}</AlertBanner>}</Col>
        </Row>
        <Row>
          <LoadingSpinner loading={loading}></LoadingSpinner>
        </Row>
        <Row className="mt-3">
          <Col className="d-flex justify-content-center">
            <Button className="secondary-btn" onClick={() => void navigate(-1)}>
              Cancel
            </Button>
            <Button
              className="ok-btn"
              disabled={viewMode === ViewMode.Editor && !editorParseValid}
              onClick={() => void handlePipelineCreate()}
            >
              Create
            </Button>
          </Col>
        </Row>
      </PipelineCreateWrapper>
    </Page>
  );
};

export default PipelineCreate;
