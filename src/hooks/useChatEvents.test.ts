import { expect, it, vi } from 'vitest';
import { act, createElement } from 'react';
import { createRoot } from 'react-dom/client';
import { useChatEvents } from './useChatEvents';
import { useChatStore } from '../stores/chat';
import { useDatabasesStore } from '../stores/databases';

const { handlers, invoke } = vi.hoisted(() => ({ handlers: new Map<string, (payload: any) => void>(), invoke: vi.fn() }));
vi.mock('../lib/transport', () => ({
  TRANSPORT_CONNECTION_EVENT: 'atomic:transport-connection',
  getTransport: () => ({ invoke, subscribe: (event: string, handler: (payload: any) => void) => {
    handlers.set(event, handler);
    return () => handlers.delete(event);
  } }),
}));

it('reloads scoped durable changes, reconnects and missed events without treating disconnection as failure', async () => {
  const previous = useChatStore.getState();
  const database = useDatabasesStore.getState().activeId;
  const refresh = vi.fn().mockResolvedValue(undefined);
  const complete = vi.fn();
  const container = document.createElement('div');
  const root = createRoot(container);
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true);
  useDatabasesStore.setState({ activeId: 'test-db' });
  useChatStore.setState({ refreshConversation: refresh, completeMessage: complete, isStreaming: true,
    currentConversation: { id: 'test-chat', title: null, created_at: '', updated_at: '', is_archived: false,
      tags: [], message_count: 0, last_message_preview: null,
      atomic_chat_state: { backend: 'api', phase: 'closing', active: null,
        recap: { request_message_id: 'control', outcome: { status: 'running' } } },
    } });
  function Subscriber() { useChatEvents('test-chat'); return null; }
  try {
    await act(async () => root.render(createElement(Subscriber)));
    handlers.get('chat-state-changed')!({ database_id: 'other-db', conversation_id: 'test-chat' });
    handlers.get('chat-state-changed')!({ database_id: 'test-db', conversation_id: 'other-chat' });
    window.dispatchEvent(new CustomEvent('atomic:transport-connection', { detail: { connected: false } }));
    expect(refresh).not.toHaveBeenCalled();
    expect(useChatStore.getState().isStreaming).toBe(true);
    handlers.get('chat-state-changed')!({ database_id: 'test-db', conversation_id: 'test-chat' });
    handlers.get('server-events-lagged')!({ skipped: 2 });
    window.dispatchEvent(new CustomEvent('atomic:transport-connection', { detail: { connected: true } }));
    handlers.get('chat-complete')!({ conversation_id: 'test-chat', message: { id: 'summary' } });
    expect(refresh).toHaveBeenCalledTimes(3);
    expect(complete).not.toHaveBeenCalled();
  } finally {
    await act(async () => root.unmount());
    expect(handlers.size).toBe(0);
    useChatStore.setState(previous);
    useDatabasesStore.setState({ activeId: database });
    vi.unstubAllGlobals();
  }
});
