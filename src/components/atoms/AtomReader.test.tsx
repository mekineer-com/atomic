import { act } from 'react';
import { createRoot } from 'react-dom/client';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { AtomWithTags } from '../../stores/atoms';

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const transport = vi.hoisted(() => ({
  invoke: vi.fn(),
  subscribe: vi.fn(() => () => undefined),
}));

vi.mock('../../lib/transport', () => ({ getTransport: () => transport }));
vi.mock('../../lib/openalma-identity', () => ({
  currentIdentity: () => ({ userId: 'Fictional User', soulId: 'Fictional Soul' }),
}));
vi.mock('../../hooks', () => ({
  useInlineEditor: () => ({
    editContent: '', editSourceUrl: '', editTags: [], saveStatus: 'idle', editorRevision: 0,
    startEditing: vi.fn(), setEditContent: vi.fn(), setEditSourceUrl: vi.fn(), setEditTags: vi.fn(),
    saveNow: vi.fn(), flushDraft: vi.fn(),
  }),
}));
vi.mock('../canvas/MiniGraphPreview', () => ({ MiniGraphPreview: () => null }));
vi.mock('../memu/EntityManager', () => ({ MemoryEntityControls: () => null }));
vi.mock('../../lib/api', () => ({ findSimilarAtoms: vi.fn().mockResolvedValue([]) }));
vi.mock('@atomic-editor/editor', () => ({ AtomicCodeMirrorEditor: () => null, wikiLinks: () => ({}) }));
vi.mock('@atomic-editor/editor/code-languages', () => ({ ATOMIC_CODE_LANGUAGES: [] }));

import { useAtomsStore } from '../../stores/atoms';
import { useUIStore } from '../../stores/ui';
import { AtomReader } from './AtomReader';

const memory = (content: string, updatedAt: string): AtomWithTags => ({
  id: 'memory:m1',
  content,
  title: content,
  snippet: content,
  source_url: null,
  source: null,
  published_at: null,
  created_at: '2026-01-01T00:00:00Z',
  updated_at: updatedAt,
  approved_at: null,
  embedding_status: 'complete',
  tagging_status: 'complete',
  kind: 'captured',
  tags: [],
  dossier_usages: [],
});

function ReaderHarness() {
  const retired = useUIStore(state => Boolean(state.tabs[0]?.retired));
  return <AtomReader atomId="memory:m1" viewKey="tab-1:memory:m1" tabId="tab-1" active retired={retired} />;
}

afterEach(() => {
  transport.invoke.mockReset();
  useAtomsStore.setState({ atoms: [] });
  useUIStore.setState({ tabs: [], activeTabId: null, nextTabOrdinal: 1 });
});

describe('AtomReader stale snapshots', () => {
  it('keeps a dirty draft mounted and read-only when a refetch changes its baseline', async () => {
    transport.invoke
      .mockResolvedValueOnce(memory('original memory', '2026-01-01T00:00:00Z'))
      .mockResolvedValueOnce(memory('external memory', '2026-01-02T00:00:00Z'))
      .mockRejectedValueOnce('memory not found');
    useUIStore.setState({
      tabs: [{
        id: 'tab-1',
        stack: [{ type: 'atom', atomId: 'memory:m1', tagId: null, highlightText: null, editing: false }],
        stackIndex: 0,
        ordinal: 1,
      }],
      activeTabId: 'tab-1',
      nextTabOrdinal: 2,
    });
    const container = document.createElement('div');
    const root = createRoot(container);

    await act(async () => {
      root.render(<ReaderHarness />);
      await Promise.resolve();
    });
    const textarea = container.querySelector('textarea')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')?.set?.call(textarea, 'local draft');
      textarea.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await act(async () => {
      useAtomsStore.setState({ atoms: [memory('external memory', '2026-01-02T00:00:00Z')] });
      await Promise.resolve();
    });

    expect(useUIStore.getState().tabs[0].retired).toBe(true);
    expect(container.querySelector('textarea')?.value).toBe('local draft');
    expect(container.querySelector('textarea')?.readOnly).toBe(true);
    expect(container.textContent).toContain('read-only snapshot');
    await act(async () => {
      [...container.querySelectorAll('button')]
        .find(button => button.textContent === 'Open current version')
        ?.click();
      await Promise.resolve();
    });
    expect(useUIStore.getState().tabs).toHaveLength(1);
    expect(container.textContent).toContain('no current version is available');
    expect([...container.querySelectorAll('button')].some(button => button.textContent === 'Open current version')).toBe(false);
    await act(async () => { root.unmount(); });
  });

  it('discards a slow refetch that started before a successful save', async () => {
    let finishRefresh!: (value: AtomWithTags) => void;
    transport.invoke.mockImplementation((command: string) => {
      if (command === 'get_atom_by_id' && transport.invoke.mock.calls.length === 1) {
        return Promise.resolve(memory('original memory', '2026-01-01T00:00:00Z'));
      }
      if (command === 'get_atom_by_id') {
        return new Promise(resolve => { finishRefresh = resolve; });
      }
      if (command === 'update_memory_summary') {
        return Promise.resolve(memory('saved memory', '2026-01-03T00:00:00Z'));
      }
      throw new Error(`unexpected command: ${command}`);
    });
    useUIStore.setState({
      tabs: [{
        id: 'tab-1',
        stack: [{ type: 'atom', atomId: 'memory:m1', tagId: null, highlightText: null, editing: false }],
        stackIndex: 0,
        ordinal: 1,
      }],
      activeTabId: 'tab-1',
      nextTabOrdinal: 2,
    });
    const container = document.createElement('div');
    const root = createRoot(container);

    await act(async () => {
      root.render(<ReaderHarness />);
      await Promise.resolve();
    });
    await act(async () => {
      useAtomsStore.setState({ atoms: [memory('store signal', '2026-01-02T00:00:00Z')] });
      await Promise.resolve();
    });
    const textarea = container.querySelector('textarea')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')?.set?.call(textarea, 'saved memory');
      textarea.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await act(async () => {
      [...container.querySelectorAll('button')].find(button => button.textContent === 'Save + approve')?.click();
      await Promise.resolve();
    });
    await act(async () => {
      finishRefresh(memory('original memory', '2026-01-01T00:00:00Z'));
      await Promise.resolve();
    });

    expect(useUIStore.getState().tabs[0].retired).not.toBe(true);
    expect(container.querySelector('textarea')?.value).toBe('saved memory');
    await act(async () => { root.unmount(); });
  });

  it('fetches a memory again after its reader is closed and reopened', async () => {
    transport.invoke
      .mockResolvedValueOnce(memory('old memory', '2026-01-01T00:00:00Z'))
      .mockResolvedValueOnce(memory('current memory', '2026-01-02T00:00:00Z'));
    const firstContainer = document.createElement('div');
    const firstRoot = createRoot(firstContainer);
    await act(async () => {
      firstRoot.render(<AtomReader atomId="memory:m1" viewKey="first" />);
      await Promise.resolve();
    });
    await act(async () => { firstRoot.unmount(); });

    const secondContainer = document.createElement('div');
    const secondRoot = createRoot(secondContainer);
    await act(async () => {
      secondRoot.render(<AtomReader atomId="memory:m1" viewKey="second" />);
      await Promise.resolve();
    });

    expect(transport.invoke).toHaveBeenCalledTimes(2);
    expect(secondContainer.querySelector('textarea')?.value).toBe('current memory');
    await act(async () => { secondRoot.unmount(); });
  });
});
