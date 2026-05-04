import type { AssociationCreate, Entity } from './types';
import { AssociationKind, BlankAssociationCreate } from './types';
import { createAssociation } from '@thorpi/associations';

export function handleAssociationUpdate(
  associationKeys: string[],
  entity: Entity | undefined,
  groups: string[],
  updatePendingAssociations: (associations: AssociationCreate[]) => void,
): void {
  const newAssociationList: AssociationCreate[] = [];
  if (entity) {
    associationKeys.map((type) => {
      const newAssociation = structuredClone(BlankAssociationCreate);
      newAssociation.kind = type.replaceAll(' ', '') as unknown as AssociationKind;
      newAssociation.source = { Entity: { id: entity.id, name: entity.name } };
      newAssociation.groups = groups;
      newAssociationList.push(newAssociation);
    });
    updatePendingAssociations(newAssociationList);
  }
}

export async function createFileAssociations(
  sha256: string,
  groups: string[],
  associations: AssociationCreate[],
): Promise<void> {
  for (let i = 0; i < associations.length; i++) {
    associations[i].groups = groups;
    associations[i].targets = [
      {
        File: sha256,
      },
    ];
    await createAssociation(associations[i], console.log);
  }
}
