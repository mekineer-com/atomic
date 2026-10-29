import { beforeEach, expect, it, vi } from 'vitest';
import { chatMaintenancePending, chatRetryMessageId, useChatStore, type ChatMessageWithContext, type AtomicChatState } from './chat';
import { useUIStore } from './ui';

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

it('shows failed tool completion immediately and successful completion normally', () => {
  const store = useChatStore.getState();
  store.startStreamingToolCall({ tool_call_id: 'toolu-failed', tool_name: 'get_atom', tool_input: {} });
  store.completeStreamingToolCall({ tool_call_id: 'toolu-failed', results_count: 0, is_error: true });
  expect(useChatStore.getState().streamingToolCalls[0].status).toBe('failed');
  store.startStreamingToolCall({ tool_call_id: 'toolu-success', tool_name: 'get_atom', tool_input: {} });
  store.completeStreamingToolCall({ tool_call_id: 'toolu-success', results_count: 1, is_error: false });
  expect(useChatStore.getState().streamingToolCalls[1].status).toBe('complete');
});

it('retains drafts per conversation and carries the closed sitting draft to a new one', async () => {
  const current = useChatStore.getState().currentConversation!;
  useChatStore.getState().setDraft(current.id, 'Refused draft');
  invoke.mockResolvedValueOnce([]);
  useChatStore.getState().showList();
  await Promise.resolve();
  expect(useChatStore.getState().drafts[current.id]).toBe('Refused draft');
  useChatStore.setState({ currentConversation: current });
  invoke.mockResolvedValue({ ...current, id: 'new-chat' });
  await useChatStore.getState().startNewSitting();
  expect(useChatStore.getState().drafts['new-chat']).toBe('Refused draft');
  expect(useChatStore.getState().drafts[current.id]).toBe('');
  expect(useUIStore.getState().chatSidebarConversationId).toBe('new-chat');
  useChatStore.getState().reset();
  expect(useChatStore.getState().drafts).toEqual({});
});

it('offers running/failed recovery but not Retry for unknown tool effects', () => {
  const current = { ...useChatStore.getState().currentConversation!, atomic_chat_state: {
    backend: 'api' as const, phase: 'open' as const,
    active: { user_message_id: 'saved-user', status: 'running' as const, error: null },
  } };
  expect(chatRetryMessageId(current, [])).toBe('saved-user');
  const user: ChatMessageWithContext = { id: 'saved-user', conversation_id: current.id, role: 'user',
    content: 'An edit', created_at: '', message_index: 0, citations: [], tool_calls: [{
      id: 'unclear-edit', message_id: 'saved-user', tool_name: 'edit_atom', tool_input: {},
      tool_output: { outcome_unknown: true }, status: 'failed', created_at: '', completed_at: null,
    }] };
  expect(chatRetryMessageId(current, [user])).toBeNull();
  expect(chatRetryMessageId({ ...current, atomic_chat_state: { ...current.atomic_chat_state, phase: 'closed' } }, [])).toBeNull();
  expect(chatRetryMessageId({ ...current, atomic_chat_state: { ...current.atomic_chat_state,
    active: { ...current.atomic_chat_state.active, status: 'uncertain' } } }, [])).toBeNull();
});

it('recognizes legacy unanswered input without offering standalone or recap Retry', () => {
  const conversation = { ...useChatStore.getState().currentConversation!, user_id: 'TestOwner', soul_id: 'TestSoul' };
  const user: ChatMessageWithContext = { id: 'legacy-user', conversation_id: conversation.id, role: 'user',
    content: 'Unanswered question', created_at: '', message_index: 0, tool_calls: [], citations: [] };
  expect(chatRetryMessageId(conversation, [user])).toBe(user.id);
  expect(chatRetryMessageId({ ...conversation, user_id: null, soul_id: null }, [user])).toBeNull();
  expect(chatRetryMessageId(conversation, [user, { ...user, id: 'answer', role: 'assistant' }])).toBeNull();
  expect(chatRetryMessageId(conversation, [{ ...user,
    content: 'This Atomic session is ending. Write a recap of your activity' }])).toBeNull();
  expect(chatRetryMessageId({ ...conversation, atomic_chat_state: { backend: 'api', phase: 'open', active: null } }, [user])).toBe(user.id);
});

it('gates native Retry during unresolved maintenance without confusing it with a reply', () => {
  const state: AtomicChatState = { backend: 'claude_code', phase: 'open',
    active: { user_message_id: 'saved-user', status: 'failed', error: 'native failure' },
    maintenance: { attempt_id: 'cleanup', kind: 'ordinary', status: 'pending', error: null,
      ordinary_disabled: false, awaiting_measurement: false, final_used: false } };
  const current = { ...useChatStore.getState().currentConversation!, atomic_chat_state: state };
  for (const status of ['pending', 'running', 'prepared', 'blocked', 'failed', 'complete'] as const) {
    state.maintenance!.status = status;
    expect(chatMaintenancePending(state)).toBe(['pending', 'running', 'prepared'].includes(status));
    expect(chatRetryMessageId(current, [])).toBe(['failed', 'complete'].includes(status) ? 'saved-user' : null);
  }
});

it.each([true, false])('refreshes Closing state after End fails, initiallyBound=%s', async (initiallyBound) => {
  const state = {
    backend: 'api' as const, phase: 'open' as const, active: null,
  };
  const current = { ...useChatStore.getState().currentConversation!, user_id: 'TestOwner', soul_id: 'TestSoul',
    atomic_chat_state: initiallyBound ? state : null };
  useChatStore.setState({ currentConversation: current });
  invoke.mockImplementation((command: string) => command === 'end_memu_session'
    ? Promise.reject('Fictional recap failure')
    : Promise.resolve({ ...current, atomic_chat_state: { ...state, phase: 'closing' }, messages: [] }));
  await useChatStore.getState().endMemuSession();
  expect(useChatStore.getState().currentConversation?.atomic_chat_state?.phase).toBe('closing');
  expect(useChatStore.getState().error).toBe('Fictional recap failure');
  expect(invoke.mock.calls.map(([command]) => command)).toEqual(['end_memu_session', 'get_conversation']);
});

it('keeps a newly bound legacy sitting visible after successful End', async () => {
  const current = { ...useChatStore.getState().currentConversation!, user_id: 'TestOwner', soul_id: 'TestSoul' };
  useChatStore.setState({ currentConversation: current });
  invoke.mockResolvedValueOnce({}).mockResolvedValueOnce({ ...current, messages: [],
    atomic_chat_state: { backend: 'api', phase: 'closed', active: null } });
  await useChatStore.getState().endMemuSession();
  expect(useChatStore.getState().currentConversation?.id).toBe(current.id);
  expect(useChatStore.getState().currentConversation?.atomic_chat_state?.phase).toBe('closed');
  expect(invoke.mock.calls.map(([command]) => command)).toEqual(['end_memu_session', 'get_conversation']);
});

it('does not infer stateless End navigation from an unavailable refresh', async () => {
  const current = { ...useChatStore.getState().currentConversation!, user_id: 'TestOwner', soul_id: 'TestSoul' };
  useChatStore.setState({ currentConversation: current });
  invoke.mockResolvedValueOnce({}).mockRejectedValueOnce('History read unavailable');
  await useChatStore.getState().endMemuSession();
  expect(useChatStore.getState().currentConversation?.id).toBe(current.id);
  expect(useChatStore.getState().error).toBe('History read unavailable');
  expect(invoke.mock.calls.map(([command]) => command)).toEqual(['end_memu_session', 'get_conversation']);
});

it('keeps the carried draft consumed and handles a failed new-sitting creation', async () => {
  const current = useChatStore.getState().currentConversation!;
  useChatStore.getState().setDraft(current.id, 'Carry once');
  invoke.mockResolvedValueOnce({ ...current, id: 'next-chat' });
  await useChatStore.getState().startNewSitting();
  useChatStore.setState({ currentConversation: current });
  invoke.mockResolvedValueOnce({ ...current, id: 'another-chat' });
  await useChatStore.getState().startNewSitting();
  expect(useChatStore.getState().drafts['another-chat']).toBe('');
  invoke.mockRejectedValueOnce('Fictional create failure');
  await expect(useChatStore.getState().startNewSitting()).resolves.toBeUndefined();
  expect(useChatStore.getState().error).toBe('Fictional create failure');
});

it('does not create two sittings when Start is pressed twice before creation finishes', async () => {
  const current = useChatStore.getState().currentConversation!;
  let finish!: (value: unknown) => void;
  invoke.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
  const first = useChatStore.getState().startNewSitting();
  await useChatStore.getState().startNewSitting();
  expect(invoke).toHaveBeenCalledTimes(1);
  finish({ ...current, id: 'next-chat' });
  await first;
});

it('refreshes durable state without discarding drafts or switching to a late conversation', async () => {
  const current = useChatStore.getState().currentConversation!;
  useChatStore.getState().setDraft(current.id, 'Newer typing');
  useChatStore.setState({ error: 'Earlier live failure' });
  const saved = { ...current, atomic_chat_state: { backend: 'api', phase: 'closed', active: null }, messages: [] };
  invoke.mockResolvedValue(saved);
  await useChatStore.getState().refreshConversation();
  expect(useChatStore.getState().currentConversation?.atomic_chat_state?.phase).toBe('closed');
  expect(useChatStore.getState().error).toBeNull();
  expect(useChatStore.getState().drafts[current.id]).toBe('Newer typing');
  let finish!: (value: unknown) => void;
  invoke.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
  const pending = useChatStore.getState().refreshConversation();
  useChatStore.setState({ currentConversation: { ...current, id: 'different-chat' } });
  finish(saved);
  await pending;
  expect(useChatStore.getState().currentConversation?.id).toBe('different-chat');
});

it('refreshes after Retry without sending another user input', async () => {
  const current = useChatStore.getState().currentConversation!;
  useChatStore.setState({ currentConversation: { ...current, atomic_chat_state: {
    backend: 'api', phase: 'open', active: { user_message_id: 'saved-user', status: 'failed', error: 'Test failure' },
  } } });
  invoke.mockResolvedValue({ ...current, atomic_chat_state: { backend: 'api', phase: 'open', active: null }, messages: [] });
  await useChatStore.getState().retryMessage('saved-user');
  expect(invoke.mock.calls.map(([command]) => command)).toEqual(['retry_chat_message', 'get_conversation']);
  expect(invoke.mock.calls[0][1]).toEqual({ conversationId: current.id, userMessageId: 'saved-user' });
  expect(useChatStore.getState().isStreaming).toBe(false);
});

it('hydrates the server meter when a managed sitting is created', async () => {
  const created = { ...useChatStore.getState().currentConversation!, id: 'new-managed-chat',
    atomic_chat_state: { backend: 'api' as const, phase: 'open' as const, active: null } };
  invoke.mockResolvedValueOnce(created).mockResolvedValueOnce({ ...created, messages: [],
    context_meter: { estimated_tokens: 1234, limit_tokens: 180000, usable_tokens: 200000 } });
  await useChatStore.getState().createConversation();
  await Promise.resolve();
  expect(useChatStore.getState().currentConversation?.context_meter?.estimated_tokens).toBe(1234);
  expect(invoke.mock.calls.map(([command]) => command)).toEqual(['create_conversation', 'get_conversation']);
});

it('does not let a successful Send reload overwrite a newer durable state read', async () => {
  const current = useChatStore.getState().currentConversation!;
  const send: ChatMessageWithContext = { id: 'temp-send', conversation_id: current.id, role: 'user', content: 'Test',
    created_at: '', message_index: 0, tool_calls: [], citations: [] };
  useChatStore.setState({ lastSend: send });
  let finish!: (value: unknown) => void;
  invoke.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
  const pending = useChatStore.getState().openConversation(current.id, send);
  invoke.mockResolvedValueOnce({ ...current, messages: [], atomic_chat_state: { backend: 'api', phase: 'closed', active: null } });
  await useChatStore.getState().refreshConversation();
  finish({ ...current, messages: [] });
  await pending;
  expect(useChatStore.getState().currentConversation?.atomic_chat_state?.phase).toBe('closed');
  expect(useChatStore.getState().isLoading).toBe(false);
});

it.each(['completion', 'retry'])('ignores a pre-%s refresh after newer work begins', async (transition) => {
  const current = { ...useChatStore.getState().currentConversation!, atomic_chat_state: {
    backend: 'api' as const, phase: 'open' as const,
    active: { user_message_id: 'saved-user', status: 'failed' as const, error: 'Earlier failure' },
  } };
  useChatStore.setState({ currentConversation: current });
  let finish!: (value: unknown) => void;
  invoke.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
  const pending = useChatStore.getState().refreshConversation();
  let retry: Promise<void> | undefined;
  let finishRetry!: () => void;
  if (transition === 'completion') {
    useChatStore.getState().completeMessage({ id: 'new-answer', conversation_id: current.id, role: 'assistant',
      content: 'New answer', created_at: '', message_index: 1, tool_calls: [], citations: [] });
  } else {
    invoke.mockImplementationOnce(() => new Promise<void>(resolve => { finishRetry = resolve; }));
    retry = useChatStore.getState().retryMessage('saved-user');
  }
  finish({ ...current, messages: [] });
  await pending;
  if (transition === 'completion') expect(useChatStore.getState().messages[0].id).toBe('new-answer');
  else {
    expect(useChatStore.getState().isStreaming).toBe(true);
    invoke.mockResolvedValueOnce({ ...current, atomic_chat_state: { ...current.atomic_chat_state, active: null }, messages: [] });
    finishRetry();
    await retry;
  }
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
