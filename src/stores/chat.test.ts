import { beforeEach, expect, it, vi } from 'vitest';
import { useChatStore, type ChatMessageWithContext } from './chat';

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

it('restores input only when a successful history read proves it was not saved', async () => {
  const earlier: ChatMessageWithContext = { id: 'earlier', conversation_id: 'test-chat', role: 'user', content: 'Draft',
    created_at: '', message_index: 0, tool_calls: [], citations: [] };
  useChatStore.setState({ messages: [earlier] });
  invoke.mockImplementation((command: string) => command === 'send_chat_message'
    ? Promise.reject('Send failed')
    : Promise.resolve({ ...useChatStore.getState().currentConversation, messages: [earlier] }));
  expect(await useChatStore.getState().sendMessage('Draft')).toBe('refused');
  expect(useChatStore.getState().messages).toEqual([earlier]);
  invoke.mockRejectedValue('History unavailable');
  expect(await useChatStore.getState().sendMessage('Draft')).toBe('failed');
  invoke.mockImplementation((command: string) => command === 'send_chat_message'
    ? Promise.reject('Send failed') : Promise.resolve(null));
  expect(await useChatStore.getState().sendMessage('Draft')).toBe('failed');
});

it.each([
  ['send-failure', false], ['refresh-success', false], ['refresh-failure', false],
  ['send-failure', true], ['refresh-success', true], ['refresh-failure', true],
])('leaves a newer send untouched when A finishes late: %s, reentry=%s', async (mode, reentry) => {
  const conversationA = useChatStore.getState().currentConversation!;
  const conversationB = { ...conversationA, id: 'chat-b' };
  let readStarted!: () => void;
  const started = new Promise<void>(resolve => { readStarted = resolve; });
  let finishRead!: (value: unknown) => void;
  let failRead!: (error: Error) => void;
  const read = new Promise((resolve, reject) => { finishRead = resolve; failRead = reject; });
  let finishB!: () => void;
  let readsA = 0, sendsA = 0;
  invoke.mockImplementation((command: string, args: { conversationId: string }) => {
    if (command === 'get_conversation') {
      if (args.conversationId === conversationA.id && ++readsA === 1) { readStarted(); return read; }
      if (args.conversationId === conversationA.id) return Promise.resolve({ ...conversationA, messages: [] });
      return Promise.resolve({ ...conversationB, messages: [] });
    }
    if (args.conversationId === conversationA.id && ++sendsA === 1) {
      return mode === 'send-failure' ? Promise.reject(new Error('A failed')) : Promise.resolve({});
    }
    return new Promise<void>(resolve => { finishB = resolve; });
  });
  const sendA = useChatStore.getState().sendMessage('Draft A');
  await started;
  if (reentry) useChatStore.getState().completeMessage({
    id: 'old-answer', conversation_id: conversationA.id, role: 'assistant', content: 'Old answer',
    created_at: '', message_index: 1, tool_calls: [], citations: [],
  });
  await useChatStore.getState().openConversation(conversationB.id);
  if (reentry) await useChatStore.getState().openConversation(conversationA.id);
  const sendB = useChatStore.getState().sendMessage('Draft B');
  useChatStore.getState().appendStreamContent('B is speaking');
  const before = useChatStore.getState();
  if (mode === 'refresh-failure') failRead(new Error('A history failed'));
  else finishRead({ ...conversationA, messages: [] });
  await sendA;
  const after = useChatStore.getState();
  expect(after.currentConversation).toEqual(before.currentConversation);
  expect(after.messages).toEqual(before.messages);
  expect(after.messages[after.messages.length - 1]?.content).toBe('Draft B');
  expect(after.isStreaming).toBe(true);
  expect(after.streamingContent).toBe('B is speaking');
  expect(after.error).toBeNull();
  expect(after.isLoading).toBe(false);
  finishB();
  await sendB;
});
