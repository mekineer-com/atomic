import { describe, expect, it } from 'vitest';
import type { TagWithCount } from '../../stores/tags';
import { groupMemuTags } from './TagTree';

const tag = (id: string, category_kind: 'lore' | 'topic' | 'goal'): TagWithCount => ({
  id: `category:${id}`,
  name: id,
  parent_id: null,
  created_at: '2026-01-01T00:00:00Z',
  is_autotag_target: false,
  autotag_description: '',
  atom_count: 1,
  children_total: 0,
  children: [],
  category_kind,
  active: true,
});

describe('groupMemuTags', () => {
  it('groups real category IDs without replacing them', () => {
    const groups = groupMemuTags([tag('self', 'lore'), tag('coffee', 'topic'), tag('travel', 'goal')]);
    expect(groups.map((group) => group.id)).toEqual(['memu:group:lore', 'memu:group:topic', 'memu:group:goal']);
    expect(groups.flatMap((group) => group.children.map((child) => child.id)))
      .toEqual(['category:self', 'category:coffee', 'category:travel']);
  });
});
