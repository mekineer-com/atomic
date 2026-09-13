import { useRef, useEffect } from 'react';
import { Search, Loader2 } from 'lucide-react';

interface CommandInputProps {
  query: string;
  onChange: (value: string) => void;
  onKeyDown?: (e: React.KeyboardEvent) => void;
  isSearching: boolean;
  shortcutHint?: string | null;
  placeholder?: string;
  prefix?: {
    token: string;
    label: string;
  } | null;
  onClearPrefix?: () => void;
}

export function CommandInput({
  query,
  onChange,
  onKeyDown,
  isSearching,
  shortcutHint = '⌘⇧P',
  placeholder = 'Type a command...',
  prefix = null,
  onClearPrefix,
}: CommandInputProps) {
  const inputRef = useRef<HTMLInputElement>(null);

  // Auto-focus on mount
  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  return (
    <div className="flex items-center gap-3 px-4 py-3 border-b border-[var(--color-border)]">
      <div className="text-[var(--color-text-secondary)]">
        {isSearching ? (
          <Loader2 className="w-5 h-5 animate-spin" strokeWidth={2} />
        ) : (
          <Search className="w-5 h-5" strokeWidth={2} />
        )}
      </div>

      {prefix ? (
        <button
          type="button"
          onClick={() => {
            onClearPrefix?.();
            inputRef.current?.focus();
          }}
          className="inline-flex items-center gap-1 rounded-full border border-[var(--color-border)] bg-[var(--color-bg-hover)] px-2 py-1 text-xs text-[var(--color-text-secondary)] transition-colors hover:border-[var(--color-accent)] hover:text-[var(--color-text-primary)]"
          title={`Clear ${prefix.label} prefix`}
        >
          <span className="font-mono text-[10px] text-[var(--color-accent-light)]">{prefix.token}</span>
          <span>{prefix.label}</span>
        </button>
      ) : null}

      <input
        ref={inputRef}
        type="text"
        value={query}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Backspace' && prefix && !query) {
            e.preventDefault();
            onClearPrefix?.();
            return;
          }
          onKeyDown?.(e);
        }}
        placeholder={placeholder}
        className="flex-1 min-w-0 bg-transparent text-[var(--color-text-primary)] placeholder-[var(--color-text-tertiary)] outline-none text-base"
        autoComplete="off"
        autoCorrect="off"
        autoCapitalize="off"
        spellCheck={false}
      />

      {shortcutHint ? (
        <div className="flex items-center gap-2 text-xs text-[var(--color-text-secondary)]">
          <kbd className="px-1.5 py-0.5 bg-[var(--color-bg-hover)] border border-[var(--color-border-hover)] rounded text-[10px] font-mono text-[var(--color-text-primary)]">
            {shortcutHint}
          </kbd>
        </div>
      ) : null}
    </div>
  );
}
