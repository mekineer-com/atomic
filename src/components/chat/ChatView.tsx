import { useState, useRef, useEffect, useCallback, useMemo } from 'react';
import { MessageSquare } from 'lucide-react';
import { chatMaintenancePending, chatRetryMessageId, useChatStore, type Conversation, type ChatMessageWithContext } from '../../stores/chat';
import { useUIStore, type TabEntry } from '../../stores/ui';
import { useChatEvents } from '../../hooks/useChatEvents';
import { useContentSearch } from '../../hooks';
import { ChatHeader } from './ChatHeader';
import { ChatMessage } from './ChatMessage';
import { ChatInput } from './ChatInput';
import { SearchBar } from '../ui/SearchBar';

export function shouldOpenChatSearch(chatSidebarOpen: boolean, entry?: TabEntry): boolean {
  return chatSidebarOpen && entry?.type !== 'tool'
    && entry?.type !== 'wiki'
    && !((entry?.type === 'atom' || entry?.type === 'graph')
      && /^(memory|category|entity):/.test(entry.atomId));
}

export function chatDisplayMessages(conversation: Conversation, messages: ChatMessageWithContext[]) {
  const recap = conversation.atomic_chat_state?.recap;
  const replyId = recap?.outcome.status === 'complete' ? recap.outcome.reply_message_id : null;
  let legacyRecap = false;
  return messages.flatMap(message => {
    if (message.id === recap?.request_message_id
      || (message.id !== conversation.atomic_chat_state?.active?.user_message_id && message.role === 'user'
        && message.content.trim().startsWith('This Atomic session is ending. Write a recap of your activity')
        && message.content.split('\n').includes('Full Atomic transcript:'))) {
      legacyRecap = true;
      return [];
    }
    const isRecap = message.id === replyId || (legacyRecap && message.role === 'assistant');
    legacyRecap = false;
    return [{ message, isRecap }];
  });
}

export function ChatView() {
  const currentConversation = useChatStore(s => s.currentConversation);
  const messages = useChatStore(s => s.messages);
  const isLoading = useChatStore(s => s.isLoading);
  const isStreaming = useChatStore(s => s.isStreaming);
  const isEndingSession = useChatStore(s => s.isEndingSession);
  const streamingContent = useChatStore(s => s.streamingContent);
  const streamingToolCalls = useChatStore(s => s.streamingToolCalls);
  const error = useChatStore(s => s.error);
  const sendMessage = useChatStore(s => s.sendMessage);
  const goBack = useChatStore(s => s.goBack);
  const retryMessage = useChatStore(s => s.retryMessage);
  const startNewSitting = useChatStore(s => s.startNewSitting);
  const setDraft = useChatStore(s => s.setDraft);
  const inputValue = useChatStore(s => s.currentConversation ? s.drafts[s.currentConversation.id] ?? '' : '');
  const setInputValue = (value: string) => {
    if (currentConversation) setDraft(currentConversation.id, value);
  };
  const lifecycle = currentConversation?.atomic_chat_state;
  const integrated = Boolean(lifecycle || (currentConversation?.user_id && currentConversation?.soul_id));
  const retryMessageId = currentConversation ? chatRetryMessageId(currentConversation, messages) : null;
  const turnError = lifecycle?.phase === 'closed' ? null : lifecycle?.active?.error;
  const displayMessages = useMemo(() => currentConversation ? chatDisplayMessages(currentConversation, messages) : [], [currentConversation, messages]);

  const openReader = useUIStore(s => s.openReader);
  const chatSearchOwnsFind = useUIStore((s) => {
    const tab = s.activeTabId ? s.tabs.find(candidate => candidate.id === s.activeTabId) : null;
    const entry = tab?.stack[tab.stackIndex];
    return shouldOpenChatSearch(s.chatSidebarOpen, entry);
  });

  const [isSending, setIsSending] = useState(false);
  const inputBlocked = isStreaming || isSending || chatMaintenancePending(lifecycle)
    || lifecycle?.maintenance?.status === 'blocked'
    || Boolean(lifecycle && (lifecycle.phase !== 'open' || lifecycle.active)) || Boolean(integrated && retryMessageId);
  const messagesEndRef = useRef<HTMLDivElement>(null);
  const messagesContainerRef = useRef<HTMLDivElement>(null);
  const isNearBottomRef = useRef(true);

  // Combine all message content for search
  const allContent = useMemo(() => {
    return displayMessages.map(({ message }) => message.content).join('\n\n');
  }, [displayMessages]);

  // Content search across all messages
  const {
    isOpen: isSearchOpen,
    query: searchQuery,
    searchedQuery,
    currentIndex,
    totalMatches,
    setQuery: setSearchQuery,
    openSearch,
    closeSearch,
    goToNext,
    goToPrevious,
    highlightText,
  } = useContentSearch(allContent);

  // Keyboard handler for Ctrl+F / Cmd+F
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (chatSearchOwnsFind && (e.ctrlKey || e.metaKey) && e.key === 'f') {
        e.preventDefault();
        openSearch();
      }
    };
    document.addEventListener('keydown', handleKeyDown);
    return () => document.removeEventListener('keydown', handleKeyDown);
  }, [chatSearchOwnsFind, openSearch]);

  // Subscribe to chat events for streaming
  useChatEvents(currentConversation?.id ?? null);

  // Check if user is near the bottom of the scroll container
  const checkIfNearBottom = useCallback(() => {
    const container = messagesContainerRef.current;
    if (!container) return true;

    const threshold = 100; // pixels from bottom to consider "at bottom"
    const { scrollTop, scrollHeight, clientHeight } = container;
    return scrollHeight - scrollTop - clientHeight < threshold;
  }, []);

  // Update near-bottom state on scroll
  const handleScroll = useCallback(() => {
    isNearBottomRef.current = checkIfNearBottom();
  }, [checkIfNearBottom]);

  // Auto-scroll to bottom only if user is already near bottom
  useEffect(() => {
    if (isNearBottomRef.current) {
      messagesEndRef.current?.scrollIntoView({ behavior: 'smooth' });
    }
  }, [messages, streamingContent]);

  // Always scroll to bottom when a new message is sent (user action)
  const scrollToBottom = useCallback(() => {
    messagesEndRef.current?.scrollIntoView({ behavior: 'smooth' });
    isNearBottomRef.current = true;
  }, []);

  const handleSend = async () => {
    if (!inputValue.trim() || inputBlocked) return;

    const content = inputValue.trim();
    setInputValue('');
    setIsSending(true);
    scrollToBottom(); // Scroll to bottom when user sends a message
    try {
      if (await sendMessage(content) === 'refused' && currentConversation) {
        const drafts = useChatStore.getState().drafts;
        if (drafts[currentConversation.id] === '') {
          setDraft(currentConversation.id, inputValue);
        }
      }
    } finally {
      setIsSending(false);
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      handleSend();
    }
  };

  // Handle viewing an atom from citation - switch drawer to viewer mode
  const handleViewAtom = useCallback((atomId: string, highlightText?: string) => {
    openReader(atomId, highlightText);
  }, [openReader]);

  if (!currentConversation) {
    return (
      <div className="flex items-center justify-center h-full text-[var(--color-text-secondary)]">
        {isLoading ? 'Loading conversation...' : 'No conversation selected'}
      </div>
    );
  }

  return (
    <div className="h-full flex flex-col">
      {/* Header with back button and scope */}
      <ChatHeader conversation={currentConversation} onBack={goBack} />

      {/* Messages area */}
      <div
        ref={messagesContainerRef}
        onScroll={handleScroll}
        className="flex-1 overflow-y-auto p-4 space-y-4 relative"
        style={{ overflowAnchor: 'none' }}
      >
        {/* Search bar */}
        {isSearchOpen && (
          <SearchBar
            query={searchQuery}
            searchedQuery={searchedQuery}
            onQueryChange={setSearchQuery}
            currentIndex={currentIndex}
            totalMatches={totalMatches}
            onNext={goToNext}
            onPrevious={goToPrevious}
            onClose={closeSearch}
          />
        )}
        {messages.length === 0 && !isStreaming && (
          <div className="flex flex-col items-center justify-center h-full gap-4 text-center">
            <div className="w-16 h-16 rounded-full bg-[var(--color-bg-card)] flex items-center justify-center">
              <MessageSquare className="w-8 h-8 text-[var(--color-accent)]" strokeWidth={2} />
            </div>
            <div>
              <p className="text-[var(--color-text-primary)] font-medium mb-1">Start the conversation</p>
              <p className="text-[var(--color-text-secondary)] text-sm max-w-sm">
                Ask questions about your knowledge base. The AI will search through your atoms to find relevant information.
              </p>
            </div>
          </div>
        )}

        {displayMessages.map(({ message, isRecap }) => (
          <div key={message.id}>
            {isRecap && <h3 className="text-sm font-medium text-[var(--color-text-secondary)] mb-2">Session recap</h3>}
            <ChatMessage
              message={message}
              onViewAtom={handleViewAtom}
              searchQuery={isSearchOpen ? searchQuery : ''}
              highlightText={highlightText}
            />
          </div>
        ))}

        {/* Streaming bubble — rendered for the whole streaming turn so tool
            calls show up while the model is still thinking and persist as
            streaming content flows in. ChatMessage handles the empty-content
            "Thinking…" fallback. */}
        {isStreaming && lifecycle?.phase !== 'closing' && (
          <ChatMessage
            message={{
              id: 'streaming',
              conversation_id: currentConversation.id,
              role: 'assistant',
              content: streamingContent,
              created_at: new Date().toISOString(),
              message_index: messages.length,
              tool_calls: streamingToolCalls,
              citations: [],
            }}
            isStreaming
            onViewAtom={handleViewAtom}
            searchQuery={isSearchOpen ? searchQuery : ''}
            highlightText={highlightText}
          />
        )}

        {lifecycle?.maintenance && lifecycle.maintenance.status !== 'complete' && (
          <p role="status" className="text-sm text-[var(--color-text-secondary)]">
            {lifecycle.maintenance.error ?? (lifecycle.maintenance.kind === 'purge'
              ? 'Removing native session files for this closed sitting. Reload to recover unfinished cleanup.'
              : 'Cleaning older tool and thinking content; dialogue is retained. Reload to recover unfinished cleanup.')}
          </p>
        )}

        {lifecycle?.phase === 'closing' && lifecycle.recap?.outcome.status !== 'complete' && (
          <div role="status" className="text-sm text-[var(--color-text-secondary)]">
            {lifecycle.recap?.outcome.status === 'failed' || lifecycle.recap?.outcome.status === 'uncertain'
              ? `${lifecycle.recap.outcome.error} Use Retry close-out to continue.`
              : isEndingSession ? 'Preparing session recap...' : 'Close-out is unfinished. Use Retry close-out; any active work will finish first.'}
            {streamingContent && <p className="mt-2 whitespace-pre-wrap">{streamingContent}</p>}
          </div>
        )}

        {lifecycle?.phase === 'closing' && lifecycle.recap?.outcome.status === 'complete' && !isEndingSession && (
          <p className="text-sm text-[var(--color-text-secondary)]">Recap saved. Use Retry close-out to finish saving the session.</p>
        )}

        {lifecycle?.phase === 'open' && lifecycle.active && !retryMessageId && !isStreaming && (
          <p className="text-sm text-[var(--color-text-secondary)]">This turn's outcome is unknown. Use End Session to close out from saved history.</p>
        )}

        {integrated && retryMessageId && !isStreaming && (
          <button disabled={isSending} className="text-sm text-[var(--color-accent)] disabled:opacity-50"
            onClick={() => retryMessage(retryMessageId)}>Retry reply</button>
        )}

        {/* Error message */}
        {(error || currentConversation.context_error || turnError) && (
          <div className="p-3 bg-red-500/10 border border-red-500/30 rounded-lg text-red-400 text-sm">
            {error ?? currentConversation.context_error ?? turnError}
          </div>
        )}

        <div ref={messagesEndRef} />
      </div>

      {/* Input area */}
      {lifecycle?.phase === 'closed' ? (
        <div className="p-4 text-sm text-[var(--color-text-secondary)]">
          This sitting is closed and read-only.
          <button onClick={startNewSitting} disabled={isLoading} className="ml-3 text-[var(--color-accent)] disabled:opacity-50">Start new sitting</button>
        </div>
      ) : <ChatInput
        value={inputValue}
        onChange={setInputValue}
        onSend={handleSend}
        onKeyDown={handleKeyDown}
        disabled={inputBlocked}
        placeholder={
          currentConversation.tags.length > 0
            ? `Ask about ${currentConversation.tags.map(t => t.name).join(', ')}...`
            : 'Ask anything about your knowledge base...'
        }
      />}
    </div>
  );
}
