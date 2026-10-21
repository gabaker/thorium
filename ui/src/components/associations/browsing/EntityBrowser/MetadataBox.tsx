// project imports
import { MetadataContent, MetadataSection } from './EntityBrowser.styled';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import EntitySummary, { SummaryVariant } from '@components/shared/info/EntitySummary';
import { InfoModel, SummaryPart, treeNodeToInfo } from '@components/shared/info/info';
import { getEntity } from '@thorpi/entities';
import { TreeNodeKey } from '@models/trees';
import { useQuery } from '@tanstack/react-query';

interface MetadataBoxProps {
  /** The info model built from the graph/tree node — rendered immediately (and as the fetch fallback). */
  model: InfoModel;
  /**
   * When set, this row is an **entity** node with the given id. The graph/tree node omits an entity's heavy
   * content (a SigmaRule's `rule` YAML, a CompiledFunction's `disassembly`, a DecompiledFunction's decompiled
   * `content`), so on first expand the full entity is fetched and rendered in its place. Omitted for
   * File/Repo/Tag nodes, which already carry everything.
   */
  entityId?: string;
  /** Whether the details body is expanded. Controlled by the parent ({@link EntityRow}) so it can suppress the
   * header's hover preview while the details are open. */
  expanded: boolean;
}

/**
 * Condensed metadata affordance under a row's header: a single "details" up/down caret (no preview peek),
 * collapsed by default with minimal vertical footprint. Expanding reveals the node's full metadata via the
 * shared {@link EntitySummary} (kind/title omitted — the header already shows the name; the duplicate marker
 * lives on the header, so it's suppressed here too). Its expanded state is owned by the parent (controlled), so
 * the row can hide its hover summary preview while these details are open.
 *
 * For **entity** nodes ({@link MetadataBoxProps.entityId} set), the graph/tree node carries only lightweight
 * metadata, so the body would be missing the entity's rich content. On the first expand we lazily
 * `getEntity(id)` (once, cached for the row's lifetime) and rebuild the model from that authoritative record,
 * so sigma-rule YAML / disassembly / decompiled source and every other field render in the body. The fetch
 * falls back to the graph-node `model` on failure; a spinner shows while it's in flight.
 */
const MetadataBox: React.FC<MetadataBoxProps> = ({ model, entityId, expanded }) => {
  const { data: entity, isFetching } = useQuery({
    queryKey: ['entity', entityId],
    queryFn: () => getEntity(entityId!, () => {}),
    enabled: expanded && !!entityId,
    staleTime: 30 * 60 * 1000,
  });

  const fullModel = entity ? treeNodeToInfo({ [TreeNodeKey.Entity]: entity }) : null;

  return (
    <MetadataSection>
      {expanded && (
        <MetadataContent>
          <EntitySummary model={fullModel ?? model} variant={SummaryVariant.Compact} exclude={[SummaryPart.Kind, SummaryPart.Title]} />
          {isFetching && <LoadingSpinner loading={true} />}
        </MetadataContent>
      )}
    </MetadataSection>
  );
};

export default MetadataBox;
