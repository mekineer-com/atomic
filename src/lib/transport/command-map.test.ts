import { describe, expect, it } from 'vitest';

import { COMMAND_MAP } from './command-map';

describe('memU command forwarding', () => {
  it('forwards entity curation search filters', () => {
    expect(COMMAND_MAP.search_atoms_hybrid.transformArgs?.({
      query: 'M42',
      limit: 8,
      memoryOnly: true,
      excludeEntityId: 'entity-1',
      excludeCategoryId: 'category-1',
    })).toEqual({
      query: 'M42',
      mode: 'hybrid',
      limit: 8,
      threshold: undefined,
      memory_only: true,
      exclude_entity_id: 'entity-1',
      exclude_category_id: 'category-1',
    });
  });

  it('forwards ordinary entity descriptions', () => {
    expect(COMMAND_MAP.update_memu_entity.transformArgs?.({
      name: 'Library',
      entityType: 'project',
      aliases: [],
      description: 'WhatsApp integration library',
    })).toEqual({
      name: 'Library',
      entity_type: 'project',
      aliases: [],
      description: 'WhatsApp integration library',
    });
  });
});
