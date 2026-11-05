import { beforeEach, describe, expect, it } from 'vitest';
import { useUIStore } from './ui';

beforeEach(() => {
  useUIStore.setState({ tabs: [], activeTabId: null, nextTabOrdinal: 1 });
});

describe('reader history preservation', () => {
  it.each([0, 1])('keeps the other reader entry when deleting history position %s', index => {
    useUIStore.getState().openReader('category:draft');
    useUIStore.getState().openReader('memory:duplicate');
    const original = useUIStore.getState().tabs[0];
    useUIStore.setState({ tabs: [{ ...original, stackIndex: index }] });

    useUIStore.getState().removeAtomFromTabs(index === 0 ? 'category:draft' : 'memory:duplicate');

    const state = useUIStore.getState();
    expect(state.tabs).toHaveLength(1);
    expect(state.activeTabId).toBe(original.id);
    expect(state.tabs[0].stack).toEqual([original.stack[1 - index]]);
    expect(state.tabs[0].stack[0]).toBe(original.stack[1 - index]);
    expect(state.tabs[0].stackIndex).toBe(0);
  });

  it('closes a deleted reader only when its tab has no other history', () => {
    useUIStore.getState().openReader('memory:duplicate');
    useUIStore.getState().removeAtomFromTabs('memory:duplicate');
    expect(useUIStore.getState().tabs).toEqual([]);
    expect(useUIStore.getState().activeTabId).toBeNull();
  });

  it('branches after Back instead of discarding a retained reader', () => {
    useUIStore.getState().openReader('memory:first');
    useUIStore.getState().openReader('category:draft');
    useUIStore.getState().tabBack();
    const original = useUIStore.getState().tabs[0];
    useUIStore.getState().openReader('memory:new');
    const state = useUIStore.getState();
    expect(state.tabs).toHaveLength(2);
    expect(state.tabs[0]).toBe(original);
    expect(state.activeTabId).toBe(state.tabs[1].id);
  });
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
