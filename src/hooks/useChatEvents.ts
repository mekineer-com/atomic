import { useEffect } from 'react';
import { getTransport, TRANSPORT_CONNECTION_EVENT } from '../lib/transport';
import { useChatStore, ChatMessageWithContext } from '../stores/chat';
import { useDatabasesStore } from '../stores/databases';

interface ChatStreamDelta {
  conversation_id: string;
  content: string;
}

interface ChatToolStart {
  conversation_id: string;
  tool_call_id: string;
  tool_name: string;
  tool_input: unknown;
}

interface ChatToolComplete {
  conversation_id: string;
  tool_call_id: string;
  results_count: number;
  is_error: boolean;
}

interface ChatComplete {
  conversation_id: string;
  message: ChatMessageWithContext;
}

interface ChatError {
  conversation_id: string;
  error: string;
}

export function useChatEvents(conversationId: string | null) {
  const appendStreamContent = useChatStore(s => s.appendStreamContent);
  const startStreamingToolCall = useChatStore(s => s.startStreamingToolCall);
  const completeStreamingToolCall = useChatStore(s => s.completeStreamingToolCall);
  const completeMessage = useChatStore(s => s.completeMessage);
  const setStreamingError = useChatStore(s => s.setStreamingError);

  useEffect(() => {
    if (!conversationId) return;

    const transport = getTransport();
    const unsubs: Array<() => void> = [];
    const refresh = () => { void useChatStore.getState().refreshConversation(); };
    const onConnection = (event: Event) => {
      if ((event as CustomEvent<{ connected: boolean }>).detail.connected) refresh();
    };
    window.addEventListener(TRANSPORT_CONNECTION_EVENT, onConnection);
    unsubs.push(() => window.removeEventListener(TRANSPORT_CONNECTION_EVENT, onConnection));
    unsubs.push(transport.subscribe('server-events-lagged', refresh));
    unsubs.push(transport.subscribe<{ database_id: string; conversation_id: string }>('chat-state-changed', payload => {
      if (payload.conversation_id === conversationId
        && payload.database_id === useDatabasesStore.getState().activeId) refresh();
    }));

    unsubs.push(transport.subscribe<ChatStreamDelta>('chat-stream-delta', (payload) => {
      if (payload.conversation_id === conversationId) {
        appendStreamContent(payload.content);
      }
    }));

    unsubs.push(transport.subscribe<ChatToolStart>('chat-tool-start', (payload) => {
      if (payload.conversation_id === conversationId) {
        startStreamingToolCall({
          tool_call_id: payload.tool_call_id,
          tool_name: payload.tool_name,
          tool_input: payload.tool_input,
        });
      }
    }));

    unsubs.push(transport.subscribe<ChatToolComplete>('chat-tool-complete', (payload) => {
      if (payload.conversation_id === conversationId) {
        completeStreamingToolCall({
          tool_call_id: payload.tool_call_id,
          results_count: payload.results_count,
          is_error: payload.is_error,
        });
      }
    }));

    unsubs.push(transport.subscribe<ChatComplete>('chat-complete', (payload) => {
      if (payload.conversation_id === conversationId) {
        if (useChatStore.getState().currentConversation?.atomic_chat_state?.phase !== 'closing') {
          completeMessage(payload.message);
        }
        if (useChatStore.getState().currentConversation?.atomic_chat_state) refresh();
      }
    }));

    unsubs.push(transport.subscribe<ChatError>('chat-error', (payload) => {
      if (payload.conversation_id === conversationId) {
        setStreamingError(payload.error);
      }
    }));

    return () => {
      unsubs.forEach((unsub) => unsub());
    };
  }, [conversationId, appendStreamContent, startStreamingToolCall, completeStreamingToolCall, completeMessage, setStreamingError]);
}
