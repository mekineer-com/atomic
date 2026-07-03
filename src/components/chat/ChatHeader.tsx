import { useEffect, useState } from 'react';
import { ChevronLeft } from 'lucide-react';
import { getTransport } from '../../lib/transport';
import { ConversationWithTags, useChatStore } from '../../stores/chat';
import { ScopeEditor } from './ScopeEditor';

interface ChatHeaderProps {
  conversation: ConversationWithTags;
  onBack: () => void;
}

export function ChatHeader({ conversation, onBack }: ChatHeaderProps) {
  const [isEditingTitle, setIsEditingTitle] = useState(false);
  const [editedTitle, setEditedTitle] = useState(conversation.title || '');
  const [memuEnabled, setMemuEnabled] = useState(false);
  const updateConversationTitle = useChatStore(s => s.updateConversationTitle);
  const endMemuSession = useChatStore(s => s.endMemuSession);
  const isStreaming = useChatStore(s => s.isStreaming);
  const isEndingSession = useChatStore(s => s.isEndingSession);

  useEffect(() => {
    let cancelled = false;
    getTransport()
      .invoke<{ enabled: boolean }>('get_memu_review_status')
      .then((status) => {
        if (!cancelled) setMemuEnabled(Boolean(status.enabled));
      })
      .catch(() => {
        if (!cancelled) setMemuEnabled(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const handleTitleSave = async () => {
    if (editedTitle.trim() !== conversation.title) {
      await updateConversationTitle(conversation.id, editedTitle.trim() || 'Untitled');
    }
    setIsEditingTitle(false);
  };

  const handleTitleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter') {
      handleTitleSave();
    } else if (e.key === 'Escape') {
      setEditedTitle(conversation.title || '');
      setIsEditingTitle(false);
    }
  };

  return (
    <div className="flex-shrink-0 border-b border-[var(--color-border)]">
      {/* Top row: Back button and title */}
      <div className="flex items-center gap-3 px-4 py-3">
        <button
          onClick={onBack}
          className="p-1.5 text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] rounded transition-colors"
          aria-label="Back to conversations"
        >
          <ChevronLeft className="w-5 h-5" strokeWidth={2} />
        </button>

        {isEditingTitle ? (
          <input
            type="text"
            value={editedTitle}
            onChange={(e) => setEditedTitle(e.target.value)}
            onBlur={handleTitleSave}
            onKeyDown={handleTitleKeyDown}
            autoComplete="off"
            autoCorrect="off"
            autoCapitalize="off"
            spellCheck={false}
            className="flex-1 bg-[var(--color-bg-main)] border border-[var(--color-border)] rounded px-2 py-1 text-[var(--color-text-primary)] focus:outline-none focus:border-[var(--color-accent)]"
            autoFocus
          />
        ) : (
          <h2
            onClick={() => {
              setEditedTitle(conversation.title || '');
              setIsEditingTitle(true);
            }}
            className="flex-1 text-[var(--color-text-primary)] font-medium cursor-pointer hover:text-[var(--color-accent-light)] transition-colors truncate"
            title="Click to edit title"
          >
            {conversation.title || 'New Conversation'}
          </h2>
        )}
        {memuEnabled && (
          <button
            onClick={endMemuSession}
            disabled={isStreaming || isEndingSession}
            className="px-3 py-1.5 text-sm text-[var(--color-text-secondary)] border border-[var(--color-border)] rounded hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
          >
            {isEndingSession ? 'Ending...' : 'End Session'}
          </button>
        )}
      </div>

      {/* Scope editor row */}
      <div className="px-4 pb-3">
        <ScopeEditor conversation={conversation} />
      </div>
    </div>
  );
}
