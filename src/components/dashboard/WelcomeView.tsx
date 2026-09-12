import { Plus } from 'lucide-react';
import { startNewAtom } from '../../lib/new-atom';
import { isDesktopApp } from '../../lib/transport';
import { CaptureOptions } from './CaptureOptions';

function greeting(date: Date): string {
  const h = date.getHours();
  if (h < 5) return 'Working late';
  if (h < 12) return 'Good morning';
  if (h < 18) return 'Good afternoon';
  return 'Good evening';
}

function formatToday(date: Date): string {
  return date
    .toLocaleDateString(undefined, { weekday: 'long', month: 'long', day: 'numeric' })
    .toUpperCase();
}

function Kbd({ children }: { children: React.ReactNode }) {
  return (
    <kbd className="px-1.5 py-0.5 rounded border border-[var(--color-border)] bg-[var(--color-bg-hover)]/60 text-[11px] font-mono text-[var(--color-text-secondary)]">
      {children}
    </kbd>
  );
}

export function WelcomeView() {
  const handleCreate = async () => {
    try {
      await startNewAtom();
    } catch (err) {
      console.error('Failed to create atom:', err);
    }
  };

  const now = new Date();
  const modKey = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform) ? '⌘' : 'Ctrl';
  // Only surface ⌘N in the desktop build — the browser reserves it for a
  // new window and will swallow the keystroke before our listener sees it.
  const showNewAtomShortcut = isDesktopApp();

  return (
    <div className="h-full overflow-y-auto scrollbar-auto-hide">
      <div className="mx-auto max-w-4xl px-6 pt-10 pb-16 md:px-10 md:pt-14 md:pb-20">
        <div className="text-[11px] font-medium uppercase tracking-[0.14em] text-[var(--color-text-tertiary)] mb-3">
          {formatToday(now)}
        </div>

        <h1 className="text-3xl md:text-4xl font-semibold text-[var(--color-text-primary)] tracking-tight mb-4">
          {greeting(now)}.
        </h1>

        <p className="text-base md:text-lg text-[var(--color-text-secondary)] leading-relaxed max-w-2xl">
          Atomic turns freeform notes into a semantically-connected knowledge graph. Capture
          a thought — embeddings, tagging, and wiki synthesis happen behind the scenes.
        </p>

        <div className="mt-8 flex flex-wrap items-center gap-x-4 gap-y-3">
          <button
            onClick={handleCreate}
            className="inline-flex items-center gap-2 px-4 py-2 rounded-md bg-[var(--color-accent)] text-white text-sm font-medium hover:bg-[var(--color-accent-hover)] transition-colors"
          >
            <Plus className="w-4 h-4" strokeWidth={2.5} />
            Create your first atom
          </button>
          <span className="text-[13px] text-[var(--color-text-tertiary)]">
            {showNewAtomShortcut && (
              <>
                or press <Kbd>{modKey}N</Kbd> ·{' '}
              </>
            )}
            command palette <Kbd>{modKey}P</Kbd>
          </span>
        </div>

        <CaptureOptions />
      </div>
    </div>
  );
}
