import { JSX, useEffect, useState } from 'react';
import { Row } from 'react-bootstrap';

// project imports
import { EntityDetailsConfig } from './configs';
import { DetailsMetadataProps } from '../EntityDetails';
import EntityTypeIcon from '@entities/shared/EntityTypeIcon';
import InfoHeader from '@entities/shared/InfoHeader';
import InfoValue from '@entities/shared/InfoValue';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import NumberInput from '@components/shared/inputs/NumberInput';
import CodeEditor from '@components/shared/inputs/code/CodeEditor/CodeEditor';
import CodeRenderer from '@components/shared/renderers/CodeRenderer';
import { stringToRenderableInput } from '@components/shared/renderers/detect';
import { getEntity } from '@thorpi/entities';
import { disassemblyToText, formatAddress, textToDisassembly } from '@utilities/disassembly';
import { FormatType } from '@utilities/rules/types';
import { Entities } from '@models/entities';
import { BlankCompiledFunction, CompiledFunction, CompiledFunctionMetaFields } from '@models/entities/functions';

// spec: ../EntityDetails.spec.md

const CompiledFunctionMetaInfo = ({
  entity,
  pendingEntity,
  handleUpdate,
  editing,
}: DetailsMetadataProps<Entities.CompiledFunction>): JSX.Element => {
  // The disassembly is edited as text (address-per-line) but stored as structured instructions, so we
  // keep the editor buffer locally and only commit it to metadata when it parses cleanly.
  const [disassemblyText, setDisassemblyText] = useState(() => disassemblyToText(pendingEntity.metadata.CompiledFunction.disassembly));
  const [parseError, setParseError] = useState<string | undefined>();

  // The entity is fetched asynchronously (and replaced on save), so the component first mounts with a
  // blank entity. Re-seed the editor buffer from the canonical entity whenever it changes; `entity`
  // only updates on load/save (not while typing), so this never clobbers in-progress edits.
  useEffect(() => {
    setDisassemblyText(disassemblyToText(entity.metadata.CompiledFunction.disassembly));
    setParseError(undefined);
  }, [entity]);

  // apply a single metadata field change and hand the updated metadata back to the entity update
  function updatePendingMeta<T extends keyof CompiledFunctionMetaFields>(field: T, value: CompiledFunctionMetaFields[T]): void {
    const updates: CompiledFunctionMetaFields = structuredClone(pendingEntity.metadata.CompiledFunction);
    updates[field] = value;
    handleUpdate('metadata', { CompiledFunction: updates });
  }

  function handleDisassemblyChange(text: string): void {
    setDisassemblyText(text);
    const { instructions, error } = textToDisassembly(text);
    setParseError(error);
    // only commit valid disassembly; malformed input leaves the last good value in place
    if (!error) updatePendingMeta('disassembly', instructions);
  }

  return (
    <>
      <Row className="mt-3">
        <InfoHeader>Address</InfoHeader>
        <InfoValue>
          {editing ? (
            <NumberInput value={pendingEntity.metadata.CompiledFunction.address} onChange={(v) => updatePendingMeta('address', v ?? 0)} min={0} />
          ) : (
            formatAddress(entity.metadata.CompiledFunction.address)
          )}
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Disassembly</InfoHeader>
        <InfoValue>
          {editing ? (
            <>
              <CodeEditor value={disassemblyText} onChange={handleDisassemblyChange} format={FormatType.Disassembly} height="400px" />
              {parseError && (
                <AlertBanner className="mt-2" severity={Severity.Error}>
                  {parseError}
                </AlertBanner>
              )}
            </>
          ) : (
            // view-only reuses the shared read-only renderer with the assembly highlighter
            <CodeRenderer
              input={stringToRenderableInput(disassemblyToText(entity.metadata.CompiledFunction.disassembly))}
              format={FormatType.Disassembly}
              height="auto"
            />
          )}
        </InfoValue>
      </Row>
    </>
  );
};

const getCompiledFunctionDetails = (
  entityID: string,
  setError: (err: string) => void,
  updateEntity: (entity: CompiledFunction) => void,
) => {
  void getEntity(entityID, setError).then((data) => {
    if (data && data.kind == Entities.CompiledFunction) {
      updateEntity(data);
    }
  });
};

const CompiledFunctionDetailsConfig: EntityDetailsConfig<Entities.CompiledFunction> = {
  getEntityDetails: getCompiledFunctionDetails,
  EntityMetaInfo: CompiledFunctionMetaInfo,
  BlankEntity: BlankCompiledFunction,
  icon: (size: number) => <EntityTypeIcon kind={Entities.CompiledFunction} size={size} />,
};

export default CompiledFunctionDetailsConfig;
