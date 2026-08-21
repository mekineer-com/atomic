import { describe, expect, it } from 'vitest';

import { COMMAND_MAP } from './command-map';

describe('memU command forwarding', () => {
  it('forwards entity curation search filters', () => {
    expect(COMMAND_MAP.search_atoms_hybrid.transformArgs?.({
      query: 'M42',
      limit: 8,
      memoryOnly: true,
      excludeEntityId: 'entity-1',
      excludeCategoryId: 'category:category-1',
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

  it('forwards free-text ordinary entity types without description', () => {
    expect(COMMAND_MAP.update_memu_entity.transformArgs?.({
      name: 'Library',
      entityType: 'WhatsApp integration library',
      aliases: [],
    })).toEqual({
      name: 'Library',
      entity_type: 'WhatsApp integration library',
      aliases: [],
    });
  });

  it('routes Relationship removal through DELETE', () => {
    const command = COMMAND_MAP.remove_memu_relationship;
    expect(command.method).toBe('DELETE');
    expect(typeof command.path === 'function' ? command.path({ id: 'entity-1' }) : command.path)
      .toBe('/api/memu/entities/entity-1/relationship');
  });
});
