import { describe, expect, it } from 'vitest';
import type { Tab } from '../stores/ui';
import { reconcileTabsForOverlay } from './RouterBridge';

describe('retired tab routing', () => {
  it('leaves forward readers intact when a fresh URL needs another tab', () => {
    const tab: Tab = {
      id: 'reader-history', ordinal: 1, stackIndex: 0,
      stack: ['memory:first', 'category:draft'].map(atomId => ({
        type: 'atom', atomId, tagId: null, highlightText: null, editing: false,
      })),
    };
    expect(reconcileTabsForOverlay([tab], tab.id,
      { kind: 'reader', atomId: 'memory:new', tagId: null }, 'unknown')).toBeNull();
    expect(tab.stack).toHaveLength(2);
    expect(tab.stackIndex).toBe(0);
  });

  it('keeps an active retired tab on its existing route', () => {
    const tab: Tab = {
      id: 'old-approvals',
      stack: [{ type: 'tool', tool: 'approvals' }],
      stackIndex: 0,
      ordinal: 1,
      retired: true,
    };

    const result = reconcileTabsForOverlay(
      [tab],
      tab.id,
      { kind: 'tool', tool: 'approvals' },
      'unknown',
    );

    expect(result?.activeTabId).toBe(tab.id);
    expect(result?.tabs).toEqual([tab]);
  });
});
