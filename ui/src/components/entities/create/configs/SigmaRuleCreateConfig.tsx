import { JSX, useEffect, useState } from 'react';
import { Row, Form, Card } from 'react-bootstrap';

// project imports
import { EntityCreateConfig } from './config';
import { CreateMetadataProps } from '../EntityCreate';
import InfoHeader from '@entities/shared/InfoHeader';
import InfoValue from '@entities/shared/InfoValue';
import { Entities } from '@models/entities/entities';
import {
  BlankCreateSigmaRule,
  SigmaActionToTake,
  SigmaReactionArgs,
  SigmaRuleAppliesTo,
  SigmaRuleMetaFields,
  SigmaSpawnReaction,
} from '@models/entities/rules/sigma';
import CodeEditor from '@components/shared/inputs/code/CodeEditor/CodeEditor';
import { SigmaRuleChecker } from '@utilities/rules/sigma';
import SelectInputArray from '@components/shared/inputs/selectable/SelectInputArray';
import styled from 'styled-components';
import NumberInput from '@components/shared/inputs/NumberInput';
import { FormatType } from '@utilities/rules/types';
import { Select } from '@components/pages/pipelines/shared.styled';
import { fetchGroups } from '@utilities/fetch';
import { listPipelines } from '@thorpi/pipelines';

const ActionCard = styled(Card)`
  margin-bottom: 0.75rem;
  background-color: var(--thorium-panel-bg);
  border: 1px solid var(--thorium-panel-border);
`;

const ActionCardBody = styled(Card.Body)`
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
`;

const ActionHeader = styled.div`
  display: flex;
  justify-content: space-between;
  align-items: center;
`;

const ActionLabel = styled.span`
  font-weight: 600;
  font-size: 0.85rem;
  color: var(--thorium-secondary-text);
  text-transform: uppercase;
  letter-spacing: 0.03em;
`;

const RemoveButton = styled.button`
  background: none;
  border: none;
  color: var(--thorium-secondary-text);
  font-size: 1.25rem;
  cursor: pointer;
  padding: 0 0.25rem;
  line-height: 1;

  &:hover {
    color: var(--bs-danger);
  }
`;

const AddActionButton = styled.button`
  color: var(--thorium-button-text);
  background-color: var(--thorium-empty-bg);
  border: 1px solid var(--thorium-empty-bg);
  border-radius: 4px;
  padding: 0.25rem 0.75rem;
  font-size: 0.875rem;
  cursor: pointer;

  &:hover {
    filter: brightness(80%);
  }

  &:active {
    filter: brightness(70%);
    box-shadow: 0 0 0 0.1rem var(--thorium-highlight-panel-border);
  }
`;

const AddActionRow = styled.div`
  display: flex;
  gap: 0.5rem;
`;

const ImageArgsCard = styled.div`
  background-color: var(--thorium-secondary-panel-bg);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 4px;
  padding: 0.75rem;
  margin-bottom: 0.5rem;
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
`;

const ImageArgsHeader = styled.div`
  display: flex;
  align-items: center;
  gap: 0.5rem;
`;

const KwargRow = styled.div`
  display: flex;
  align-items: flex-start;
  gap: 0.5rem;
  margin-bottom: 0.5rem;
`;

const KwargName = styled.div`
  flex: 0 0 30%;
`;

const KwargValues = styled.div`
  flex: 1;
  min-width: 0;
`;

interface RenamableKeyInputProps {
  name: string;
  placeholder: string;
  onRename: (name: string) => boolean;
}

const RenamableKeyInput: React.FC<RenamableKeyInputProps> = ({ name, placeholder, onRename }) => {
  // edits stay local until they are committed on blur/enter
  const [text, setText] = useState(name);

  // re-seed the local text whenever the key changes externally
  useEffect(() => {
    setText(name);
  }, [name]);

  // commit the rename and revert the local text if it was rejected
  const commit = () => {
    if (text !== name && !onRename(text)) {
      setText(name);
    }
  };

  return (
    <Form.Control
      type="text"
      placeholder={placeholder}
      value={text}
      onChange={(e) => setText(e.target.value)}
      onBlur={commit}
      onKeyDown={(e) => {
        if (e.key === 'Enter') {
          e.preventDefault();
          commit();
        }
      }}
    />
  );
};

interface ImageArgsEditorProps {
  name: string;
  args: SigmaReactionArgs;
  onRename: (name: string) => boolean;
  onChange: (args: SigmaReactionArgs) => void;
  onRemove: () => void;
}

const ImageArgsEditor: React.FC<ImageArgsEditorProps> = ({ name, args, onRename, onChange, onRemove }) => {
  // helper to update a single field of this image's args
  const updateField = <T extends keyof SigmaReactionArgs>(field: T, value: SigmaReactionArgs[T]) => {
    const next = structuredClone(args);
    next[field] = value;
    onChange(next);
  };

  const renameKwarg = (oldName: string, newName: string): boolean => {
    // reject empty names and renames that would collide with another kwarg entry
    // (own keys only, so names like 'constructor' don't match Object.prototype)
    if (newName === '' || (newName !== oldName && Object.hasOwn(args.kwargs, newName))) return false;
    // nothing to update if the name is unchanged
    if (newName === oldName) return true;
    const next = structuredClone(args);
    // rebuild the kwargs map so the renamed entry keeps its position
    const renamed: Record<string, string[]> = {};
    Object.entries(next.kwargs).forEach(([kwarg, values]) => {
      renamed[kwarg === oldName ? newName : kwarg] = values;
    });
    next.kwargs = renamed;
    onChange(next);
    return true;
  };

  const addKwarg = () => {
    const next = structuredClone(args);
    // key the new kwarg entry by the first unused placeholder name
    let count = Object.keys(next.kwargs).length + 1;
    while (`--kwarg${count}` in next.kwargs) {
      count++;
    }
    next.kwargs[`--kwarg${count}`] = [];
    onChange(next);
  };

  const updateKwargValues = (kwarg: string, values: string[]) => {
    const next = structuredClone(args);
    next.kwargs[kwarg] = values;
    onChange(next);
  };

  const removeKwarg = (kwarg: string) => {
    const next = structuredClone(args);
    delete next.kwargs[kwarg];
    onChange(next);
  };

  return (
    <ImageArgsCard>
      <ImageArgsHeader>
        <RenamableKeyInput name={name} placeholder="Image name" onRename={onRename} />
        <RemoveButton onClick={onRemove} title="Remove image">
          &times;
        </RemoveButton>
      </ImageArgsHeader>
      <Form.Group>
        <Form.Label>Positional Args</Form.Label>
        <SelectInputArray
          isCreatable
          values={args.positionals}
          onChange={(positionals) => updateField('positionals', positionals)}
          defaultMessage='jq filter, e.g. .pid or "literal"'
        />
      </Form.Group>
      <Form.Group>
        <Form.Label>Kwargs</Form.Label>
        {Object.entries(args.kwargs).map(([kwarg, values], i) => (
          <KwargRow key={i}>
            <KwargName>
              <RenamableKeyInput name={kwarg} placeholder="Kwarg name, e.g. --pid" onRename={(newName) => renameKwarg(kwarg, newName)} />
            </KwargName>
            <KwargValues>
              <SelectInputArray
                isCreatable
                values={values}
                error={values.length === 0}
                onChange={(updated) => updateKwargValues(kwarg, updated)}
                defaultMessage='jq filter, e.g. .pid or "literal"'
              />
            </KwargValues>
            <RemoveButton onClick={() => removeKwarg(kwarg)} title="Remove kwarg">
              &times;
            </RemoveButton>
          </KwargRow>
        ))}
        <AddActionButton onClick={addKwarg}>+ Add Kwarg</AddActionButton>
      </Form.Group>
      <Form.Group>
        <Form.Label>Switches</Form.Label>
        <SelectInputArray
          isCreatable
          values={args.switches}
          onChange={(switches) => updateField('switches', switches)}
          defaultMessage="Literal switch, e.g. --verbose"
        />
      </Form.Group>
    </ImageArgsCard>
  );
};

interface SpawnReactionEditorProps {
  reaction: SigmaSpawnReaction;
  groups: string[];
  onChange: (reaction: SigmaSpawnReaction) => void;
}

const SpawnReactionEditor: React.FC<SpawnReactionEditorProps> = ({ reaction, groups, onChange }) => {
  // the pipelines that can be spawned in the currently selected group
  const [pipelines, setPipelines] = useState<string[]>([]);

  // reload the selectable pipelines whenever the selected group changes
  useEffect(() => {
    // without a group there are no pipelines to select from
    if (!reaction.group) {
      setPipelines([]);
      return;
    }
    let cancelled = false;
    // list this group's pipeline names and push them into state
    void listPipelines(reaction.group, console.log, false, null, 1000).then((result) => {
      // skip stale results for a group that is no longer selected
      if (cancelled) return;
      setPipelines(result ? [...(result as string[])].sort() : []);
    });
    return () => {
      cancelled = true;
    };
  }, [reaction.group]);

  // helper to update a single top level field of this reaction
  const updateField = <T extends keyof SigmaSpawnReaction>(field: T, value: SigmaSpawnReaction[T]) => {
    const next = structuredClone(reaction);
    next[field] = value;
    onChange(next);
  };

  // select a new group and clear the group scoped pipeline in a single update
  const updateGroup = (group: string) => {
    const next = structuredClone(reaction);
    next.group = group;
    next.pipeline = '';
    onChange(next);
  };

  // keep the current group selectable even when it isn't in the fetched list
  const groupOptions = reaction.group && !groups.includes(reaction.group) ? [reaction.group, ...groups] : groups;
  // keep the current pipeline selectable even when it isn't in the fetched list
  const pipelineOptions = reaction.pipeline && !pipelines.includes(reaction.pipeline) ? [reaction.pipeline, ...pipelines] : pipelines;

  const renameImage = (oldName: string, newName: string): boolean => {
    // reject empty names and renames that would collide with another image entry
    // (own keys only, so names like 'constructor' don't match Object.prototype)
    if (newName === '' || (newName !== oldName && Object.hasOwn(reaction.args, newName))) return false;
    // nothing to update if the name is unchanged
    if (newName === oldName) return true;
    const next = structuredClone(reaction);
    // rebuild the args map so the renamed entry keeps its position
    const renamed: Record<string, SigmaReactionArgs> = {};
    Object.entries(next.args).forEach(([image, args]) => {
      renamed[image === oldName ? newName : image] = args;
    });
    next.args = renamed;
    onChange(next);
    return true;
  };

  const addImage = () => {
    const next = structuredClone(reaction);
    // key the new image entry by the first unused placeholder name
    let count = Object.keys(next.args).length + 1;
    while (`image${count}` in next.args) {
      count++;
    }
    next.args[`image${count}`] = { positionals: [], kwargs: {}, switches: [] };
    onChange(next);
  };

  const updateImageArgs = (image: string, args: SigmaReactionArgs) => {
    const next = structuredClone(reaction);
    next.args[image] = args;
    onChange(next);
  };

  const removeImage = (image: string) => {
    const next = structuredClone(reaction);
    delete next.args[image];
    onChange(next);
  };

  return (
    <>
      <Form.Group>
        <Form.Label>
          Group<sub>*</sub>
        </Form.Label>
        <Select value={reaction.group} onChange={(e) => updateGroup(e.target.value)}>
          <option value="">Select a group</option>
          {groupOptions.map((group) => (
            <option key={group} value={group}>
              {group}
            </option>
          ))}
        </Select>
      </Form.Group>
      <Form.Group>
        <Form.Label>
          Pipeline<sub>*</sub>
        </Form.Label>
        <Select value={reaction.pipeline} disabled={!reaction.group} onChange={(e) => updateField('pipeline', e.target.value)}>
          <option value="">{reaction.group ? 'Select a pipeline' : 'Select a group first'}</option>
          {pipelineOptions.map((pipeline) => (
            <option key={pipeline} value={pipeline}>
              {pipeline}
            </option>
          ))}
        </Select>
      </Form.Group>
      <Form.Group>
        <Form.Label>Image Args</Form.Label>
        {Object.entries(reaction.args).map(([image, args], i) => (
          <ImageArgsEditor
            key={i}
            name={image}
            args={args}
            onRename={(newName) => renameImage(image, newName)}
            onChange={(updated) => updateImageArgs(image, updated)}
            onRemove={() => removeImage(image)}
          />
        ))}
        <AddActionButton onClick={addImage}>+ Add Image</AddActionButton>
      </Form.Group>
    </>
  );
};

interface ActionsEditorProps {
  actions: SigmaActionToTake[];
  onChange: (actions: SigmaActionToTake[]) => void;
}

export const ActionsEditor: React.FC<ActionsEditorProps> = ({ actions, onChange }) => {
  // all of the groups this user can see for the spawn reaction group dropdowns
  const [groups, setGroups] = useState<string[]>([]);

  // load this user's groups once on mount
  useEffect(() => {
    void fetchGroups(
      (fetched) => setGroups(fetched as string[]),
      () => {},
      false,
      'Array',
    );
  }, []);

  const addFlagAction = () => {
    onChange([...actions, { Flag: { content: '', reasoning: '' } }]);
  };

  const addSpawnReactionAction = () => {
    onChange([...actions, { SpawnReaction: { group: '', pipeline: '', args: {} } }]);
  };

  const updateAction = (index: number, action: SigmaActionToTake) => {
    const next = structuredClone(actions);
    next[index] = action;
    onChange(next);
  };

  const removeAction = (index: number) => {
    onChange(actions.filter((_, i) => i !== index));
  };

  return (
    <>
      {actions.map((action, i) => (
        <ActionCard key={i}>
          <ActionCardBody>
            {'Flag' in action && (
              <>
                <ActionHeader>
                  <ActionLabel>Flag Action #{i + 1}</ActionLabel>
                  <RemoveButton onClick={() => removeAction(i)} title="Remove action">
                    &times;
                  </RemoveButton>
                </ActionHeader>
                <Form.Group>
                  <Form.Label>Content</Form.Label>
                  <Form.Control
                    type="text"
                    placeholder="Interesting characteristic (optional)"
                    value={action.Flag.content ?? ''}
                    onChange={(e) => updateAction(i, { Flag: { ...action.Flag, content: e.target.value || undefined } })}
                  />
                </Form.Group>
                <Form.Group>
                  <Form.Label>
                    Reasoning<sub>*</sub>
                  </Form.Label>
                  <Form.Control
                    as="textarea"
                    rows={2}
                    placeholder="Reason for this flag (required)"
                    value={action.Flag.reasoning}
                    onChange={(e) => updateAction(i, { Flag: { ...action.Flag, reasoning: e.target.value } })}
                  />
                </Form.Group>
              </>
            )}
            {'SpawnReaction' in action && (
              <>
                <ActionHeader>
                  <ActionLabel>Spawn Reaction Action #{i + 1}</ActionLabel>
                  <RemoveButton onClick={() => removeAction(i)} title="Remove action">
                    &times;
                  </RemoveButton>
                </ActionHeader>
                <SpawnReactionEditor
                  reaction={action.SpawnReaction}
                  groups={groups}
                  onChange={(reaction) => updateAction(i, { SpawnReaction: reaction })}
                />
              </>
            )}
          </ActionCardBody>
        </ActionCard>
      ))}
      <AddActionRow>
        <AddActionButton onClick={addFlagAction}>+ Add Flag Action</AddActionButton>
        <AddActionButton onClick={addSpawnReactionAction}>+ Add Spawn Reaction Action</AddActionButton>
      </AddActionRow>
    </>
  );
};

const SigmaRuleMetaInfo = ({ entity, onChange }: CreateMetadataProps<Entities.SigmaRule>): JSX.Element => {
  // helper to update nested metadata
  const updatePendingMeta = <T extends keyof SigmaRuleMetaFields>(field: T, value: SigmaRuleMetaFields[T]) => {
    const updates: SigmaRuleMetaFields = structuredClone(entity.metadata.SigmaRule);
    updates[field] = value;
    onChange('metadata', { SigmaRule: updates });
  };
  const sigmaChecker = new SigmaRuleChecker();
  return (
    <>
      <Row>
        <InfoHeader>Rule</InfoHeader>
        <InfoValue>
          <CodeEditor
            value={entity.metadata.SigmaRule.rule}
            onChange={(text) => updatePendingMeta('rule', text)}
            checker={sigmaChecker}
            format={FormatType.YAML}
            height="400px"
          />
        </InfoValue>
      </Row>
      <Row>
        <InfoHeader>Applies To</InfoHeader>
        <InfoValue className="mt-2">
          <SelectInputArray
            isCreatable={false}
            options={Object.values(SigmaRuleAppliesTo)}
            values={entity.metadata.SigmaRule.applies_to}
            onChange={(selected) => updatePendingMeta('applies_to', selected as SigmaRuleAppliesTo[])}
          />
        </InfoValue>
      </Row>
      <Row>
        <InfoHeader>Score</InfoHeader>
        <InfoValue className="mt-2">
          <NumberInput
            step={10}
            value={entity.metadata.SigmaRule.score}
            onChange={(score) => updatePendingMeta('score', score ?? 0)}
            min={0}
          />
        </InfoValue>
      </Row>
      <Row>
        <InfoHeader>Actions</InfoHeader>
        <InfoValue className="mt-2">
          <ActionsEditor actions={entity.metadata.SigmaRule.actions} onChange={(actions) => updatePendingMeta('actions', actions)} />
        </InfoValue>
      </Row>
    </>
  );
};

const SigmaRuleCreateConfig: EntityCreateConfig<Entities.SigmaRule> = {
  kind: Entities.SigmaRule,
  EntityMetadata: SigmaRuleMetaInfo,
  BlankCreateEntity: BlankCreateSigmaRule,
};

export default SigmaRuleCreateConfig;
