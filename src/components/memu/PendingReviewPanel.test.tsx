import { act } from 'react';
import { createRoot } from 'react-dom/client';
import { afterEach, describe, expect, it, vi } from 'vitest';

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const transport = vi.hoisted(() => ({
  invoke: vi.fn(),
  listeners: new Map<string, (payload: unknown) => void>(),
  subscribe: vi.fn((event: string, callback: (payload: unknown) => void) => {
    transport.listeners.set(event, callback);
    return () => transport.listeners.delete(event);
  }),
}));

vi.mock('../../lib/transport', () => ({ getTransport: () => transport }));

import { PendingReviewPanel } from './PendingReviewPanel';

afterEach(() => {
  transport.invoke.mockReset();
  transport.listeners.clear();
});

describe('PendingReviewPanel', () => {
  it('does not let an in-flight reload overwrite a newer review event', async () => {
    let finishLoad!: (value: unknown) => void;
    transport.invoke
      .mockImplementationOnce(() => new Promise(resolve => { finishLoad = resolve; }))
      .mockResolvedValueOnce({
        items: [],
        categories: [{ id: 'category:c2', label: 'Fresh category', summary: 'new' }],
        soul_summaries: [],
        summaries_revision: 2,
      });
    const container = document.createElement('div');
    const root = createRoot(container);

    await act(async () => { root.render(<PendingReviewPanel />); });
    await act(async () => {
      transport.listeners.get('memu-reviews-changed')?.({
        category_id: 'category:c1',
        summaries_revision: 2,
        pending: false,
      });
      finishLoad({
        items: [],
        categories: [{ id: 'category:c1', label: 'Stale category', summary: 'old' }],
        soul_summaries: [],
        summaries_revision: 2,
      });
    });

    expect(transport.invoke).toHaveBeenCalledTimes(2);
    expect(container.querySelector('input')?.value).toBe('Fresh category');
    await act(async () => { root.unmount(); });
  });

  it('applies soul-summary events', async () => {
    transport.invoke.mockResolvedValue({
      items: [],
      categories: [],
      soul_summaries: [{ id: 'soul-summary:narrative_self', kind: 'narrative_self', summary: 'old' }],
      summaries_revision: 1,
    });
    const container = document.createElement('div');
    const root = createRoot(container);

    await act(async () => { root.render(<PendingReviewPanel />); });
    await act(async () => {
      transport.listeners.get('memu-soul-summary-changed')?.({
        id: 'soul-summary:narrative_self',
        kind: 'narrative_self',
        summary: 'new soul summary',
        summaries_revision: 2,
      });
    });

    expect(container.textContent).toContain('new soul summary');
    await act(async () => { root.unmount(); });
  });

  it('retires a conflicting review without offering refresh', async () => {
    const onStale = vi.fn();
    transport.invoke.mockResolvedValue({
      items: [],
      categories: [{ id: 'category:c1', label: 'Edited category', summary: 'draft' }],
      soul_summaries: [],
      summaries_revision: 1,
    });
    const container = document.createElement('div');
    const root = createRoot(container);

    await act(async () => { root.render(<PendingReviewPanel onStale={onStale} />); });
    const input = container.querySelector('input')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')?.set?.call(input, 'Changed category');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    expect([...container.querySelectorAll('button')].find(button => button.textContent === 'Refresh')?.disabled).toBe(true);
    await act(async () => {
      transport.listeners.get('memu-reviews-changed')?.({
        category_id: 'category:c1',
        summaries_revision: 2,
        pending: true,
      });
    });

    expect(onStale).toHaveBeenCalledOnce();
    const banner = [...container.querySelectorAll('p')].find(node => node.textContent?.includes('read-only snapshot'));
    expect(banner?.textContent).toContain('Memory summaries changed');
    expect(banner?.classList.contains('sticky')).toBe(true);
    expect([...container.querySelectorAll('button')].some(button => button.textContent === 'Refresh')).toBe(false);
    expect([...container.querySelectorAll('textarea')].every(textarea => textarea.readOnly)).toBe(true);
    await act(async () => {
      transport.listeners.get('memu-reviews-changed')?.({
        category_id: 'category:c1',
        summaries_revision: 3,
        pending: false,
      });
    });
    expect(container.querySelector('input')).not.toBeNull();
    await act(async () => { root.unmount(); });
  });

  it('updates clean memory rows and retires dirty conflicting rows', async () => {
    const onStale = vi.fn();
    transport.invoke.mockResolvedValue({
      items: [{ id: 'memory:m1', summary: 'old memory', memory_type: 'knowledge' }],
      categories: [],
      soul_summaries: [],
      summaries_revision: 1,
    });
    const container = document.createElement('div');
    const root = createRoot(container);

    await act(async () => { root.render(<PendingReviewPanel onStale={onStale} />); });
    await act(async () => {
      transport.listeners.get('atom-updated')?.({
        id: 'memory:m1', content: 'external memory', approved_at: null,
      });
    });
    const textarea = container.querySelector('textarea')!;
    expect(textarea.value).toBe('external memory');

    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')?.set?.call(textarea, 'local draft');
      textarea.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await act(async () => {
      transport.listeners.get('atom-updated')?.({
        id: 'memory:m1', content: 'new external memory', approved_at: null,
      });
    });

    expect(onStale).toHaveBeenCalledOnce();
    expect(textarea.value).toBe('local draft');
    expect(textarea.readOnly).toBe(true);
    await act(async () => { root.unmount(); });
  });

  it('does not let an in-flight reload restore a memory changed by an event', async () => {
    let finishLoad!: (value: unknown) => void;
    transport.invoke
      .mockImplementationOnce(() => new Promise(resolve => { finishLoad = resolve; }))
      .mockResolvedValueOnce({ items: [], categories: [], soul_summaries: [], summaries_revision: 1 });
    const container = document.createElement('div');
    const root = createRoot(container);

    await act(async () => { root.render(<PendingReviewPanel />); });
    await act(async () => {
      transport.listeners.get('atom-updated')?.({
        id: 'memory:m1', content: 'approved elsewhere', approved_at: '2026-01-01T00:00:00Z',
      });
      finishLoad({
        items: [{ id: 'memory:m1', summary: 'stale pending row', memory_type: 'knowledge' }],
        categories: [],
        soul_summaries: [],
        summaries_revision: 1,
      });
    });

    expect(container.textContent).not.toContain('stale pending row');
    expect(transport.invoke).toHaveBeenCalledTimes(2);
    await act(async () => { root.unmount(); });
  });

  it('retires instead of reloading over a draft when a soul event has no revision', async () => {
    const onStale = vi.fn();
    transport.invoke.mockResolvedValue({
      items: [],
      categories: [{ id: 'category:c1', label: 'Category', summary: 'original' }],
      soul_summaries: [],
      summaries_revision: 1,
    });
    const container = document.createElement('div');
    const root = createRoot(container);

    await act(async () => { root.render(<PendingReviewPanel onStale={onStale} />); });
    const input = container.querySelector('input')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')?.set?.call(input, 'local title');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await act(async () => {
      transport.listeners.get('memu-soul-summary-changed')?.({ kind: 'narrative_self', summary: 'external' });
    });

    expect(onStale).toHaveBeenCalledOnce();
    expect(container.querySelector('input')?.value).toBe('local title');
    expect(transport.invoke).toHaveBeenCalledTimes(1);
    await act(async () => { root.unmount(); });
  });
});
