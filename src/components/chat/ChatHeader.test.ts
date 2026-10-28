import { expect, it, vi } from 'vitest';
import { act, createElement } from 'react';
import { createRoot } from 'react-dom/client';
import { ChatHeader } from './ChatHeader';
import { useChatStore, type ConversationWithTags } from '../../stores/chat';

vi.mock('../../lib/transport', () => ({ getTransport: () => ({ invoke: () => Promise.resolve({ enabled: true }) }) }));
vi.mock('./ScopeEditor', () => ({ ScopeEditor: () => null }));

it('keeps End available to wait for managed work, offers close-out recovery and hides End when closed', async () => {
  const previous = useChatStore.getState();
  const rootElement = document.createElement('div');
  const root = createRoot(rootElement);
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true);
  const conversation: ConversationWithTags = { id: 'test-chat', title: 'Test', created_at: '', updated_at: '',
    is_archived: false, tags: [], message_count: 1, last_message_preview: null,
    atomic_chat_state: { backend: 'api', phase: 'open', active: { user_message_id: 'user', status: 'running', error: null } },
    context_meter: { estimated_tokens: 1234, limit_tokens: 180000, usable_tokens: 200000 },
  };
  useChatStore.setState({ isStreaming: true, isEndingSession: false });
  const render = async (value: ConversationWithTags) => {
    await act(async () => root.render(createElement(ChatHeader, { conversation: value, onBack: () => {} })));
  };
  try {
    await render(conversation);
    expect(rootElement.textContent).toContain('Estimated context:');
    expect(rootElement.textContent).toContain('API mode');
    expect(rootElement.querySelector('[title="Required source for this chat\'s AI responses."]')).not.toBeNull();
    expect(rootElement.querySelector('meter')?.max).toBe(180000);
    expect(rootElement.querySelector('meter')?.value).toBe(1234);
    const end = [...rootElement.querySelectorAll('button')].find(button => button.textContent === 'End Session')!;
    expect(end.disabled).toBe(false);
    await render({ ...conversation, atomic_chat_state: { ...conversation.atomic_chat_state!, phase: 'closing' } });
    expect(rootElement.textContent).toContain('Retry close-out');
    await render({ ...conversation, atomic_chat_state: { ...conversation.atomic_chat_state!, phase: 'closed' } });
    expect(rootElement.textContent).not.toContain('End Session');
    expect(rootElement.textContent).not.toContain('Retry close-out');
  } finally {
    await act(async () => root.unmount());
    useChatStore.setState(previous);
    vi.unstubAllGlobals();
  }
});
