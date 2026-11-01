import { describe, expect, it } from 'vitest';
import { normalizeServerEvent } from './event-normalizer';

describe('normalizeServerEvent', () => {
  it('preserves database/conversation scope on durable chat changes', () => {
    const event = { type: 'ChatStateChanged', database_id: 'test-db', conversation_id: 'test-chat' };
    expect(normalizeServerEvent(event)).toEqual({ event: 'chat-state-changed', payload: event });
  });
  it('normalizes memU review changes', () => {
    expect(normalizeServerEvent({
      type: 'MemuReviewsChanged',
      category_id: 'category:c1',
      summaries_revision: 9,
      pending: false,
      user_id: 'owner',
      soul_id: 'soul',
    })).toEqual({
      event: 'memu-reviews-changed',
      payload: { category_id: 'category:c1', summaries_revision: 9, pending: false },
    });
  });

  it('normalizes soul-summary changes', () => {
    const summary = { kind: 'narrative_self', summaries_revision: 10 };
    expect(normalizeServerEvent({ type: 'MemuSoulSummaryChanged', summary })).toEqual({
      event: 'memu-soul-summary-changed',
      payload: summary,
    });
  });

  it('normalizes scoped memory deletion', () => {
    expect(normalizeServerEvent({
      type: 'MemuMemoryDeleted', id: 'memory:m1', user_id: 'owner', soul_id: 'soul',
    })).toEqual({ event: 'memu-memory-deleted', payload: { id: 'memory:m1' } });
  });
});
