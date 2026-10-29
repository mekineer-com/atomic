import { expect, it, vi } from 'vitest';
import { act, createElement } from 'react';
import { createRoot } from 'react-dom/client';
import { useChatStore } from '../../stores/chat';
import { ChatView, chatDisplayMessages } from './ChatView';

const { input } = vi.hoisted(() => ({ input: { props: null as null | {
  value: string; disabled: boolean; onChange: (text: string) => void; onSend: () => Promise<void>;
} } }));
vi.mock('../../hooks/useChatEvents', () => ({ useChatEvents: () => {} }));
vi.mock('./ChatHeader', () => ({ ChatHeader: () => null }));
vi.mock('./ChatInput', () => ({ ChatInput: (props: NonNullable<typeof input.props>) => {
  input.props = props;
  return null;
} }));

it('shows only recap text, never its End instruction, after reopening', () => {
  const message = (id: string, role: 'user' | 'assistant', content: string) => ({
    id, role, content, conversation_id: 'test-chat', created_at: '', message_index: 0, tool_calls: [], citations: [],
  });
  const conversation = { id: 'test-chat', title: null, created_at: '', updated_at: '', is_archived: false,
    atomic_chat_state: { backend: 'api' as const, phase: 'closed' as const, active: null,
      recap: { request_message_id: 'control', outcome: { status: 'complete' as const, reply_message_id: 'summary' } } } };
  const messages = [message('question', 'user', 'Ordinary dialogue'),
    message('control', 'user', 'Hidden End Session instruction'), message('summary', 'assistant', 'Visible recap')];
  const shown = chatDisplayMessages(conversation, messages);
  expect(shown.map(row => row.message.content)).toEqual(['Ordinary dialogue', 'Visible recap']);
  expect(shown[1].isRecap).toBe(true);
  const legacy = [message('quoted', 'user', 'This Atomic session is ending. Write a recap of your activity is a quote'),
    message('legacy-control', 'user', 'This Atomic session is ending. Write a recap of your activity\nFull Atomic transcript:\nHidden transcript'),
    message('legacy-summary', 'assistant', 'Older recap')];
  expect(chatDisplayMessages({ ...conversation, atomic_chat_state: null }, legacy).map(row => [row.message.id, row.isRecap]))
    .toEqual([['quoted', false], ['legacy-summary', true]]);
});

it('renders a reopened saved recap as a summary in a read-only sitting', async () => {
  const previous = useChatStore.getState();
  const container = document.createElement('div');
  const root = createRoot(container);
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true);
  const scroll = vi.spyOn(HTMLElement.prototype, 'scrollIntoView').mockImplementation(() => {});
  input.props = null;
  useChatStore.setState({ currentConversation: {
    id: 'reopened-chat', title: 'Test', created_at: '', updated_at: '', is_archived: false,
    tags: [], message_count: 2, last_message_preview: 'Saved recap',
    atomic_chat_state: { backend: 'api', phase: 'closed', active: {
      user_message_id: 'old-user', status: 'failed', error: 'Historical turn failure',
    },
      recap: { request_message_id: 'control', outcome: { status: 'complete', reply_message_id: 'summary' } } },
  }, isStreaming: false, error: null, messages: [
    { id: 'control', role: 'user', content: 'End Session instruction must stay hidden', conversation_id: 'reopened-chat',
      created_at: '', message_index: 0, tool_calls: [], citations: [] },
    { id: 'summary', role: 'assistant', content: 'Saved recap text', conversation_id: 'reopened-chat',
      created_at: '', message_index: 1, tool_calls: [], citations: [] },
  ] });
  try {
    await act(async () => root.render(createElement(ChatView)));
    expect(container.textContent).toContain('Session recap');
    expect(container.textContent).toContain('Saved recap text');
    expect(container.textContent).not.toContain('End Session instruction');
    expect(container.textContent).not.toContain('Historical turn failure');
    expect(container.textContent).toContain('Start new sitting');
    expect(input.props).toBeNull();
    const current = useChatStore.getState().currentConversation!;
    await act(async () => useChatStore.setState({ currentConversation: { ...current,
      atomic_chat_state: { ...current.atomic_chat_state!, backend: 'claude_code', maintenance: {
        attempt_id: 'purge', kind: 'purge', status: 'failed', error: 'Native files could not be removed',
        ordinary_disabled: false, awaiting_measurement: false, final_used: false,
      } } } }));
    expect(container.textContent).toContain('Native files could not be removed');
    expect(container.textContent).toContain('Saved recap text');
    expect(input.props).toBeNull();
    await act(async () => useChatStore.setState({ currentConversation: { ...current,
      atomic_chat_state: { ...current.atomic_chat_state!, phase: 'closing' } }, isEndingSession: false }));
    expect(container.textContent).toContain('Use Retry close-out to finish saving');
    await act(async () => useChatStore.setState({ currentConversation: { ...current,
      atomic_chat_state: { backend: 'api', phase: 'open', active: { user_message_id: 'missing-turn',
        status: 'uncertain', error: 'Unclear outcome' } } }, messages: [] }));
    expect(container.textContent).toContain('Use End Session to close out');
    expect(container.textContent).not.toContain('Retry reply');
    await act(async () => useChatStore.setState({ currentConversation: { ...current,
      atomic_chat_state: { backend: 'api', phase: 'open', active: { user_message_id: 'missing-turn',
        status: 'running', error: null } } }, isStreaming: true }));
    expect(container.textContent).not.toContain('Retry reply');
    await act(async () => useChatStore.setState({ isStreaming: false }));
    expect(container.textContent).toContain('Retry reply');
    await act(async () => useChatStore.setState({ currentConversation: { ...current, user_id: 'TestOwner', soul_id: 'TestSoul', atomic_chat_state: null },
      messages: [{ id: 'legacy-user', conversation_id: current.id, role: 'user', content: 'Legacy unanswered input',
        created_at: '', message_index: 0, tool_calls: [], citations: [] }] }));
    expect(container.textContent).toContain('Retry reply');
    expect(input.props!.disabled).toBe(true);
  } finally {
    await act(async () => root.unmount());
    useChatStore.setState(previous);
    scroll.mockRestore();
    vi.unstubAllGlobals();
  }
});

it('keeps a rejected draft and never clears newer typing after success', async () => {
  const previous = useChatStore.getState();
  const container = document.createElement('div');
  const root = createRoot(container);
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true);
  const scroll = vi.spyOn(HTMLElement.prototype, 'scrollIntoView').mockImplementation(() => {});
  const send = vi.fn().mockResolvedValue('refused');
  useChatStore.setState({ currentConversation: {
    id: 'test-chat', title: 'Test', created_at: '', updated_at: '', is_archived: false,
    tags: [], message_count: 0, last_message_preview: null,
  }, messages: [], sendMessage: send });
  try {
    await act(async () => root.render(createElement(ChatView)));
    await act(async () => input.props!.onChange('Original draft'));
    await act(async () => input.props!.onSend());
    expect(input.props!.value).toBe('Original draft');
    let finish!: (success: string) => void;
    send.mockImplementationOnce(() => new Promise<string>(resolve => { finish = resolve; }));
    let pending!: Promise<void>;
    await act(async () => { pending = input.props!.onSend(); });
    expect(input.props!.value).toBe('');
    expect(input.props!.disabled).toBe(true);
    await act(async () => input.props!.onChange('Newer draft'));
    await act(async () => { finish('sent'); await pending; });
    expect(input.props!.value).toBe('Newer draft');
    send.mockResolvedValueOnce('sent');
    await act(async () => input.props!.onSend());
    expect(input.props!.value).toBe('');
    await act(async () => input.props!.onChange('Draft before navigation'));
    send.mockImplementationOnce(() => new Promise<string>(resolve => { finish = resolve; }));
    await act(async () => { pending = input.props!.onSend(); });
    await act(async () => useChatStore.setState({ currentConversation: {
      ...useChatStore.getState().currentConversation!, id: 'different-chat',
    } }));
    await act(async () => { finish('refused'); await pending; });
    expect(useChatStore.getState().drafts['test-chat']).toBe('Draft before navigation');
    expect(input.props!.value).toBe('');
  } finally {
    await act(async () => root.unmount());
    useChatStore.setState(previous);
    scroll.mockRestore();
    vi.unstubAllGlobals();
  }
});
