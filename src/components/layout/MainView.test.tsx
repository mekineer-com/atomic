import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const transport = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('../../lib/transport', () => ({ getTransport: () => transport }));
vi.mock('../../hooks', () => ({ useIsMobile: () => false }));
vi.mock('../atoms/AtomGrid', () => ({ AtomGrid: () => null }));
vi.mock('../atoms/AtomList', () => ({ AtomList: () => null }));
vi.mock('../atoms/AtomReader', () => ({ AtomReader: () => <div>Memory reader</div> }));
vi.mock('../atoms/FilterBar', () => ({ FilterBar: () => null }));
vi.mock('../atoms/FilterSheet', () => ({ FilterSheet: () => null }));
vi.mock('../canvas/SigmaCanvas', () => ({ SigmaCanvas: () => null }));
vi.mock('../canvas/LocalGraphView', () => ({ LocalGraphView: () => null }));
vi.mock('../dashboard/DashboardView', () => ({ DashboardView: () => null }));
vi.mock('../ui/FAB', () => ({ FAB: () => null }));
vi.mock('../wiki/WikiFullView', () => ({ WikiFullView: () => null }));
vi.mock('../wiki/WikiReader', () => ({ WikiReader: () => null }));
vi.mock('../reports', () => ({ ReportsFullView: () => null, ReportDetailView: () => null, FindingReader: () => null }));
vi.mock('../chat/ChatViewer', () => ({ ChatViewer: () => null }));
vi.mock('../memu/PendingReviewPanel', () => ({ PendingReviewPanel: () => null }));
vi.mock('./TabStrip', () => ({ TabStrip: () => null }));

import { currentIdentity, selectSoul } from '../../lib/openalma-identity';
import { useAtomsStore } from '../../stores/atoms';
import { useUIStore } from '../../stores/ui';
import { MainView } from './MainView';

let root: Root;
let container: HTMLDivElement;

beforeEach(() => {
  sessionStorage.clear();
  localStorage.clear();
  selectSoul('Fictional Owner', 'Fictional Soul');
  useUIStore.setState(useUIStore.getInitialState(), true);
  useAtomsStore.setState({ atoms: [], initialLoadComplete: true, semanticSearchResults: null });
  transport.invoke.mockImplementation((command: string, args?: { id: string }) => {
    if (command === 'get_memu_review_status') return Promise.resolve({ enabled: true });
    if (command === 'get_memu_entity') {
      const identity = currentIdentity()!;
      return Promise.resolve({
        id: args!.id, name: `${identity.userId}/${identity.soulId}/${args!.id}`,
        entity_type: 'person', properties: { aliases: ['Original alias'] },
        is_relationship: false, ignored: false, orphan: true,
        linked_memory_count: 0, last_mentioned_at: null, created_at: null, memories: [],
      });
    }
    throw new Error(`Unexpected command: ${command}`);
  });
  container = document.createElement('div');
  document.body.append(container);
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => { root.unmount(); });
  container.remove();
  transport.invoke.mockReset();
  useUIStore.setState(useUIStore.getInitialState(), true);
  sessionStorage.clear();
  localStorage.clear();
});

async function openEntity() {
  await act(async () => {
    useUIStore.getState().openReader('entity:fictional-entity', undefined, { newTab: true });
    root.render(<MainView />);
  });
  return useUIStore.getState().activeTabId!;
}

function activeReader() {
  return [...container.querySelectorAll('article')].find(article => !article.parentElement!.classList.contains('hidden'))!;
}

async function edit(reader: HTMLElement, drafts: string[]) {
  await act(async () => {
    [...reader.querySelectorAll('button')].find(button => button.textContent === 'Edit')!.click();
  });
  await act(async () => {
    drafts.forEach((draft, index) => {
      const input = reader.querySelectorAll('input')[index];
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, draft);
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
  });
}

function draftValues(reader: HTMLElement) {
  return [...reader.querySelectorAll('input')].slice(0, 3).map(input => input.value);
}

describe('MainView entity reader lifecycle', () => {
  it('keeps the actual entity draft and scroll mounted while another reader tab is active', async () => {
    const tabId = await openEntity();
    const reader = activeReader();
    const drafts = ['Unsaved name', 'Unsaved type', 'Unsaved alias'];
    await edit(reader, drafts);
    reader.scrollTop = 342;

    await act(async () => {
      useUIStore.getState().openReader('memory:fictional-memory', undefined, { newTab: true });
    });
    expect(reader.isConnected).toBe(true);
    expect(reader.parentElement!.classList.contains('hidden')).toBe(true);
    await act(async () => { useUIStore.getState().switchToTab(tabId); });

    expect(activeReader()).toBe(reader);
    expect(draftValues(reader)).toEqual(drafts);
    expect(reader.scrollTop).toBe(342);
    expect(transport.invoke.mock.calls.filter(([command]) => command === 'get_memu_entity')).toHaveLength(1);
  });

  it('isolates duplicate entity tabs and discards only the closed tab, including on reopen', async () => {
    const firstTab = await openEntity();
    const first = activeReader();
    await edit(first, ['First draft']);
    first.scrollTop = 101;
    const secondTab = await openEntity();
    const second = activeReader();
    expect(second).not.toBe(first);
    expect(second.querySelector('label input')).toBeNull();
    await edit(second, ['Second draft']);
    second.scrollTop = 202;

    await act(async () => { useUIStore.getState().switchToTab(firstTab); });
    expect(activeReader()).toBe(first);
    expect(draftValues(first)[0]).toBe('First draft');
    expect(first.scrollTop).toBe(101);
    await act(async () => { useUIStore.getState().switchToTab(secondTab); });
    expect(activeReader()).toBe(second);
    expect(draftValues(second)[0]).toBe('Second draft');
    expect(second.scrollTop).toBe(202);

    await act(async () => { useUIStore.getState().closeTab(firstTab); });
    expect(first.isConnected).toBe(false);
    expect(activeReader()).toBe(second);
    await act(async () => { useUIStore.getState().closeTab(secondTab); });
    expect(second.isConnected).toBe(false);
    expect(container.querySelector('article')).toBeNull();
    await openEntity();
    expect(activeReader()).not.toBe(second);
    expect(activeReader().querySelector('label input')).toBeNull();
    expect(activeReader().scrollTop).toBe(0);
    expect(transport.invoke.mock.calls.filter(([command]) => command === 'get_memu_entity')).toHaveLength(3);
  });

  it.each([
    ['Other Fictional Owner', 'Fictional Soul'],
    ['Fictional Owner', 'Other Fictional Soul'],
  ])('does not reuse entity drafts across owner/Soul changes to %s/%s', async (userId, soulId) => {
    await openEntity();
    const reader = activeReader();
    await edit(reader, ['Private unsaved draft']);
    reader.scrollTop = 303;

    await act(async () => {
      selectSoul(userId, soulId);
      root.render(<MainView />);
    });
    expect(reader.isConnected).toBe(false);
    expect(activeReader()).not.toBe(reader);
    expect(activeReader().querySelector('label input')).toBeNull();
    expect(activeReader().querySelector('h1')!.textContent).toBe(`${userId}/${soulId}/fictional-entity`);
    expect(activeReader().scrollTop).toBe(0);
  });
});
