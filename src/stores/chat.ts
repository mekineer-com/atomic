import { create } from 'zustand';
import { getTransport } from '../lib/transport';
import { CANVAS_NONE_KEY, useUIStore } from './ui';
import { useCanvasStore } from './canvas';

// ==================== Types ====================

export interface Tag {
  id: string;
  name: string;
  parent_id: string | null;
  created_at: string;
}

export interface Conversation {
  id: string;
  title: string | null;
  created_at: string;
  updated_at: string;
  is_archived: boolean;
  user_id?: string | null;
  soul_id?: string | null;
  atomic_chat_state?: AtomicChatState | null;
}

export interface AtomicChatState {
  backend: 'api' | 'claude_code';
  phase: 'open' | 'closing' | 'closed';
  context_window_tokens?: number | null;
  active: { user_message_id: string; status: 'running' | 'failed' | 'uncertain'; error: string | null } | null;
  recap?: { request_message_id: string; outcome:
    | { status: 'running' }
    | { status: 'failed' | 'uncertain'; error: string }
    | { status: 'complete'; reply_message_id: string }
  } | null;
}

export function chatRetryMessageId(conversation: Conversation, messages: ChatMessageWithContext[]): string | null {
  const state = conversation.atomic_chat_state;
  if (state ? state.phase !== 'open' : !(conversation.user_id && conversation.soul_id)) return null;
  const active = state?.active;
  if (active?.status === 'uncertain') return null;
  const user = active ? messages.find(message => message.id === active.user_message_id)
    : [...messages].reverse().find(message => message.role === 'user' || message.role === 'assistant');
  if (!active && (user?.role !== 'user' || user.id.startsWith('temp-')
    || user.content.trim().startsWith('This Atomic session is ending. Write a recap of your activity'))) return null;
  if (user?.tool_calls.some(call => call.status === 'running'
    || (typeof call.tool_output === 'object' && call.tool_output !== null
      && 'outcome_unknown' in call.tool_output && call.tool_output.outcome_unknown === true))) return null;
  return active?.user_message_id ?? user?.id ?? null;
}

function chatIsRunning(state?: AtomicChatState | null): boolean {
  return state?.active?.status === 'running' || state?.recap?.outcome.status === 'running';
}

export interface ConversationWithTags extends Conversation {
  tags: Tag[];
  message_count: number;
  last_message_preview: string | null;
}

export interface ChatMessage {
  id: string;
  conversation_id: string;
  role: 'user' | 'assistant' | 'system' | 'tool';
  content: string;
  created_at: string;
  message_index: number;
}

export interface ChatToolCall {
  id: string;
  message_id: string;
  tool_name: string;
  tool_input: unknown;
  tool_output: unknown | null;
  status: 'pending' | 'running' | 'complete' | 'failed';
  created_at: string;
  completed_at: string | null;
}

export interface ChatCitation {
  id: string;
  message_id: string;
  citation_index: number;
  atom_id: string;
  chunk_index: number | null;
  excerpt: string;
  relevance_score: number | null;
}

export interface ChatMessageWithContext extends ChatMessage {
  tool_calls: ChatToolCall[];
  citations: ChatCitation[];
}

export interface ConversationWithMessages extends Conversation {
  tags: Tag[];
  messages: ChatMessageWithContext[];
}

function conversationSummary(saved: ConversationWithMessages): ConversationWithTags {
  return { ...saved, message_count: saved.messages.length,
    last_message_preview: saved.messages[saved.messages.length - 1]?.content.slice(0, 100) ?? null };
}

interface PageContext {
  view: string;
  atom_id?: string | null;
  atom_title?: string | null;
  atom_snippet?: string | null;
  wiki_tag_id?: string | null;
  wiki_tag_name?: string | null;
  selected_tag_id?: string | null;
}

function buildPageContext(): PageContext {
  const ui = useUIStore.getState();

  if (ui.readerState.atomId) {
    return {
      view: ui.readerState.editing ? 'atom_editor' : 'atom_reader',
      atom_id: ui.readerState.atomId,
      selected_tag_id: ui.selectedTagId,
    };
  }

  if (ui.wikiReaderState.tagId) {
    return {
      view: 'wiki_reader',
      wiki_tag_id: ui.wikiReaderState.tagId,
      wiki_tag_name: ui.wikiReaderState.tagName,
      selected_tag_id: ui.selectedTagId,
    };
  }

  if (ui.localGraph.isOpen && ui.localGraph.centerAtomId) {
    return {
      view: 'atom_graph',
      atom_id: ui.localGraph.centerAtomId,
      selected_tag_id: ui.selectedTagId,
    };
  }

  return {
    view: ui.viewMode,
    selected_tag_id: ui.selectedTagId,
  };
}

// ==================== Store ====================

type ChatView = 'list' | 'conversation';
let refreshVersion = 0;

interface ChatStore {
  // View state
  view: ChatView;

  // Current conversation (when view === 'conversation')
  currentConversation: ConversationWithTags | null;
  messages: ChatMessageWithContext[];
  lastSend: ChatMessageWithContext | null;
  drafts: Record<string, string>;

  // Conversations list
  conversations: ConversationWithTags[];
  listFilterTagId: string | null;

  // Streaming state
  isLoading: boolean;
  isStreaming: boolean;
  isEndingSession: boolean;
  streamingContent: string;
  streamingMessageId: string | null;
  /**
   * Tool calls fired during the current streaming turn. Populated from
   * chat-tool-start / chat-tool-complete events so the UI can render
   * collapsible cards that persist through streaming and get superseded
   * (not discarded) when the final ChatComplete message arrives.
   */
  streamingToolCalls: ChatToolCall[];

  // Error state
  error: string | null;

  // Actions - Navigation
  showList: (filterTagId?: string) => void;
  openConversation: (id: string, send?: ChatMessageWithContext) => Promise<void>;
  openOrCreateForTag: (tagId: string) => Promise<void>;
  goBack: () => void;

  // Actions - CRUD
  fetchConversations: (tagId?: string) => Promise<void>;
  createConversation: (tagIds?: string[]) => Promise<ConversationWithTags>;
  deleteConversation: (id: string) => Promise<void>;
  updateConversationTitle: (id: string, title: string) => Promise<void>;

  // Actions - Scope Management
  setScope: (tagIds: string[]) => Promise<void>;
  addTagToScope: (tagId: string) => Promise<void>;
  removeTagFromScope: (tagId: string) => Promise<void>;

  // Actions - Messaging (placeholder for now)
  sendMessage: (content: string) => Promise<'sent' | 'refused' | 'failed'>;
  setDraft: (conversationId: string, content: string) => void;
  refreshConversation: () => Promise<boolean>;
  retryMessage: (userMessageId: string) => Promise<void>;
  startNewSitting: () => Promise<void>;
  endMemuSession: () => Promise<void>;

  // Actions - Streaming updates (called from event handlers)
  appendStreamContent: (delta: string) => void;
  startStreamingToolCall: (call: { tool_call_id: string; tool_name: string; tool_input: unknown }) => void;
  completeStreamingToolCall: (update: { tool_call_id: string; results_count: number; is_error: boolean }) => void;
  completeMessage: (message: ChatMessageWithContext) => void;
  setStreamingError: (error: string) => void;

  // Actions - Utilities
  clearError: () => void;
  reset: () => void;
}

export const useChatStore = create<ChatStore>((set, get) => ({
  // Initial state
  view: 'list',
  currentConversation: null,
  messages: [],
  lastSend: null,
  drafts: {},
  conversations: [],
  listFilterTagId: null,
  isLoading: false,
  isStreaming: false,
  isEndingSession: false,
  streamingContent: '',
  streamingMessageId: null,
  streamingToolCalls: [],
  error: null,

  // Navigation
  showList: (filterTagId?: string) => {
    set({
      view: 'list',
      listFilterTagId: filterTagId ?? null,
      currentConversation: null,
      messages: [],
    });
    useUIStore.getState().setChatSidebarConversationId(null);
    get().fetchConversations(filterTagId);
  },

  openConversation: async (id: string, send?: ChatMessageWithContext) => {
    const stale = () => send && (get().currentConversation?.id !== id || get().lastSend !== send);
    if (stale()) return;
    set({ isLoading: true, error: null });
    try {
      const result = await getTransport().invoke<ConversationWithMessages | null>('get_conversation', {
        conversationId: id,
      });
      if (stale()) return;

      if (result) {
        set({
          view: 'conversation',
          currentConversation: conversationSummary(result),
          messages: result.messages,
          isLoading: false,
          ...(get().currentConversation?.id !== id || (result.atomic_chat_state && !chatIsRunning(result.atomic_chat_state))
            ? { isStreaming: false, streamingContent: '', streamingToolCalls: [] } : {}),
        });
        useUIStore.getState().setChatSidebarConversationId(id);
      } else {
        // Conversation went missing (deleted, different DB, stale persisted
        // id from a prior session) — fall back to the list rather than
        // leaving the user staring at an error with no recovery path.
        set({ isLoading: false, error: null });
        get().showList();
      }
    } catch (e) {
      if (stale()) return;
      set({ error: String(e), isLoading: false });
    }
  },

  openOrCreateForTag: async (tagId: string) => {
    set({ isLoading: true, error: null, listFilterTagId: tagId });
    try {
      // Fetch conversations that contain this tag
      const conversations = await getTransport().invoke<ConversationWithTags[]>('get_conversations', {
        filterTagId: tagId,
        limit: 50,
        offset: 0,
      });

      // Find a conversation with exactly this one tag
      const exactMatch = conversations.find(
        (c) => c.tags.length === 1 && c.tags[0].id === tagId
      );

      if (exactMatch) {
        // Open the existing conversation
        await get().openConversation(exactMatch.id);
      } else {
        // Create a new conversation with this tag
        await get().createConversation([tagId]);
      }
    } catch (e) {
      set({ error: String(e), isLoading: false });
    }
  },

  goBack: () => {
    const { listFilterTagId } = get();
    set({
      view: 'list',
      currentConversation: null,
      messages: [],
      streamingContent: '',
      streamingToolCalls: [],
    });
    useUIStore.getState().setChatSidebarConversationId(null);
    get().fetchConversations(listFilterTagId ?? undefined);
  },

  // CRUD
  fetchConversations: async (tagId?: string) => {
    set({ isLoading: true, error: null });
    try {
      const conversations = await getTransport().invoke<ConversationWithTags[]>('get_conversations', {
        filterTagId: tagId ?? null,
        limit: 50,
        offset: 0,
      });
      set({ conversations, isLoading: false, listFilterTagId: tagId ?? null });
    } catch (e) {
      set({ error: String(e), isLoading: false });
    }
  },

  createConversation: async (tagIds?: string[]) => {
    set({ isLoading: true, error: null });
    try {
      const conversation = await getTransport().invoke<ConversationWithTags>('create_conversation', {
        tagIds: tagIds ?? [],
        title: null,
      });

      // Add to list and open it
      set((state) => ({
        conversations: [conversation, ...state.conversations],
        view: 'conversation',
        currentConversation: conversation,
        messages: [],
        isLoading: false,
      }));

      return conversation;
    } catch (e) {
      set({ error: String(e), isLoading: false });
      throw e;
    }
  },

  deleteConversation: async (id: string) => {
    try {
      await getTransport().invoke('delete_conversation', { id });
      set((state) => ({
        conversations: state.conversations.filter((c) => c.id !== id),
        // If we deleted the current conversation, go back to list
        ...(state.currentConversation?.id === id
          ? { view: 'list' as const, currentConversation: null, messages: [] }
          : {}),
      }));
    } catch (e) {
      set({ error: String(e) });
    }
  },

  updateConversationTitle: async (id: string, title: string) => {
    try {
      await getTransport().invoke('update_conversation', { id, title, isArchived: null });
      set((state) => ({
        conversations: state.conversations.map((c) =>
          c.id === id ? { ...c, title } : c
        ),
        currentConversation:
          state.currentConversation?.id === id
            ? { ...state.currentConversation, title }
            : state.currentConversation,
      }));
    } catch (e) {
      set({ error: String(e) });
    }
  },

  // Scope Management
  setScope: async (tagIds: string[]) => {
    const { currentConversation } = get();
    if (!currentConversation) return;

    try {
      const updated = await getTransport().invoke<ConversationWithTags>('set_conversation_scope', {
        conversationId: currentConversation.id,
        tagIds,
      });
      set({ currentConversation: updated });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  addTagToScope: async (tagId: string) => {
    const { currentConversation } = get();
    if (!currentConversation) return;

    try {
      const updated = await getTransport().invoke<ConversationWithTags>('add_tag_to_scope', {
        conversationId: currentConversation.id,
        tagId,
      });
      set({ currentConversation: updated });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  removeTagFromScope: async (tagId: string) => {
    const { currentConversation } = get();
    if (!currentConversation) return;

    try {
      const updated = await getTransport().invoke<ConversationWithTags>('remove_tag_from_scope', {
        conversationId: currentConversation.id,
        tagId,
      });
      set({ currentConversation: updated });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  // Messaging
  setDraft: (conversationId, content) => set(state => ({ drafts: { ...state.drafts, [conversationId]: content } })),

  refreshConversation: async () => {
    const current = get().currentConversation;
    if (!current) return false;
    const version = ++refreshVersion;
    try {
      const saved = await getTransport().invoke<ConversationWithMessages | null>('get_conversation', { conversationId: current.id });
      if (!saved || version !== refreshVersion || get().currentConversation !== current) return false;
      set({ currentConversation: conversationSummary(saved), messages: saved.messages,
        ...(saved.atomic_chat_state && !chatIsRunning(saved.atomic_chat_state) ? { isStreaming: false, streamingContent: '', streamingToolCalls: [] } : {}),
        ...(saved.atomic_chat_state?.phase === 'closed' ? { error: null } : {}),
      });
      return true;
    } catch (error) {
      if (version === refreshVersion && get().currentConversation === current) set({ error: String(error) });
      return false;
    }
  },

  retryMessage: async (userMessageId) => {
    const current = get().currentConversation;
    if (!current || (!current.atomic_chat_state && !(current.user_id && current.soul_id))) return;
    ++refreshVersion;
    set({ isStreaming: true, streamingContent: '', streamingToolCalls: [], error: null });
    try {
      await getTransport().invoke('retry_chat_message', { conversationId: current.id, userMessageId });
    } catch (error) {
      if (get().currentConversation?.id === current.id) set({ error: String(error), isStreaming: false });
    }
    if (get().currentConversation?.id === current.id) await get().refreshConversation();
  },

  startNewSitting: async () => {
    const current = get().currentConversation;
    if (!current || get().isLoading) return;
    const draft = get().drafts[current.id] ?? '';
    try {
      const next = await get().createConversation(current.tags.map(tag => tag.id));
      get().setDraft(next.id, draft);
      get().setDraft(current.id, '');
      useUIStore.getState().setChatSidebarConversationId(next.id);
    } catch {
      // createConversation already records the error for the visible chat.
    }
  },

  sendMessage: async (content: string) => {
    const { currentConversation, messages, openConversation } = get();
    if (!currentConversation) {
      set({ error: 'No conversation selected' });
      return 'failed';
    }
    ++refreshVersion;

    // Add user message optimistically
    const userMessage: ChatMessageWithContext = {
      id: `temp-user-${Date.now()}`,
      conversation_id: currentConversation.id,
      role: 'user',
      content,
      created_at: new Date().toISOString(),
      message_index: messages.length,
      tool_calls: [],
      citations: [],
    };

    set({
      messages: [...messages, userMessage],
      lastSend: userMessage,
      isStreaming: true,
      streamingContent: '',
      streamingToolCalls: [],
      error: null,
    });

    try {
      // Auto-inject canvas context when chatting from canvas view
      let canvasContext = undefined;
      if (useUIStore.getState().viewMode === 'canvas') {
        const ui = useUIStore.getState();
        const canvasData = useCanvasStore.getState().canvasData;
        if (canvasData) {
          const categoryIds = [...new Set(canvasData.atoms.flatMap((atom) => atom.tag_ids))].sort();
          const entityIds = [...new Set(canvasData.atoms.flatMap((atom) => atom.entity_ids))].sort();
          canvasContext = {
            clusters: canvasData.clusters.map((c) => ({
              label: c.label,
              atom_count: c.atom_count,
            })),
            layers: {
              categories: [CANVAS_NONE_KEY, ...categoryIds].filter((id) => ui.canvasCategoryVisible[id] ?? true),
              entities: [CANVAS_NONE_KEY, ...entityIds].filter((id) => ui.canvasEntityVisible[id] ?? true),
              filter: ui.canvasFilter,
              category_show_dimmed: ui.canvasCategoryShowDimmed,
              entity_show_dimmed: ui.canvasEntityShowDimmed,
            },
          };
        }
      }

      await getTransport().invoke<ChatMessageWithContext>('send_chat_message', {
        conversationId: currentConversation.id,
        content,
        canvasContext,
        pageContext: buildPageContext(),
      });

      // Refetch the conversation to get the properly saved messages
      // This ensures correct IDs and ordering from the database
      await openConversation(currentConversation.id, userMessage);
      return 'sent';
    } catch (e) {
      const paused = e instanceof Error && 'code' in e && e.code === 'soul_paused';
      let unsaved = paused;
      if (!paused) {
        const saved = await getTransport().invoke<ConversationWithMessages | null>('get_conversation', {
          conversationId: currentConversation.id,
        }).catch(() => undefined);
        if (saved) {
          unsaved = !saved.messages.some(m => m.role === 'user' && m.content === content
            && !messages.some(previous => previous.id === m.id));
          if (get().currentConversation?.id === currentConversation.id && get().lastSend === userMessage) {
            set({ messages: saved.messages, currentConversation: conversationSummary(saved) });
          }
        }
      }
      // Remove the temp user message on error
      set((state) => state.currentConversation?.id === currentConversation.id && state.lastSend === userMessage ? {
        messages: state.messages.filter((m) => !m.id.startsWith('temp-')),
        error: String(e),
        isStreaming: false,
        streamingContent: '',
      } : {});
      return unsaved ? 'refused' : 'failed';
    }
  },

  endMemuSession: async () => {
    const { currentConversation, fetchConversations } = get();
    if (!currentConversation) return;
    const integrated = Boolean(currentConversation.atomic_chat_state || (currentConversation.user_id && currentConversation.soul_id));
    set({ isEndingSession: true, error: null });
    try {
      await getTransport().invoke('end_memu_session', {
        conversationId: currentConversation.id,
      });
      if (get().currentConversation?.id !== currentConversation.id
        || (integrated && !await get().refreshConversation())) {
        set({ isEndingSession: false });
        return;
      }
      if (currentConversation.atomic_chat_state
        || (get().currentConversation?.id === currentConversation.id && get().currentConversation?.atomic_chat_state)) {
        set({ isEndingSession: false });
        return;
      }
      await fetchConversations(get().listFilterTagId ?? undefined);
      set({ view: 'list', currentConversation: null, messages: [], isEndingSession: false });
      useUIStore.getState().setChatSidebarConversationId(null);
    } catch (e) {
      set({ error: String(e), isEndingSession: false });
      if (get().currentConversation?.id === currentConversation.id && integrated) {
        await get().refreshConversation();
      }
    }
  },

  // Streaming updates - receives full accumulated content from backend
  appendStreamContent: (content: string) => {
    set({ streamingContent: content });
  },

  startStreamingToolCall: ({ tool_call_id, tool_name, tool_input }) => {
    const now = new Date().toISOString();
    set((state) => ({
      streamingToolCalls: [
        ...state.streamingToolCalls,
        {
          id: tool_call_id,
          message_id: '',
          tool_name,
          tool_input,
          tool_output: null,
          status: 'running',
          created_at: now,
          completed_at: null,
        },
      ],
    }));
  },

  completeStreamingToolCall: ({ tool_call_id, results_count, is_error }) => {
    const now = new Date().toISOString();
    set((state) => ({
      streamingToolCalls: state.streamingToolCalls.map((call) =>
        call.id === tool_call_id
          ? {
              ...call,
              status: is_error ? 'failed' as const : 'complete' as const,
              tool_output: { results_count },
              completed_at: now,
            }
          : call
      ),
    }));
  },

  completeMessage: (message: ChatMessageWithContext) => {
    ++refreshVersion;
    set((state) => {
      // Don't add if message already exists (prevents duplicates from event + refetch)
      const messageExists = state.messages.some((m) => m.id === message.id);
      if (messageExists) {
        return {
          isStreaming: false,
          streamingContent: '',
          streamingMessageId: null,
          streamingToolCalls: [],
        };
      }
      return {
        messages: [...state.messages, message],
        isStreaming: false,
        streamingContent: '',
        streamingMessageId: null,
        streamingToolCalls: [],
      };
    });
  },

  setStreamingError: (error: string) => {
    ++refreshVersion;
    set({
      error,
      isStreaming: false,
      streamingContent: '',
    });
  },

  // Utilities
  clearError: () => set({ error: null }),

  reset: () =>
    set({
      view: 'list',
      currentConversation: null,
      messages: [],
      lastSend: null,
      drafts: {},
      conversations: [],
      listFilterTagId: null,
      isLoading: false,
      isStreaming: false,
      isEndingSession: false,
      streamingContent: '',
      streamingMessageId: null,
      streamingToolCalls: [],
      error: null,
    }),
}));
