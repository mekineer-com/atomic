import { describe, expect, it } from 'vitest';

import { COMMAND_MAP } from './command-map';

describe('memU command forwarding', () => {
  it('forwards category description edits from approvals', () => {
    expect(COMMAND_MAP.update_category_summary.transformArgs?.({
      summary: 'Current category prose',
      title: 'Current category',
      description: 'Edited category description',
      kind: 'topic',
      displayed_summary: 'Previous category prose',
      displayed_title: 'Previous category',
      displayed_description: 'Previous description',
      summaries_revision: 4,
    })).toEqual({
      summary: 'Current category prose',
      title: 'Current category',
      description: 'Edited category description',
      kind: 'topic',
      displayed_summary: 'Previous category prose',
      displayed_title: 'Previous category',
      displayed_description: 'Previous description',
      summaries_revision: 4,
    });
  });

  it('forwards the loaded identity for category approval and membership', () => {
    const guard = {
      displayed_summary: 'Shown', summaries_revision: 4,
      displayed_title: 'Loaded title', displayed_description: 'Loaded description',
    };
    expect(COMMAND_MAP.approve_category.transformArgs?.(guard)).toEqual(guard);
    for (const command of ['attach_memu_category_memory', 'detach_memu_category_memory']) {
      expect(COMMAND_MAP[command].transformArgs?.({
        displayedSummary: guard.displayed_summary, summariesRevision: guard.summaries_revision,
        displayedTitle: guard.displayed_title, displayedDescription: guard.displayed_description,
      })).toEqual(guard);
    }
  });

  it('keeps categories in tag search rather than atom search', () => {
    expect(COMMAND_MAP.search_atoms_keyword.transformArgs?.({ query: 'test', limit: 5 })).toMatchObject({
      memory_only: true,
    });
    expect(COMMAND_MAP.search_atoms_semantic.transformArgs?.({ query: 'test', limit: 5 })).toMatchObject({
      memory_only: true,
    });
  });

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
});
