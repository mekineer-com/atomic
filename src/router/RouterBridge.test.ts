import { describe, expect, it } from 'vitest';
import type { Tab } from '../stores/ui';
import { reconcileTabsForOverlay } from './RouterBridge';

describe('retired tab routing', () => {
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
