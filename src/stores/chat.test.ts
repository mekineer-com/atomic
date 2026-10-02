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
  invoke.mockRejectedValue('TestSoul is paused. Retry in OpenAlma launcher.');
  expect(await useChatStore.getState().sendMessage('Draft')).toBe(false);
  expect(useChatStore.getState().messages).toEqual([]);
  expect(useChatStore.getState().isStreaming).toBe(false);
  expect(useChatStore.getState().error).toContain('Retry in OpenAlma launcher');
});
