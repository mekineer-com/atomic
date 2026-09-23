import { beforeEach, describe, expect, it } from 'vitest';
import { useUIStore } from './ui';

beforeEach(() => {
  useUIStore.setState({ tabs: [], activeTabId: null, nextTabOrdinal: 1 });
});

describe('Approvals tabs', () => {
  it('opens a fresh singleton after the previous tab is retired', () => {
    useUIStore.getState().openToolTab('approvals');
    const oldId = useUIStore.getState().activeTabId!;
    useUIStore.getState().retireApprovalsTab(oldId);
    useUIStore.getState().openToolTab('approvals');

    const state = useUIStore.getState();
    expect(state.tabs).toHaveLength(2);
    expect(state.tabs.find(tab => tab.id === oldId)?.retired).toBe(true);
    expect(state.activeTabId).not.toBe(oldId);

    state.openToolTab('approvals');
    expect(useUIStore.getState().tabs).toHaveLength(2);
  });
});
