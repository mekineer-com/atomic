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
    transport.invoke.mockReturnValue(new Promise(resolve => { finishLoad = resolve; }));
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

    expect(container.textContent).not.toContain('Stale category');
    await act(async () => { root.unmount(); });
  });
});
