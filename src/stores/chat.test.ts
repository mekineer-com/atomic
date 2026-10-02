import { beforeEach, expect, it, vi } from 'vitest';
import { useChatStore } from './chat';

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('../lib/transport', () => ({ getTransport: () => ({ invoke }) }));

beforeEach(() => {
  invoke.mockReset();
  useChatStore.getState().reset();
  useChatStore.setState({ currentConversation: {
    id: 'test-chat', title: 'Test', created_at: '', updated_at: '', is_archived: false,
    tags: [], message_count: 0, last_message_preview: null,
  } });
});

it('reports a refused send without leaving synthetic chat history', async () => {
  invoke.mockRejectedValue(Object.assign(new Error('TestSoul is paused. Retry in OpenAlma launcher.'), { code: 'soul_paused' }));
  expect(await useChatStore.getState().sendMessage('Draft')).toBe('refused');
  expect(useChatStore.getState().messages).toEqual([]);
  expect(useChatStore.getState().isStreaming).toBe(false);
  expect(useChatStore.getState().error).toContain('Retry in OpenAlma launcher');
});

it('reconciles saved user input after a provider failure without restoring it as a draft', async () => {
  const saved = { id: 'saved-user', conversation_id: 'test-chat', role: 'user', content: 'Draft',
    created_at: '', message_index: 0, tool_calls: [], citations: [] };
  invoke.mockImplementation((command: string) => command === 'send_chat_message'
    ? Promise.reject('Provider failed')
    : Promise.resolve({ ...useChatStore.getState().currentConversation, messages: [saved] }));
  expect(await useChatStore.getState().sendMessage('Draft')).toBe('failed');
  expect(useChatStore.getState().messages).toEqual([saved]);
  expect(useChatStore.getState().error).toBe('Provider failed');
});
