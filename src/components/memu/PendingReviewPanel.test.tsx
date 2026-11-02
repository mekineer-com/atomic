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
  it('approves with loaded category identity rather than draft fields', async () => {
    transport.invoke.mockResolvedValueOnce({
      items: [], categories: [{ id: 'category:c1', label: 'Loaded title', description: 'Loaded brief', summary: 'Shown' }],
      soul_summaries: [], summaries_revision: 4,
    }).mockRejectedValueOnce('summary_snapshot_stale');
    const container = document.createElement('div');
    const mounted = createRoot(container);
    await act(async () => { mounted.render(<PendingReviewPanel />); });
    const input = container.querySelector('input')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')?.set?.call(input, 'Draft title');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await act(async () => {
      [...container.querySelectorAll('button')].find(button => button.textContent === 'Save + approve')!.click();
    });
    expect(transport.invoke).toHaveBeenLastCalledWith('update_category_summary', expect.objectContaining({
      title: 'Draft title', displayed_title: 'Loaded title', displayed_description: 'Loaded brief',
      displayed_summary: 'Shown', summaries_revision: 4,
    }));
    await act(async () => { mounted.unmount(); });
  });

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
    await act(async () => {
      transport.listeners.get('memu-soul-summary-changed')?.({
        id: 'soul-summary:narrative_self',
        kind: 'narrative_self',
        summary: 'new soul summary',
        pending: false,
        summaries_revision: 3,
      });
    });
    expect(container.textContent).not.toContain('new soul summary');
    await act(async () => { root.unmount(); });
  });

  it('removes narrative self after Save + approve and uses matching text geometry', async () => {
    transport.invoke
      .mockResolvedValueOnce({
        items: [],
        categories: [],
        soul_summaries: [{
          id: 'soul-summary:narrative_self',
          kind: 'narrative_self',
          label: 'Narrative Self',
          approved_summary: 'Same shaped paragraph.',
          summary: 'Same shaped paragraph.',
        }],
        summaries_revision: 1,
      })
      .mockResolvedValueOnce({
        id: 'soul-summary:narrative_self',
        kind: 'narrative_self',
        summary: 'Edited paragraph.',
        approved_summary: 'Edited paragraph.',
        pending: false,
        summaries_revision: 2,
      });
    const container = document.createElement('div');
    const root = createRoot(container);

    await act(async () => { root.render(<PendingReviewPanel />); });
    const approved = container.querySelector<HTMLTextAreaElement>('textarea[aria-label="Approved summary"]')!;
    const textarea = container.querySelector<HTMLTextAreaElement>('textarea[aria-label="Proposed summary"]')!;
    expect(approved.readOnly).toBe(true);
    expect(approved.value).toBe('Same shaped paragraph.');
    for (const className of ['min-h-[21rem]', 'font-sans', 'text-sm', 'leading-5', 'tracking-normal', 'overflow-y-scroll', 'focus:overflow-y-auto']) {
      expect(approved.classList.contains(className)).toBe(true);
      expect(textarea.classList.contains(className)).toBe(true);
    }
    Object.defineProperties(approved, {
      scrollHeight: { configurable: true, value: 500 },
      offsetHeight: { configurable: true, value: 338 },
      clientHeight: { configurable: true, value: 336 },
    });
    await act(async () => { approved.dispatchEvent(new FocusEvent('focusin', { bubbles: true })); });
    expect(approved.style.height).toBe('502px');
    const button = [...container.querySelectorAll('button')].find(node => node.textContent === 'Approve')!;
    await act(async () => { approved.dispatchEvent(new FocusEvent('focusout', { bubbles: true, relatedTarget: button })); });
    expect(approved.style.height).toBe('502px');
    await act(async () => { approved.dispatchEvent(new FocusEvent('focusout', { bubbles: true })); });
    expect(approved.style.height).toBe('');
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')?.set?.call(textarea, 'Edited paragraph.');
      textarea.dispatchEvent(new Event('input', { bubbles: true }));
    });
    const saveButton = [...container.querySelectorAll('button')].find(node => node.textContent === 'Save + approve')!;
    await act(async () => { saveButton.click(); });

    expect(transport.invoke).toHaveBeenLastCalledWith('update_soul_summary', expect.objectContaining({
      kind: 'narrative_self',
      summary: 'Edited paragraph.',
    }));
    expect(container.textContent).not.toContain('Narrative Self');
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
      items: [{
        id: 'memory:m1', memory_ref: 42, summary: 'old memory', memory_type: 'knowledge',
        dossier_usages: [{ id: 'category:c1', name: 'Test dossier', cited: false }],
      }],
      categories: [],
      soul_summaries: [],
      summaries_revision: 1,
    });
    const container = document.createElement('div');
    const root = createRoot(container);

    await act(async () => { root.render(<PendingReviewPanel onStale={onStale} />); });
    expect(container.textContent).toContain('M42Used byTest dossier');
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

  it.each([false, true])('handles a memory deleted elsewhere without losing a dirty draft (%s)', async (dirty) => {
    const onStale = vi.fn();
    transport.invoke.mockResolvedValue({
      items: [{ id: 'memory:m1', summary: 'pending memory', memory_type: 'knowledge' }],
      categories: [], soul_summaries: [], summaries_revision: 1,
    });
    const container = document.createElement('div');
    const root = createRoot(container);
    await act(async () => { root.render(<PendingReviewPanel onStale={onStale} />); });
    try {
      const textarea = container.querySelector('textarea')!;
      if (dirty) {
        await act(async () => {
          Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')?.set?.call(textarea, 'local draft');
          textarea.dispatchEvent(new Event('input', { bubbles: true }));
        });
      }
      await act(async () => { transport.listeners.get('memu-memory-deleted')?.({ id: 'memory:other' }); });
      expect(container.querySelector('textarea')).not.toBeNull();
      expect(onStale).not.toHaveBeenCalled();
      await act(async () => { transport.listeners.get('memu-memory-deleted')?.({ id: 'memory:m1' }); });
      if (dirty) {
        expect(textarea.value).toBe('local draft');
        expect(textarea.readOnly).toBe(true);
        expect(onStale).toHaveBeenCalledOnce();
      } else {
        expect(container.querySelector('textarea')).toBeNull();
        expect(onStale).not.toHaveBeenCalled();
      }
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it('does not let an in-flight reload restore a deleted memory', async () => {
    let finishLoad!: (value: unknown) => void;
    transport.invoke
      .mockImplementationOnce(() => new Promise(resolve => { finishLoad = resolve; }))
      .mockResolvedValueOnce({ items: [], categories: [], soul_summaries: [], summaries_revision: 1 });
    const container = document.createElement('div');
    const root = createRoot(container);
    await act(async () => { root.render(<PendingReviewPanel />); });
    try {
      await act(async () => {
        transport.listeners.get('memu-memory-deleted')?.({ id: 'memory:m1' });
        finishLoad({
          items: [{ id: 'memory:m1', summary: 'deleted pending row', memory_type: 'knowledge' }],
          categories: [], soul_summaries: [], summaries_revision: 1,
        });
      });
      expect(container.querySelector('textarea')).toBeNull();
      expect(transport.invoke).toHaveBeenCalledTimes(2);
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it.each([false, true])('does not retire a draft on its own delete while another review is busy (%s)', async (concurrent) => {
    const onStale = vi.fn();
    const container = document.createElement('div');
    transport.invoke.mockResolvedValueOnce({
      items: [
        { id: 'memory:m1', summary: 'pending memory', memory_type: 'knowledge' },
        ...(concurrent ? [{ id: 'memory:m2', summary: 'other memory', memory_type: 'knowledge' }] : []),
      ],
      categories: [], soul_summaries: [], summaries_revision: 1,
    }).mockImplementationOnce(async () => {
      if (concurrent) {
        [...container.querySelectorAll('button')].find(button => button.textContent === 'Approve')!.click();
      }
      transport.listeners.get('memu-memory-deleted')?.({ id: 'memory:m1' });
      return { status: 'deleted' };
    }).mockResolvedValueOnce({ status: 'approved' });
    const root = createRoot(container);
    await act(async () => { root.render(<PendingReviewPanel onStale={onStale} />); });
    try {
      const textarea = container.querySelector('textarea')!;
      await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')?.set?.call(textarea, 'local draft');
        textarea.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await act(async () => {
        [...container.querySelectorAll('button')].find(button => button.textContent === 'Delete')!.click();
      });
      expect(onStale).not.toHaveBeenCalled();
      expect(container.querySelector('textarea')).toBeNull();
      expect(transport.invoke).toHaveBeenCalledWith('delete_memory', { id: 'memory:m1', displayed_summary: 'pending memory' });
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it.each([false, true])('locks only the saving memory until its request finishes (%s)', async (succeeds) => {
    let finishSave!: (result: unknown) => void;
    let failSave!: (error: unknown) => void;
    transport.invoke.mockResolvedValueOnce({
      items: [
        { id: 'memory:m1', summary: 'pending memory', memory_type: 'knowledge' },
        { id: 'memory:m2', summary: 'other memory', memory_type: 'knowledge' },
      ],
      categories: [], soul_summaries: [], summaries_revision: 1,
    }).mockImplementationOnce(() => new Promise((resolve, reject) => { finishSave = resolve; failSave = reject; }));
    const container = document.createElement('div');
    const root = createRoot(container);
    try {
      await act(async () => { root.render(<PendingReviewPanel />); });
      const [textarea, other] = container.querySelectorAll('textarea');
      await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(textarea, 'saving draft');
        textarea.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await act(async () => {
        [...container.querySelectorAll('button')].find(button => button.textContent === 'Save + approve')!.click();
      });
      expect(transport.invoke).toHaveBeenLastCalledWith('update_memory_summary', {
        id: 'memory:m1', summary: 'saving draft', displayed_summary: 'pending memory',
      });
      expect(other.readOnly).toBe(false);
      expect(textarea.readOnly).toBe(true);
      await act(async () => {
        if (succeeds) finishSave({ status: 'approved' });
        else failSave('Fictional save failed');
      });
      expect(container.contains(textarea)).toBe(!succeeds);
      if (!succeeds) {
        expect(textarea.value).toBe('saving draft');
        expect(textarea.readOnly).toBe(false);
      }
      expect(other.readOnly).toBe(false);
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it.each([false, true])('keeps a retired draft when its pending save finishes (%s)', async (succeeds) => {
    const onStale = vi.fn();
    let failSave!: (error: unknown) => void;
    let finishSave!: (result: unknown) => void;
    transport.invoke.mockResolvedValueOnce({
      items: [{ id: 'memory:m1', summary: 'pending memory', memory_type: 'knowledge' }],
      categories: [], soul_summaries: [], summaries_revision: 1,
    }).mockImplementationOnce(() => new Promise((resolve, reject) => { finishSave = resolve; failSave = reject; }));
    const container = document.createElement('div');
    const root = createRoot(container);
    await act(async () => { root.render(<PendingReviewPanel onStale={onStale} />); });
    try {
      const textarea = container.querySelector('textarea')!;
      await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')?.set?.call(textarea, 'saving draft');
        textarea.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await act(async () => {
        [...container.querySelectorAll('button')].find(button => button.textContent === 'Save + approve')!.click();
      });
      await act(async () => {
        // A committed save can be deleted using its new summary before its HTTP response arrives.
        if (succeeds) transport.listeners.get('atom-updated')?.({ id: 'memory:m1', content: 'saving draft', approved_at: '2026-01-01T00:00:00Z' });
        transport.listeners.get('memu-memory-deleted')?.({ id: 'memory:m1' });
        if (succeeds) finishSave({ status: 'approved' });
        else failSave('memory not found');
      });
      expect(container.contains(textarea)).toBe(true);
      expect(textarea.value).toBe('saving draft');
      expect(textarea.readOnly).toBe(true);
      expect(onStale).toHaveBeenCalledOnce();
    } finally {
      await act(async () => { root.unmount(); });
    }
  });

  it.each(['category', 'soul'])('keeps a retired %s review when its pending approval succeeds', async (kind) => {
    let finishApprove!: (result: unknown) => void;
    const review = { id: `${kind}:fictional`, kind: 'narrative_self', label: 'Fictional review', summary: 'Review prose' };
    transport.invoke.mockResolvedValueOnce({
      items: [{ id: 'memory:m1', summary: 'pending memory', memory_type: 'knowledge' }],
      categories: kind === 'category' ? [review] : [],
      soul_summaries: kind === 'soul' ? [review] : [], summaries_revision: 1,
    }).mockImplementationOnce(() => new Promise(resolve => { finishApprove = resolve; }));
    const container = document.createElement('div');
    const root = createRoot(container);
    await act(async () => { root.render(<PendingReviewPanel />); });
    try {
      const memory = container.querySelector('textarea')!;
      const prose = container.querySelector('textarea[aria-label="Proposed summary"]')!;
      await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')?.set?.call(memory, 'local draft');
        memory.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await act(async () => {
        [...container.querySelectorAll('button')].find(button => button.textContent === 'Approve')!.click();
      });
      await act(async () => {
        transport.listeners.get('memu-memory-deleted')?.({ id: 'memory:m1' });
        finishApprove({ ...review, pending: false, summaries_revision: 2 });
      });
      expect(container.contains(prose)).toBe(true);
      expect(memory.value).toBe('local draft');
      expect(memory.readOnly).toBe(true);
    } finally {
      await act(async () => { root.unmount(); });
    }
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
