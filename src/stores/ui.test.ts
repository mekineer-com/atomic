import { beforeEach, describe, expect, it } from 'vitest';
import { useUIStore } from './ui';

beforeEach(() => {
  useUIStore.setState({ tabs: [], activeTabId: null, nextTabOrdinal: 1 });
});

describe('Approvals tabs', () => {
  it('opens a fresh singleton after the previous tab is retired', () => {
    useUIStore.getState().openToolTab('approvals');
    const oldId = useUIStore.getState().activeTabId!;
    useUIStore.getState().retireTab(oldId);
    useUIStore.getState().openToolTab('approvals');

    const state = useUIStore.getState();
    expect(state.tabs).toHaveLength(2);
    expect(state.tabs.find(tab => tab.id === oldId)?.retired).toBe(true);
    expect(state.activeTabId).not.toBe(oldId);

    state.openToolTab('approvals');
    expect(useUIStore.getState().tabs).toHaveLength(2);
  });

  it('never navigates within or deduplicates to a retired tab', () => {
    useUIStore.getState().openToolTab('approvals');
    const oldId = useUIStore.getState().activeTabId!;
    useUIStore.getState().retireTab(oldId);

    useUIStore.getState().openReader('memory:current');
    const state = useUIStore.getState();
    expect(state.tabs.find(tab => tab.id === oldId)?.stack).toHaveLength(1);
    expect(state.activeTabId).not.toBe(oldId);
  });

  it('does not close a retired snapshot when the current atom is removed', () => {
    useUIStore.getState().openReader('memory:old');
    const oldId = useUIStore.getState().activeTabId!;
    useUIStore.getState().retireTab(oldId);

    useUIStore.getState().removeAtomFromTabs('memory:old');

    expect(useUIStore.getState().tabs.find(tab => tab.id === oldId)?.retired).toBe(true);
  });
});
