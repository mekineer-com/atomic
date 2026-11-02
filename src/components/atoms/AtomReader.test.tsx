import { act } from 'react';
import { createRoot } from 'react-dom/client';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { AtomWithTags } from '../../stores/atoms';

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const transport = vi.hoisted(() => ({
  invoke: vi.fn(),
  listeners: new Map<string, (payload: unknown) => void>(),
  subscribe: vi.fn((event: string, callback: (payload: unknown) => void) => {
    transport.listeners.set(event, callback);
    return () => { transport.listeners.delete(event); };
  }),
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
vi.mock('../memu/EntityManager', () => ({
  MemoryEntityControls: ({ entityNames = [], onUpdated, readOnly }: {
    entityNames?: string[]; onUpdated: (atom: AtomWithTags) => void; readOnly?: boolean;
  }) => <>
    <div data-entity-links>{entityNames.join(', ')}</div>
    <button disabled={readOnly} onClick={async () => {
      onUpdated(await transport.invoke('detach_memu_entity', { memoryId: 'm1', entityId: 'old' }));
    }}>Detach entity</button>
  </>,
}));
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

function ReaderHarness({ current = true, atomId = 'memory:m1', initialEditing = false }: {
  current?: boolean; atomId?: string; initialEditing?: boolean;
}) {
  const retired = useUIStore(state => Boolean(state.tabs[0]?.retired));
  return <AtomReader atomId={atomId} viewKey={`tab-1:${atomId}`} tabId="tab-1" active={current} current={current} retired={retired} initialEditing={initialEditing} />;
}

afterEach(() => {
  transport.invoke.mockReset();
  transport.listeners.clear();
  useAtomsStore.setState({ atoms: [] });
  useUIStore.setState({ tabs: [], activeTabId: null, nextTabOrdinal: 1 });
});

describe('AtomReader stale snapshots', () => {
  it('settles loading without fetching when mounted as a retired snapshot', async () => {
    const container = document.createElement('div');
    const root = createRoot(container);
    try {
      await act(async () => { root.render(<AtomReader atomId="memory:m1" viewKey="retired" retired />); });
      expect(container.textContent).toContain('Atom not found');
      expect(transport.invoke).not.toHaveBeenCalled();
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it.each([false, true])('refetches links from stripped events without changing prose or retiring a draft (dirty=%s)', async dirty => {
    const original = { ...memory('Original prose', '2026-01-01T00:00:00Z'), entity_ids: ['entity:old'], entity_names: ['Old link'] };
    transport.invoke.mockResolvedValueOnce(original).mockResolvedValueOnce({
      ...original, entity_ids: ['entity:new'], entity_names: ['New link'],
    });
    useUIStore.setState({ tabs: [{ id: 'tab-1', stack: [], stackIndex: 0, ordinal: 1 }], activeTabId: 'tab-1' });
    const container = document.createElement('div');
    const root = createRoot(container);
    try {
      await act(async () => { root.render(<ReaderHarness />); });
      const textarea = container.querySelector('textarea')!;
      if (dirty) await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(textarea, 'Fictional draft');
        textarea.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await act(async () => { transport.listeners.get('atom-updated')?.(memory('Event without entity metadata', original.updated_at)); });
      expect(container.querySelector('[data-entity-links]')?.textContent).toBe('New link');
      expect(container.querySelector('textarea')).toBe(textarea);
      expect(textarea.value).toBe(dirty ? 'Fictional draft' : 'Original prose');
      expect(textarea.readOnly).toBe(false);
      expect(useUIStore.getState().tabs[0].retired).not.toBe(true);
      expect(transport.invoke).toHaveBeenCalledTimes(2);
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it.each(['old-first', 'new-first'])('keeps fresh link detail over an initial read (%s)', async order => {
    let old!: (value: AtomWithTags) => void;
    let fresh!: (value: AtomWithTags) => void;
    transport.invoke.mockImplementationOnce(() => new Promise(resolve => { old = resolve; }))
      .mockImplementationOnce(() => new Promise(resolve => { fresh = resolve; }));
    const container = document.createElement('div');
    const root = createRoot(container);
    try {
      await act(async () => { root.render(<ReaderHarness />); });
      await act(async () => { transport.listeners.get('atom-updated')?.({ id: 'memory:m1' }); });
      for (const read of order === 'old-first' ? ['old', 'new'] : ['new', 'old']) {
        await act(async () => {
          const value = memory('Original prose', '2026-01-01T00:00:00Z');
          if (read === 'old') old({ ...value, entity_names: ['Old link'], entity_ids: ['entity:old'] });
          else fresh({ ...value, entity_names: ['New link'], entity_ids: ['entity:new'] });
        });
      }
      expect(container.querySelector('[data-entity-links]')?.textContent).toBe('New link');
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it.each(['before', 'after'])('ignores an initial read error superseded by event detail (%s the newer read)', async order => {
    let failOld!: (error: Error) => void;
    let finishNew!: (value: AtomWithTags) => void;
    transport.invoke.mockImplementationOnce(() => new Promise((_resolve, reject) => { failOld = reject; }))
      .mockImplementationOnce(() => new Promise(resolve => { finishNew = resolve; }));
    const errors = vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const container = document.createElement('div');
    const root = createRoot(container);
    try {
      await act(async () => { root.render(<ReaderHarness />); });
      await act(async () => { transport.listeners.get('atom-updated')?.({ id: 'memory:m1' }); });
      const fail = async () => act(async () => { failOld(new Error('Obsolete initial error')); });
      if (order === 'before') {
        await fail();
        expect(container.textContent).not.toContain('Atom not found');
      }
      await act(async () => { finishNew({ ...memory('Current prose', '2026-01-01T00:00:00Z'), entity_names: ['New link'], entity_ids: ['entity:new'] }); });
      if (order === 'after') await fail();
      expect(container.querySelector('[data-entity-links]')?.textContent).toBe('New link');
      expect(errors).not.toHaveBeenCalled();
    } finally {
      await act(async () => { root.unmount(); });
      errors.mockRestore();
    }
  });

  it.each([false, true])('uses newer canonical event detail over a pending prose read (dirty=%s)', async dirty => {
    const original = memory('Original prose', '2026-01-01T00:00:00Z');
    let finishProse!: (value: AtomWithTags) => void;
    transport.invoke.mockResolvedValueOnce(original)
      .mockImplementationOnce(() => new Promise(resolve => { finishProse = resolve; }))
      .mockResolvedValueOnce({ ...memory('External prose', '2026-01-02T00:00:00Z'), entity_ids: ['entity:new'], entity_names: ['New link'] });
    useUIStore.setState({ tabs: [{ id: 'tab-1', stack: [], stackIndex: 0, ordinal: 1 }], activeTabId: 'tab-1' });
    const container = document.createElement('div');
    const root = createRoot(container);
    try {
      await act(async () => { root.render(<ReaderHarness />); });
      if (dirty) await act(async () => {
        const textarea = container.querySelector('textarea')!;
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(textarea, 'Fictional draft');
        textarea.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await act(async () => { useAtomsStore.setState({ atoms: [memory('External prose', '2026-01-02T00:00:00Z')] }); });
      await act(async () => { transport.listeners.get('atom-updated')?.({ id: 'memory:m1' }); });
      await act(async () => { finishProse(memory('External prose', '2026-01-02T00:00:00Z')); });
      expect(container.querySelector('textarea')?.value).toBe(dirty ? 'Fictional draft' : 'External prose');
      expect(Boolean(useUIStore.getState().tabs[0].retired)).toBe(dirty);
      expect(container.querySelector('[data-entity-links]')?.textContent).toBe(dirty ? '' : 'New link');
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it('discards obsolete link reads and does not refresh a retired snapshot', async () => {
    let old!: (value: AtomWithTags) => void;
    const original = memory('Original prose', '2026-01-01T00:00:00Z');
    transport.invoke.mockResolvedValueOnce(original)
      .mockImplementationOnce(() => new Promise(resolve => { old = resolve; }))
      .mockResolvedValueOnce({ ...original, entity_ids: ['entity:new'], entity_names: ['New link'] });
    useUIStore.setState({ tabs: [{ id: 'tab-1', stack: [], stackIndex: 0, ordinal: 1 }], activeTabId: 'tab-1' });
    const container = document.createElement('div');
    const root = createRoot(container);
    try {
      await act(async () => { root.render(<ReaderHarness />); });
      await act(async () => {
        transport.listeners.get('atom-updated')?.({ id: 'memory:m1' });
        transport.listeners.get('atom-updated')?.({ id: 'memory:m1' });
      });
      await act(async () => { old({ ...original, entity_ids: [], entity_names: [] }); });
      expect(container.querySelector('[data-entity-links]')?.textContent).toBe('New link');
      await act(async () => { useUIStore.getState().retireTab('tab-1'); });
      const reads = transport.invoke.mock.calls.length;
      await act(async () => { transport.listeners.get('atom-updated')?.({ id: 'memory:m1' }); });
      expect(transport.invoke).toHaveBeenCalledTimes(reads);
      expect(container.querySelector('[data-entity-links]')?.textContent).toBe('New link');
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it.each(['success', 'failure'])('refreshes notifications received during Save after its %s', async outcome => {
    const original = { ...memory('Original prose', '2026-01-01T00:00:00Z'), entity_ids: ['entity:old'], entity_names: ['Old link'] };
    const saved = { ...memory('Fictional draft', '2026-01-02T00:00:00Z'), entity_ids: ['entity:old'], entity_names: ['Old link'] };
    let finish!: (value: AtomWithTags) => void;
    let fail!: (error: Error) => void;
    let finishRefresh!: (value: AtomWithTags) => void;
    transport.invoke.mockResolvedValueOnce(original)
      .mockImplementationOnce(() => new Promise((resolve, reject) => { finish = resolve; fail = reject; }))
      .mockImplementationOnce(() => new Promise(resolve => { finishRefresh = resolve; }));
    useUIStore.setState({ tabs: [{ id: 'tab-1', stack: [], stackIndex: 0, ordinal: 1 }], activeTabId: 'tab-1' });
    const container = document.createElement('div');
    const root = createRoot(container);
    try {
      await act(async () => { root.render(<ReaderHarness />); });
      const textarea = container.querySelector('textarea')!;
      await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(textarea, 'Fictional draft');
        textarea.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await act(async () => { [...container.querySelectorAll('button')].find(button => button.textContent === 'Save + approve')!.click(); });
      await act(async () => { transport.listeners.get('atom-updated')?.({ id: 'memory:m1' }); });
      expect(transport.invoke).toHaveBeenCalledTimes(2);
      await act(async () => { if (outcome === 'success') finish(saved); else fail(new Error('Fictional save failure')); });
      expect(container.querySelector('[data-entity-links]')?.textContent).toBe('Old link');
      // Save has rendered before the pending read completes, so its baseline guard is exercised.
      expect(transport.invoke).toHaveBeenCalledTimes(3);
      await act(async () => { finishRefresh({ ...(outcome === 'success' ? saved : original), entity_ids: ['entity:new'], entity_names: ['New link'] }); });
      expect(container.querySelector('[data-entity-links]')?.textContent).toBe('New link');
      expect(container.querySelector('textarea')).toBe(textarea);
      expect(textarea.value).toBe('Fictional draft');
      expect(textarea.readOnly).toBe(false);
      expect(useUIStore.getState().tabs[0].retired).not.toBe(true);
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it.each(['during', 'after'])('keeps a notification read that completes %s a failing Save', async order => {
    const original = { ...memory('Original prose', '2026-01-01T00:00:00Z'), entity_ids: ['entity:old'], entity_names: ['Old link'] };
    const linked = { ...original, entity_ids: ['entity:new'], entity_names: ['New link'] };
    let finishRead!: (value: AtomWithTags) => void;
    let failSave!: (error: Error) => void;
    transport.invoke.mockResolvedValueOnce(original)
      .mockImplementationOnce(() => new Promise(resolve => { finishRead = resolve; }))
      .mockImplementationOnce(() => new Promise((_resolve, reject) => { failSave = reject; }))
      .mockResolvedValueOnce(linked);
    useUIStore.setState({ tabs: [{ id: 'tab-1', stack: [], stackIndex: 0, ordinal: 1 }], activeTabId: 'tab-1' });
    const container = document.createElement('div');
    const root = createRoot(container);
    try {
      await act(async () => { root.render(<ReaderHarness />); });
      const textarea = container.querySelector('textarea')!;
      await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(textarea, 'Fictional draft');
        textarea.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await act(async () => { transport.listeners.get('atom-updated')?.({ id: 'memory:m1' }); });
      await act(async () => { [...container.querySelectorAll('button')].find(button => button.textContent === 'Save + approve')!.click(); });
      if (order === 'during') {
        await act(async () => { finishRead(linked); });
        expect(container.querySelector('[data-entity-links]')?.textContent).toBe('Old link');
      }
      await act(async () => { failSave(new Error('Fictional save failure')); });
      if (order === 'after') await act(async () => { finishRead(linked); });
      expect(container.querySelector('[data-entity-links]')?.textContent).toBe('New link');
      expect(container.querySelector('textarea')).toBe(textarea);
      expect(textarea.value).toBe('Fictional draft');
      expect(textarea.readOnly).toBe(false);
      expect(useUIStore.getState().tabs[0].retired).not.toBe(true);
      expect(transport.invoke.mock.calls.filter(([command]) => command === 'get_atom_by_id')).toHaveLength(order === 'during' ? 3 : 2);
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it.each([false, true])('revalidates unchanged-timestamp links after returning from history (dirty=%s)', async dirty => {
    const original = { ...memory('Original prose', '2026-01-01T00:00:00Z'), entity_ids: ['entity:old'], entity_names: ['Old link'] };
    transport.invoke.mockResolvedValueOnce(original)
      .mockResolvedValueOnce({ ...original, entity_ids: ['entity:new'], entity_names: ['New link'] });
    useUIStore.setState({ tabs: [{ id: 'tab-1', stack: [], stackIndex: 0, ordinal: 1 }], activeTabId: 'tab-1' });
    const container = document.createElement('div');
    const root = createRoot(container);
    try {
      await act(async () => { root.render(<ReaderHarness />); });
      const textarea = container.querySelector('textarea')!;
      if (dirty) await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(textarea, 'Fictional draft');
        textarea.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await act(async () => { root.render(<ReaderHarness current={false} />); });
      await act(async () => { transport.listeners.get('atom-updated')?.({ id: 'memory:m1' }); });
      expect(transport.invoke).toHaveBeenCalledTimes(1);
      expect(container.querySelector('[data-entity-links]')?.textContent).toBe('Old link');
      await act(async () => { root.render(<ReaderHarness />); });
      expect(transport.invoke).toHaveBeenCalledTimes(2);
      expect(container.querySelector('[data-entity-links]')?.textContent).toBe('New link');
      expect(container.querySelector('textarea')).toBe(textarea);
      expect(textarea.value).toBe(dirty ? 'Fictional draft' : 'Original prose');
      expect(textarea.readOnly).toBe(false);
      expect(useUIStore.getState().tabs[0].retired).not.toBe(true);
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it('keeps Cancel closed with initialEditing true and does not collapse an edit returned to baseline', async () => {
    const category = { ...memory('Original dossier', '2026-01-01T00:00:00Z'), id: 'category:c1', summaries_revision: 1 };
    transport.invoke.mockResolvedValue(category);
    const container = document.createElement('div');
    const root = createRoot(container);
    try {
      await act(async () => { root.render(<AtomReader atomId={category.id} viewKey={category.id} initialEditing />); });
      const textarea = [...container.querySelectorAll('textarea')].find(input => input.value === category.content)!;
      for (const text of ['Fictional edit', category.content]) {
        await act(async () => {
          Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(textarea, text);
          textarea.dispatchEvent(new Event('input', { bubbles: true }));
        });
      }
      expect(container.contains(textarea)).toBe(true);
      await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(textarea, 'Discard this fictional edit');
        textarea.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await act(async () => {
        [...container.querySelectorAll('button')].find(button => button.textContent === 'Cancel')!.click();
      });
      expect(container.querySelector('textarea')).toBeNull();
      await act(async () => { root.render(<AtomReader atomId={category.id} viewKey={category.id} initialEditing />); });
      expect(container.querySelector('textarea')).toBeNull();
      expect([...container.querySelectorAll('button')].some(button => button.textContent === 'Edit')).toBe(true);
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it('updates clean memory text without retiring its tab', async () => {
    transport.invoke
      .mockResolvedValueOnce(memory('original memory', '2026-01-01T00:00:00Z'))
      .mockResolvedValueOnce(memory('external memory', '2026-01-02T00:00:00Z'));
    useUIStore.setState({ tabs: [{ id: 'tab-1', stack: [], stackIndex: 0, ordinal: 1 }], activeTabId: 'tab-1' });
    const container = document.createElement('div');
    const root = createRoot(container);
    try {
      await act(async () => { root.render(<ReaderHarness />); });
      await act(async () => { useAtomsStore.setState({ atoms: [memory('external memory', '2026-01-02T00:00:00Z')] }); });
      expect(container.querySelector('textarea')?.value).toBe('external memory');
      expect(useUIStore.getState().tabs[0].retired).not.toBe(true);
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it.each(['refresh', 'link'])('discards a pending %s response after its item is buried, then checks the draft on return', async source => {
    let finishRefresh!: (value: AtomWithTags) => void;
    let finishReturn!: (value: AtomWithTags) => void;
    transport.invoke
      .mockResolvedValueOnce(memory('original memory', '2026-01-01T00:00:00Z'))
      .mockImplementationOnce(() => new Promise(resolve => { finishRefresh = resolve; }))
      .mockImplementationOnce(() => new Promise(resolve => { finishReturn = resolve; }));
    useUIStore.setState({
      tabs: [{ id: 'tab-1', stack: [{ type: 'atom', atomId: 'memory:m1', tagId: null, highlightText: null, editing: false }], stackIndex: 0, ordinal: 1 }],
      activeTabId: 'tab-1',
    });
    const container = document.createElement('div');
    const root = createRoot(container);
    try {
      await act(async () => { root.render(<ReaderHarness />); });
      const textarea = container.querySelector('textarea')!;
      await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(textarea, 'Fictional draft');
        textarea.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await act(async () => {
        if (source === 'refresh') useAtomsStore.setState({ atoms: [memory('external memory', '2026-01-02T00:00:00Z')] });
        else [...container.querySelectorAll('button')].find(button => button.textContent === 'Detach entity')!.click();
      });
      await act(async () => {
        useUIStore.getState().openReader('entity:old');
        root.render(<ReaderHarness current={false} />);
      });
      await act(async () => { finishRefresh(memory('external memory', '2026-01-02T00:00:00Z')); });
      expect(useUIStore.getState().tabs[0].retired).not.toBe(true);
      expect(useUIStore.getState().tabs[0].stack[1]).toMatchObject({ atomId: 'entity:old' });
      expect(textarea.value).toBe('Fictional draft');
      expect(textarea.readOnly).toBe(false);
      await act(async () => {
        useUIStore.getState().tabBack();
        root.render(<ReaderHarness />);
      });
      expect(container.querySelector('textarea')).toBe(textarea);
      expect(textarea.value).toBe('Fictional draft');
      await act(async () => { finishReturn(memory('external memory', '2026-01-02T00:00:00Z')); });
      expect(useUIStore.getState().tabs[0].retired).toBe(true);
      expect(container.querySelector('textarea')).toBe(textarea);
      expect(textarea.value).toBe('Fictional draft');
      expect(textarea.readOnly).toBe(true);
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it.each([false, true].flatMap(dirty => [
    'memory not found', 'HTTP 404: memory not found', null, 'Fictional network unavailable',
  ].map(error => ({ dirty, error }))))('revalidates retained history without losing a draft ($dirty, $error)', async ({ dirty, error }) => {
    transport.invoke.mockResolvedValueOnce(memory('original memory', '2026-01-01T00:00:00Z'));
    if (error === null) transport.invoke.mockResolvedValueOnce(null);
    else transport.invoke.mockRejectedValueOnce(error);
    useUIStore.setState({ tabs: [{ id: 'tab-1', stack: [], stackIndex: 0, ordinal: 1 }], activeTabId: 'tab-1' });
    const errors = vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const container = document.createElement('div');
    const root = createRoot(container);
    try {
      await act(async () => { root.render(<ReaderHarness />); });
      const textarea = container.querySelector('textarea')!;
      if (dirty) await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(textarea, 'Unsaved fictional draft');
        textarea.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await act(async () => { root.render(<ReaderHarness current={false} />); });
      await act(async () => { root.render(<ReaderHarness />); });
      const missing = error !== 'Fictional network unavailable';
      expect(useUIStore.getState().tabs[0].retired === true).toBe(dirty && missing);
      if (missing && !dirty) {
        expect(container.textContent).toContain('Atom not found');
        expect(container.querySelector('textarea')).toBeNull();
      } else {
        expect(container.querySelector('textarea')).toBe(textarea);
        expect(textarea.value).toBe(dirty ? 'Unsaved fictional draft' : 'original memory');
        expect(textarea.readOnly).toBe(missing);
        if (missing) expect(container.textContent).toContain('no current version is available');
      }
      expect(errors).toHaveBeenCalledTimes(missing ? 0 : 1);
    } finally {
      await act(async () => { root.unmount(); });
      errors.mockRestore();
    }
  });

  it.each(['refresh', 'link'])('does not retire a replacement scoped reader when an unmounted draft %s finishes', async source => {
    let finishOld!: (value: AtomWithTags) => void;
    transport.invoke
      .mockResolvedValueOnce(memory('original memory', '2026-01-01T00:00:00Z'))
      .mockImplementationOnce(() => new Promise(resolve => { finishOld = resolve; }))
      .mockResolvedValue(memory('Other fictional scope', '2026-01-03T00:00:00Z'));
    useUIStore.setState({ tabs: [{ id: 'tab-1', stack: [], stackIndex: 0, ordinal: 1 }], activeTabId: 'tab-1' });
    const container = document.createElement('div');
    const root = createRoot(container);
    try {
      await act(async () => { root.render(<ReaderHarness key="old-scope" />); });
      const textarea = container.querySelector('textarea')!;
      await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(textarea, 'Old fictional draft');
        textarea.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await act(async () => {
        if (source === 'refresh') useAtomsStore.setState({ atoms: [memory('old scope changed', '2026-01-02T00:00:00Z')] });
        else [...container.querySelectorAll('button')].find(button => button.textContent === 'Detach entity')!.click();
      });
      await act(async () => { root.render(<ReaderHarness key="replacement-scope" />); });
      await act(async () => { finishOld(memory('old scope changed', '2026-01-02T00:00:00Z')); });
      expect(useUIStore.getState().tabs[0].retired).not.toBe(true);
      expect(container.querySelector('textarea')?.value).toBe('Other fictional scope');
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it('sends loaded category identity and does not retry over an unseen description', async () => {
    const category = {
      ...memory('Shown', '2026-01-01T00:00:00Z'), id: 'category:c1',
      title: 'Loaded title', description: 'Loaded description', summaries_revision: 4,
    };
    useUIStore.setState({
      tabs: [{ id: 'category-tab', stack: [{ type: 'atom', atomId: category.id, tagId: null, highlightText: null, editing: false }], stackIndex: 0, ordinal: 1 }],
      activeTabId: 'category-tab', nextTabOrdinal: 2,
    });
    transport.invoke.mockImplementation((command: string) => {
      if (command === 'get_atom_by_id') {
        return Promise.resolve(transport.invoke.mock.calls.length === 1 ? category : {
          ...category, description: 'Unseen description',
        });
      }
      if (command === 'approve_category') return Promise.reject('summary_snapshot_stale');
      throw new Error(`unexpected command: ${command}`);
    });
    const container = document.createElement('div');
    const root = createRoot(container);
    await act(async () => {
      root.render(<AtomReader atomId="category:c1" viewKey="category:c1" tabId="category-tab" active />);
      await Promise.resolve();
    });
    await act(async () => {
      [...container.querySelectorAll('button')].find(button => button.textContent === 'Approve')!.click();
    });
    const mutations = transport.invoke.mock.calls.filter(([command]) => command === 'approve_category');
    expect(mutations).toHaveLength(1);
    expect(mutations[0][1]).toMatchObject({
      displayed_summary: 'Shown', displayed_title: 'Loaded title',
      displayed_description: 'Loaded description', summaries_revision: 4,
    });
    expect(useUIStore.getState().tabs[0].retired).toBe(true);
    await act(async () => { root.unmount(); });
  });

  it.each(['memory not found', 'HTTP 404: memory not found', null])('keeps a dirty snapshot and handles missing current state: %s', async (missing) => {
    transport.invoke
      .mockResolvedValueOnce(memory('original memory', '2026-01-01T00:00:00Z'))
      .mockResolvedValueOnce(memory('external memory', '2026-01-02T00:00:00Z'));
    if (missing === null) transport.invoke.mockResolvedValueOnce(null);
    else transport.invoke.mockRejectedValueOnce(missing);
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

  it('shows a current-version network failure without losing the snapshot or retry', async () => {
    transport.invoke
      .mockResolvedValueOnce(memory('original memory', '2026-01-01T00:00:00Z'))
      .mockResolvedValueOnce(memory('external memory', '2026-01-02T00:00:00Z'))
      .mockRejectedValueOnce(new Error('Fictional network unavailable'))
      .mockResolvedValueOnce(memory('external memory', '2026-01-02T00:00:00Z'));
    useUIStore.setState({
      tabs: [{ id: 'tab-1', stack: [{ type: 'atom', atomId: 'memory:m1', tagId: null, highlightText: null, editing: false }], stackIndex: 0, ordinal: 1 }],
      activeTabId: 'tab-1', nextTabOrdinal: 2,
    });
    const container = document.createElement('div');
    const root = createRoot(container);
    try {
      await act(async () => { root.render(<ReaderHarness />); });
      const textarea = container.querySelector('textarea')!;
      await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(textarea, 'Unsaved fictional draft');
        textarea.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await act(async () => { useAtomsStore.setState({ atoms: [memory('external memory', '2026-01-02T00:00:00Z')] }); });
      const openCurrent = () => [...container.querySelectorAll('button')].find(button => button.textContent === 'Open current version')!;
      await act(async () => { openCurrent().click(); });
      expect(container.textContent).toContain('Fictional network unavailable');
      expect(container.textContent).not.toContain('no current version is available');
      expect(container.querySelector('textarea')).toBe(textarea);
      expect(textarea.value).toBe('Unsaved fictional draft');
      expect(textarea.readOnly).toBe(true);
      expect(useUIStore.getState().tabs).toHaveLength(1);
      await act(async () => { openCurrent().click(); });
      expect(container.textContent).not.toContain('Fictional network unavailable');
      expect(useUIStore.getState().tabs).toHaveLength(2);
      expect(useUIStore.getState().tabs[0].retired).toBe(true);
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it.each([false, true])('discards a slow refetch that started before a successful save (later link event=%s)', async linkEvent => {
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
    if (linkEvent) {
      transport.invoke.mockResolvedValueOnce({ ...memory('saved memory', '2026-01-03T00:00:00Z'), entity_ids: ['entity:new'], entity_names: ['New link'] });
      await act(async () => { transport.listeners.get('atom-updated')?.({ id: 'memory:m1' }); });
    }
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

describe.each(['memory', 'category'])('AtomReader %s link draft protection', kind => {
  const originalValue = (): AtomWithTags => ({
    ...memory('Original prose', '2026-01-01T00:00:00Z'),
    id: kind === 'memory' ? 'memory:m1' : 'category:c1',
    entity_ids: ['entity:old'], entity_names: ['Old link'],
    summaries_revision: 1, description: '', category_kind: 'topic',
    members: [{
      id: 'memory:member', memory_id: 'member', memory_ref: 'M1', summary: 'Fictional member',
      memory_type: 'event', happened_at: null, created_at: null, status: 'Active', cited: false,
    }],
  });
  const mount = async (id: string) => {
    useUIStore.setState({ tabs: [{ id: 'tab-1', stack: [], stackIndex: 0, ordinal: 1 }], activeTabId: 'tab-1' });
    const container = document.createElement('div');
    const root = createRoot(container);
    await act(async () => { root.render(<ReaderHarness atomId={id} initialEditing />); });
    return { container, root };
  };
  const edit = async (container: HTMLDivElement) => {
    const textarea = [...container.querySelectorAll('textarea')].find(input => input.value === 'Original prose')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(textarea, 'Fictional draft');
      textarea.dispatchEvent(new Event('input', { bubbles: true }));
    });
    return textarea;
  };

  it.each(['settled', 'before-effects', 'direct-response'])('preserves a draft when external prose arrives %s', async order => {
    const original = originalValue();
    const linked = { ...original, entity_ids: [], entity_names: [], members: [] };
    let finishLink!: (value: AtomWithTags) => void;
    transport.invoke.mockResolvedValueOnce(original)
      .mockImplementationOnce(() => new Promise(resolve => { finishLink = resolve; }))
      .mockResolvedValue({ ...linked, content: 'External prose' });
    const { container, root } = await mount(original.id);
    try {
      const textarea = await edit(container);
      if (kind === 'category') await act(async () => {
        [...container.querySelectorAll('button')].find(button => button.textContent?.startsWith('Memories in this dossier'))!.click();
      });
      await act(async () => {
        [...container.querySelectorAll('button')].find(button => button.textContent === (kind === 'memory' ? 'Detach entity' : 'Detach'))!.click();
      });
      const refresh = () => transport.listeners.get('tagging-complete')?.({ atom_id: original.id });
      if (order === 'before-effects') {
        await act(async () => {
          finishLink(linked);
          await Promise.resolve();
          // Complete the refresh before React restores the dirty ref through its effect.
          refresh();
          await Promise.resolve();
        });
      } else {
        await act(async () => { finishLink(order === 'direct-response' ? { ...linked, content: 'External prose' } : linked); });
        if (order === 'settled') {
          expect(textarea.value).toBe('Fictional draft');
          expect(useUIStore.getState().tabs[0].retired).not.toBe(true);
          if (kind === 'memory') expect(container.querySelector('[data-entity-links]')?.textContent).toBe('');
          else expect(container.textContent).toContain('Memories in this dossier (0)');
          await act(async () => { refresh(); });
        }
      }
      expect(container.querySelectorAll('textarea')).toContain(textarea);
      expect(textarea.value).toBe('Fictional draft');
      expect(textarea.readOnly).toBe(true);
      expect(useUIStore.getState().tabs[0].retired).toBe(true);
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it.each([
    ['Save', 'success'], ['Save', 'failure'], ['Approve', 'success'], ['Approve', 'failure'],
  ])('handles a queued external refresh after %s %s', async (action, outcome) => {
    const original = originalValue();
    if (action === 'Save') Object.assign(original, {
      approved_at: original.updated_at, approved_summary: original.content, approved_description: '',
    });
    let finish!: (value: AtomWithTags) => void;
    let fail!: (error: Error) => void;
    transport.invoke.mockResolvedValueOnce(original)
      .mockImplementationOnce(() => new Promise((resolve, reject) => { finish = resolve; fail = reject; }))
      .mockResolvedValue({ ...original, content: 'External prose' });
    const { container, root } = await mount(original.id);
    try {
      const textarea = action === 'Save' ? await edit(container) : null;
      await act(async () => { [...container.querySelectorAll('button')].find(button => button.textContent === action)!.click(); });
      await act(async () => { transport.listeners.get('tagging-complete')?.({ atom_id: original.id }); });
      await act(async () => {
        if (outcome === 'failure') fail(new Error('Fictional mutation failure'));
        else finish({ ...original, content: action === 'Save' ? 'Fictional draft' : original.content });
      });
      const keepDraft = action === 'Save' && outcome === 'failure';
      expect(Boolean(useUIStore.getState().tabs[0].retired)).toBe(keepDraft);
      if (keepDraft) {
        expect(textarea!.value).toBe('Fictional draft');
        expect(textarea!.readOnly).toBe(true);
      } else {
        expect(container.textContent).not.toContain('read-only snapshot');
        expect([...container.querySelectorAll('textarea')].some(input => input.value === 'External prose')
          || container.textContent?.includes('External prose')).toBe(true);
      }
    } finally {
      await act(async () => { root.unmount(); });
    }
  });
});
