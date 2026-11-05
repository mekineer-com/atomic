import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const transport = vi.hoisted(() => ({ invoke: vi.fn(), subscribe: vi.fn(() => () => undefined) }));
vi.mock('../../lib/transport', () => ({ getTransport: () => transport }));
vi.mock('../../hooks', () => ({
  useIsMobile: () => false,
  useInlineEditor: () => ({
    editContent: '', editSourceUrl: '', editTags: [], saveStatus: 'idle', editorRevision: 0,
    startEditing: vi.fn(), setEditContent: vi.fn(), setEditSourceUrl: vi.fn(), setEditTags: vi.fn(),
    saveNow: vi.fn(), flushDraft: vi.fn(),
  }),
}));
vi.mock('../atoms/AtomGrid', () => ({ AtomGrid: () => null }));
vi.mock('../atoms/AtomList', () => ({ AtomList: () => null }));
vi.mock('../canvas/MiniGraphPreview', () => ({ MiniGraphPreview: () => null }));
vi.mock('../../lib/api', () => ({ findSimilarAtoms: vi.fn().mockResolvedValue([]) }));
vi.mock('@atomic-editor/editor', () => ({ AtomicCodeMirrorEditor: () => null, wikiLinks: () => ({}) }));
vi.mock('@atomic-editor/editor/code-languages', () => ({ ATOMIC_CODE_LANGUAGES: [] }));
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
import { useAtomsStore, type AtomWithTags } from '../../stores/atoms';
import { useUIStore } from '../../stores/ui';
import { MainView } from './MainView';

let root: Root;
let container: HTMLDivElement;

function memory(id: string): AtomWithTags {
  return {
    id, content: 'Original fictional summary', title: 'Fictional title', snippet: '',
    description: 'Fictional description', summaries_revision: 1,
    source_url: null, source: null, published_at: null,
    created_at: '2026-01-01T00:00:00Z', updated_at: '2026-01-01T00:00:00Z', approved_at: null,
    embedding_status: 'complete', tagging_status: 'complete', kind: 'captured', tags: [], dossier_usages: [],
  };
}

beforeEach(() => {
  sessionStorage.clear();
  localStorage.clear();
  selectSoul('Fictional Owner', 'Fictional Soul');
  useUIStore.setState(useUIStore.getInitialState(), true);
  useAtomsStore.setState({ atoms: [], initialLoadComplete: true, semanticSearchResults: null });
  transport.invoke.mockImplementation((command: string, args?: { id: string }) => {
    if (command === 'get_memu_review_status') return Promise.resolve({ enabled: true });
    if (command === 'get_atom_by_id') return Promise.resolve(memory(args!.id));
    if (command === 'list_memu_entities') return Promise.resolve({ entities: [] });
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
  it.each([
    ['resolve', 'before'], ['resolve', 'after'], ['reject', 'before'], ['reject', 'after'],
  ])('ignores an obsolete initial read that will %s %s the replacement read after Back', async (outcome, order) => {
    let finishOld!: (value: AtomWithTags) => void;
    let failOld!: (error: Error) => void;
    let finishCurrent!: (value: AtomWithTags) => void;
    let reads = 0;
    const invoke = transport.invoke.getMockImplementation()!;
    transport.invoke.mockImplementation((command: string, args?: { id: string }) => {
      if (command !== 'get_atom_by_id') return invoke(command, args);
      return new Promise((resolve, reject) => {
        if (++reads === 1) { finishOld = resolve; failOld = reject; }
        else finishCurrent = resolve;
      });
    });
    const errors = vi.spyOn(console, 'error').mockImplementation(() => undefined);
    try {
      await act(async () => {
        useUIStore.getState().openReader('memory:fictional-memory', undefined, { newTab: true });
        root.render(<MainView />);
      });
      await act(async () => { useUIStore.getState().openReader('entity:next'); });
      await act(async () => { useUIStore.getState().tabBack(); });
      expect(reads).toBe(2);
      const latest = { ...memory('memory:fictional-memory'), content: 'Current fictional summary' };
      if (order === 'after') await act(async () => { finishCurrent(latest); });
      await act(async () => {
        if (outcome === 'resolve') finishOld({ ...latest, content: 'Obsolete fictional summary' });
        else failOld(new Error('Obsolete fictional failure'));
      });
      expect(container.textContent).not.toContain('Obsolete fictional summary');
      expect(container.textContent).not.toContain('Atom not found');
      expect(errors).not.toHaveBeenCalled();
      if (order === 'before') await act(async () => { finishCurrent(latest); });
      expect(container.querySelector('textarea')?.value).toBe('Current fictional summary');
    } finally {
      errors.mockRestore();
    }
  });

  it('loads a retained history entry first selected after restoring a tab', async () => {
    useUIStore.setState({
      tabs: [{
        id: 'restored-tab', ordinal: 1, stackIndex: 1,
        stack: ['memory:fictional-memory', 'entity:next'].map(atomId => ({ type: 'atom', atomId, tagId: null, highlightText: null, editing: false })),
      }],
      activeTabId: 'restored-tab',
    });
    await act(async () => { root.render(<MainView />); });
    expect(transport.invoke.mock.calls.some(([command]) => command === 'get_atom_by_id')).toBe(false);
    await act(async () => { useUIStore.getState().tabBack(); });
    expect(container.querySelector('textarea')?.value).toBe('Original fictional summary');
  });

  it('retains back and forward drafts, deduplicates repeated items and branches without truncating', async () => {
    const tabId = await openEntity();
    const first = activeReader();
    await edit(first, ['First unsaved draft']);
    first.scrollTop = 123;
    await act(async () => { useUIStore.getState().openReader('entity:second'); });
    const second = activeReader();
    await edit(second, ['Second unsaved draft']);
    expect(first.isConnected).toBe(true);
    expect(first.parentElement!.classList.contains('hidden')).toBe(true);

    await act(async () => { useUIStore.getState().tabBack(); });
    expect(activeReader()).toBe(first);
    expect(draftValues(first)[0]).toBe('First unsaved draft');
    expect(first.scrollTop).toBe(123);
    expect(second.isConnected).toBe(true);
    await act(async () => { useUIStore.getState().tabForward(); });
    expect(activeReader()).toBe(second);
    expect(draftValues(second)[0]).toBe('Second unsaved draft');

    await act(async () => { useUIStore.getState().openReader('entity:fictional-entity'); });
    expect(activeReader()).toBe(first);
    expect(container.querySelectorAll('article')).toHaveLength(2);
    await act(async () => { useUIStore.getState().tabBack(); useUIStore.getState().tabBack(); });
    await act(async () => { useUIStore.getState().openReader('entity:replacement'); });
    const replacementTab = useUIStore.getState().activeTabId!;
    expect(replacementTab).not.toBe(tabId);
    expect(second.isConnected).toBe(true);
    expect(draftValues(second)[0]).toBe('Second unsaved draft');
    expect(first.isConnected).toBe(true);
    await act(async () => { useUIStore.getState().closeTab(tabId); });
    expect(first.isConnected).toBe(false);
    expect(second.isConnected).toBe(false);
    expect(container.querySelectorAll('article')).toHaveLength(1);
    await act(async () => { useUIStore.getState().closeTab(replacementTab); });
    expect(container.querySelector('article')).toBeNull();
  });

  it('returns to the same draft when deleting a reader opened from it', async () => {
    const tabId = await openEntity();
    const first = activeReader();
    await edit(first, ['First unsaved draft']);
    await act(async () => { useUIStore.getState().openReader('memory:fictional-memory'); });
    await act(async () => { useUIStore.getState().removeAtomFromTabs('memory:fictional-memory'); });
    expect(useUIStore.getState().activeTabId).toBe(tabId);
    expect(activeReader()).toBe(first);
    expect(draftValues(first)[0]).toBe('First unsaved draft');
    expect(useUIStore.getState().tabs[0].stack).toHaveLength(1);
  });

  it.each(['memory:fictional-memory', 'category:fictional-dossier'])(
    'preserves the actual %s editor through same-tab history and revalidates only its current item', async (id) => {
      await act(async () => {
        useUIStore.getState().openReaderEditing(id, { newTab: true });
        root.render(<MainView />);
      });
      const textarea = [...container.querySelectorAll('textarea')].find(input => input.value === 'Original fictional summary')!;
      await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(textarea, 'Unsaved fictional prose');
        textarea.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await act(async () => { useUIStore.getState().openReader(id); });
      expect([...container.querySelectorAll('textarea')]).toContain(textarea);
      expect(textarea.value).toBe('Unsaved fictional prose');
      await act(async () => { useUIStore.getState().openReader('entity:next'); });
      expect(textarea.isConnected).toBe(true);
      expect(textarea.value).toBe('Unsaved fictional prose');
      const save = new KeyboardEvent('keydown', { key: 's', ctrlKey: true, cancelable: true });
      await act(async () => { document.dispatchEvent(save); });
      expect(save.defaultPrevented).toBe(false);
      expect(transport.invoke.mock.calls.some(([command]) => command === 'update_memory_summary' || command === 'update_category_summary')).toBe(false);
      const reads = transport.invoke.mock.calls.filter(([command]) => command === 'get_atom_by_id').length;
      transport.invoke.mockImplementation((command: string, args?: { id: string }) => {
        if (command === 'get_atom_by_id') return Promise.resolve({ ...memory(args!.id), content: 'Changed fictional prose' });
        throw new Error(`Unexpected command: ${command}`);
      });
      await act(async () => { useAtomsStore.setState({ atoms: [{ ...memory(id), updated_at: '2026-01-02T00:00:00Z' }] }); });
      expect(transport.invoke.mock.calls.filter(([command]) => command === 'get_atom_by_id')).toHaveLength(reads);
      expect(useUIStore.getState().tabs[0].retired).not.toBe(true);
      await act(async () => { useUIStore.getState().tabBack(); });
      expect([...container.querySelectorAll('textarea')]).toContain(textarea);
      expect(textarea.value).toBe('Unsaved fictional prose');
      expect(textarea.readOnly).toBe(true);
      expect(useUIStore.getState().tabs[0].retired).toBe(true);
    },
  );

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
