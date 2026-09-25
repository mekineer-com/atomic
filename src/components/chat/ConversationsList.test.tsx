import { act } from 'react';
import { createRoot } from 'react-dom/client';
import { afterEach, expect, it, vi } from 'vitest';

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const chat = vi.hoisted(() => ({
  conversations: [],
  isLoading: false,
  error: null,
  listFilterTagId: null,
  createConversation: vi.fn(),
  openConversation: vi.fn(),
  deleteConversation: vi.fn(),
}));

vi.mock('../../stores/chat', () => ({
  useChatStore: (selector: (state: typeof chat) => unknown) => selector(chat),
}));
vi.mock('./ConversationCard', () => ({ ConversationCard: () => null }));
vi.mock('../ui/Modal', () => ({ Modal: () => null }));

import { ConversationsList } from './ConversationsList';

afterEach(() => chat.createConversation.mockReset());

it('shows creation progress and ignores repeated clicks', async () => {
  let finish!: () => void;
  chat.createConversation.mockImplementation(() => new Promise<void>((resolve) => { finish = resolve; }));
  const container = document.createElement('div');
  const root = createRoot(container);

  await act(async () => { root.render(<ConversationsList />); });
  const button = container.querySelector('button')!;
  await act(async () => {
    button.click();
    button.click();
    button.click();
  });

  expect(chat.createConversation).toHaveBeenCalledTimes(1);
  expect(button.textContent).toContain('Creating...');
  expect(button.disabled).toBe(true);

  await act(async () => { finish(); });
  await act(async () => { root.unmount(); });
});
