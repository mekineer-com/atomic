import { act, createElement } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const transport = vi.hoisted(() => ({
  invoke: vi.fn(),
  listeners: new Set<(atom: { id: string }) => void>(),
  subscribe: vi.fn((_event: string, callback: (atom: { id: string }) => void) => {
    transport.listeners.add(callback);
    return () => { transport.listeners.delete(callback); };
  }),
}));
vi.mock('../../lib/transport', () => ({ getTransport: () => transport }));

import { EntityManager, EntityReader, entitySaveCommand } from './EntityManager';

const entity = (id: string, linked = false) => ({
  id, atom_id: `entity:${id}`, name: `Fictional ${id}`, normalized: id,
  entity_type: 'person', properties: { aliases: ['Original alias'], relationship: 'Original prose' },
  is_relationship: false, ignored: false, orphan: !linked, linked_memory_count: linked ? 1 : 0,
  last_mentioned_at: null, created_at: null, updated_at: null,
  memories: linked ? [{ id: 'memory:m1', memory_ref: '[M1]', memory_type: 'knowledge', summary: 'Fresh fictional link', happened_at: null, created_at: null }] : [],
});

let root: Root;
let container: HTMLDivElement;

beforeEach(() => {
  container = document.createElement('div');
  document.body.append(container);
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => { root.unmount(); });
  container.remove();
  transport.invoke.mockReset();
  transport.listeners.clear();
  vi.restoreAllMocks();
});

async function click(text: string) {
  await act(async () => {
    [...container.querySelectorAll('button')].find(button => button.textContent === text)!.click();
  });
}

function emitMemory() {
  // The serialized WebSocket Atom has no entity_ids/entity_names.
  transport.listeners.forEach(callback => callback({ id: 'memory:m1' }));
}

describe('entitySaveCommand', () => {
  it.each([
    [false, false, 'update_memu_entity'],
    [false, true, 'promote_memu_relationship'],
    [true, false, 'update_memu_relationship'],
    [true, true, 'promote_memu_relationship'],
  ])('routes relationship=%s promoting=%s to %s', (isRelationship, promoting, command) => {
    expect(entitySaveCommand(isRelationship, promoting)).toBe(command);
  });
});

describe('entity reader ownership and link refresh', () => {
  it.each(['save', 'delete'])('keeps B selected after A completes %s', async action => {
    let finish!: () => void;
    let entities = [entity('A'), entity('C'), entity('B')];
    transport.invoke.mockImplementation((command: string, args: { id: string }) => {
      if (command === 'list_memu_entities') return Promise.resolve({ entities });
      if (command === 'get_memu_entity') return Promise.resolve(entity(args.id));
      if (command === 'update_memu_entity' || command === 'delete_memu_entity') {
        return new Promise(resolve => { finish = () => {
          if (action === 'delete') entities = entities.filter(row => row.id !== 'A');
          resolve({});
        }; });
      }
      throw new Error(command);
    });
    vi.spyOn(window, 'confirm').mockReturnValue(true);
    await act(async () => { root.render(createElement(EntityManager)); });
    if (action === 'save') { await click('Edit'); await click('Save'); }
    else await click('Delete');
    await act(async () => {
      [...container.querySelectorAll<HTMLButtonElement>('aside button')].find(button => button.textContent?.startsWith('Fictional B'))!.click();
    });
    if (action === 'delete') {
      await click('Edit');
      await act(async () => {
        const input = container.querySelector('article input')!;
        Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, 'Fictional B draft');
        input.dispatchEvent(new Event('input', { bubbles: true }));
      });
    }
    await act(async () => { finish(); });
    expect(container.querySelector('article h1')?.textContent).toBe('Fictional B');
    expect([...container.querySelectorAll('aside button')].find(button => button.textContent?.startsWith('Fictional B'))?.className).toContain('bg-[var(--color-bg-hover)]');
    if (action === 'delete') expect(container.querySelector<HTMLInputElement>('article input')?.value).toBe('Fictional B draft');
  });

  it.each(['old-first', 'new-first'])('initializes from the latest read when an event overlaps mount (%s)', async order => {
    let old!: (value: ReturnType<typeof entity>) => void;
    let fresh!: (value: ReturnType<typeof entity>) => void;
    transport.invoke.mockImplementationOnce(() => new Promise(resolve => { old = resolve; }))
      .mockImplementationOnce(() => new Promise(resolve => { fresh = resolve; }));
    await act(async () => { root.render(createElement(EntityReader, { entityId: 'A' })); });
    await act(async () => { emitMemory(); });
    for (const read of order === 'old-first' ? ['old', 'new'] : ['new', 'old']) {
      await act(async () => { if (read === 'old') old(entity('A')); else fresh(entity('A', true)); });
    }
    expect(container.textContent).toContain('Fresh fictional link');
    await click('Edit');
    expect(container.querySelector('input')?.value).toBe('Fictional A');
  });

  it.each(['resolve', 'reject'])('keeps entity drafts and the newest links when an older read will %s', async outcome => {
    let old!: (value: ReturnType<typeof entity>) => void;
    let fail!: (error: Error) => void;
    let fresh!: (value: ReturnType<typeof entity>) => void;
    transport.invoke.mockResolvedValueOnce({ ...entity('A'), is_relationship: true })
      .mockImplementationOnce(() => new Promise((resolve, reject) => { old = resolve; fail = reject; }))
      .mockImplementationOnce(() => new Promise(resolve => { fresh = resolve; }));
    await act(async () => { root.render(createElement(EntityReader, { entityId: 'A' })); });
    await click('Edit');
    await act(async () => {
      [...container.querySelectorAll('article input, article textarea')].slice(0, 4).forEach((input, index) => {
        const element = input as HTMLInputElement | HTMLTextAreaElement;
        const prototype = element.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
        Object.getOwnPropertyDescriptor(prototype, 'value')!.set!.call(element, `Fictional draft ${index}`);
        element.dispatchEvent(new Event('input', { bubbles: true }));
      });
    });
    const article = container.querySelector('article')!;
    article.scrollTop = 83;
    await act(async () => { emitMemory(); emitMemory(); });
    await act(async () => { fresh(entity('A', true)); });
    await act(async () => { if (outcome === 'resolve') old(entity('A')); else fail(new Error('Obsolete read failed')); });
    expect(container.textContent).toContain('Fresh fictional link');
    expect(container.textContent).not.toContain('Obsolete read failed');
    expect([...container.querySelectorAll('article input, article textarea')].slice(0, 4).map(input => (input as HTMLInputElement).value))
      .toEqual([0, 1, 2, 3].map(index => `Fictional draft ${index}`));
    expect(article.scrollTop).toBe(83);
  });

  it.each([false, true])('handles an event during an explicit save refresh (read fails=%s)', async fails => {
    let finish!: (value: ReturnType<typeof entity>) => void;
    const saved = { ...entity('A'), name: 'Saved fictional name' };
    transport.invoke.mockResolvedValueOnce(entity('A')).mockResolvedValueOnce({});
    if (fails) transport.invoke.mockRejectedValueOnce(new Error('Fictional refresh failure'));
    else transport.invoke.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
    transport.invoke.mockResolvedValueOnce({ ...entity('A', true), name: saved.name });
    await act(async () => { root.render(createElement(EntityReader, { entityId: 'A' })); });
    await click('Edit');
    await act(async () => {
      const input = container.querySelector('input')!;
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, saved.name);
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await click('Save');
    if (fails) await act(async () => {
      const input = container.querySelector('input')!;
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, 'New fictional draft');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await act(async () => { emitMemory(); });
    if (fails) expect(container.querySelector('input')?.value).toBe('New fictional draft');
    else {
      await act(async () => { finish(entity('A')); });
      expect(container.querySelector('h1')?.textContent).toBe(saved.name);
    }
    expect(container.textContent).toContain('Fresh fictional link');
  });

  it('does not let a pre-merge link read erase the merged memory list', async () => {
    let finish!: (value: ReturnType<typeof entity>) => void;
    let reads = 0;
    transport.invoke.mockImplementation((command: string) => {
      if (command === 'get_memu_entity') {
        return ++reads === 1 ? Promise.resolve(entity('A')) : new Promise(resolve => { finish = resolve; });
      }
      if (command === 'list_memu_entities') return Promise.resolve({ entities: [entity('B')] });
      if (command === 'preview_memu_entity_merge') return Promise.resolve({
        can_merge: true, conflicts: [], warnings: [],
        impact: { memory_count: 1, speaker_memory_count: 0, current_triple_count: 1, historical_triple_count: 0, category_titles: [], aliases: [], source_refs: [] },
      });
      if (command === 'merge_memu_entities') return Promise.resolve(entity('A', true));
      throw new Error(command);
    });
    vi.spyOn(window, 'confirm').mockReturnValue(true);
    await act(async () => { root.render(createElement(EntityReader, { entityId: 'A' })); });
    await act(async () => { emitMemory(); });
    await click('Merge duplicate');
    await click('Preview merge');
    await click('Keep Fictional A and merge selected');
    await act(async () => { finish(entity('A')); });
    expect(container.textContent).toContain('Fresh fictional link');
  });
});
