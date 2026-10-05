import { expect, it, vi } from 'vitest';
import { act, createElement } from 'react';
import { createRoot } from 'react-dom/client';
import { useChatStore } from '../../stores/chat';
import { ChatView } from './ChatView';

const { input } = vi.hoisted(() => ({ input: { props: null as null | {
  value: string; disabled: boolean; onChange: (text: string) => void; onSend: () => Promise<void>;
} } }));
vi.mock('../../hooks/useChatEvents', () => ({ useChatEvents: () => {} }));
vi.mock('./ChatHeader', () => ({ ChatHeader: () => null }));
vi.mock('./ChatInput', () => ({ ChatInput: (props: NonNullable<typeof input.props>) => {
  input.props = props;
  return null;
} }));

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
  } finally {
    await act(async () => root.unmount());
    useChatStore.setState(previous);
    scroll.mockRestore();
    vi.unstubAllGlobals();
  }
});
