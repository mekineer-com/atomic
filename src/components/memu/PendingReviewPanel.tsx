import { useEffect, useState } from 'react';
import { getTransport } from '../../lib/transport';

type MemoryReview = {
  id: string;
  summary: string;
  category_names?: string[];
};

type CategoryReview = {
  id: string;
  summary: string;
  approved_summary?: string | null;
  label?: string;
};

type PendingReviews = {
  items: MemoryReview[];
  categories: CategoryReview[];
};

export function PendingReviewPanel({ isOpen, onClose }: { isOpen: boolean; onClose: () => void }) {
  const [reviews, setReviews] = useState<PendingReviews>({ items: [], categories: [] });
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (!isOpen) return;
    setLoading(true);
    setError(null);
    getTransport()
      .invoke<PendingReviews>('list_pending_memu_reviews')
      .then(setReviews)
      .catch((err) => setError(String(err)))
      .finally(() => setLoading(false));
  }, [isOpen]);

  if (!isOpen) return null;

  const removeMemory = (id: string) => setReviews((r) => ({ ...r, items: r.items.filter((item) => item.id !== id) }));
  const removeCategory = (id: string) => setReviews((r) => ({ ...r, categories: r.categories.filter((cat) => cat.id !== id) }));
  const reportError = (err: unknown) => setError(String(err));

  return (
    <div className="fixed inset-0 z-50 flex justify-end bg-black/30" onClick={onClose}>
      <aside
        className="h-full w-full max-w-2xl overflow-y-auto border-l border-[var(--color-border)] bg-[var(--color-bg-main)] p-4 shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-4 flex items-center justify-between">
          <div>
            <h2 className="text-lg font-semibold text-[var(--color-text-primary)]">memU review</h2>
            <p className="text-sm text-[var(--color-text-secondary)]">Approve agent edits and pending memories.</p>
          </div>
          <button className="rounded px-3 py-1 text-sm hover:bg-[var(--color-bg-hover)]" onClick={onClose}>Close</button>
        </div>

        {loading && <p className="text-sm text-[var(--color-text-secondary)]">Loading...</p>}
        {error && <p className="mb-3 rounded border border-red-500/40 bg-red-500/10 p-2 text-sm text-red-500">{error}</p>}
        {!loading && reviews.items.length === 0 && reviews.categories.length === 0 && (
          <p className="text-sm text-[var(--color-text-secondary)]">Nothing pending.</p>
        )}

        <section className="space-y-3">
          {reviews.items.map((item) => (
            <MemoryRow key={item.id} item={item} onDone={() => removeMemory(item.id)} onError={reportError} />
          ))}
        </section>

        {reviews.categories.length > 0 && <h3 className="mt-6 mb-3 font-medium text-[var(--color-text-primary)]">Categories</h3>}
        <section className="space-y-3">
          {reviews.categories.map((category) => (
            <CategoryRow key={category.id} category={category} onDone={() => removeCategory(category.id)} onError={reportError} />
          ))}
        </section>
      </aside>
    </div>
  );
}

function MemoryRow({ item, onDone, onError }: { item: MemoryReview; onDone: () => void; onError: (err: unknown) => void }) {
  const [summary, setSummary] = useState(item.summary);
  const [busy, setBusy] = useState(false);
  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await fn();
      onDone();
    } catch (err) {
      onError(err);
    } finally {
      setBusy(false);
    }
  };

  return (
    <article className="rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-card)] p-3">
      <div className="mb-2 text-xs text-[var(--color-text-tertiary)]">{item.category_names?.join(', ')}</div>
      <textarea className="min-h-28 w-full rounded border border-[var(--color-border)] bg-transparent p-2 text-sm" value={summary} onChange={(e) => setSummary(e.target.value)} />
      <div className="mt-2 flex gap-2">
        <button disabled={busy} className="rounded bg-[var(--color-accent)] px-3 py-1 text-sm text-white" onClick={() => run(() => getTransport().invoke('approve_memory', { id: item.id }))}>Approve</button>
        <button disabled={busy} className="rounded border border-[var(--color-border)] px-3 py-1 text-sm" onClick={() => run(() => getTransport().invoke('update_memory_summary', { id: item.id, summary }))}>Save + approve</button>
        <button disabled={busy} className="rounded border border-red-500/50 px-3 py-1 text-sm text-red-500" onClick={() => run(() => getTransport().invoke('delete_memory', { id: item.id }))}>Delete</button>
      </div>
    </article>
  );
}

function CategoryRow({ category, onDone, onError }: { category: CategoryReview; onDone: () => void; onError: (err: unknown) => void }) {
  const [summary, setSummary] = useState(category.summary);
  const [busy, setBusy] = useState(false);
  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await fn();
      onDone();
    } catch (err) {
      onError(err);
    } finally {
      setBusy(false);
    }
  };

  return (
    <article className="rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-card)] p-3">
      <div className="mb-2 text-sm font-medium">{category.label ?? category.id}</div>
      <div className="grid gap-2 md:grid-cols-2">
        <pre className="min-h-28 whitespace-pre-wrap rounded border border-[var(--color-border)] p-2 text-xs text-[var(--color-text-secondary)]">{category.approved_summary ?? ''}</pre>
        <textarea className="min-h-28 rounded border border-[var(--color-border)] bg-transparent p-2 text-sm" value={summary} onChange={(e) => setSummary(e.target.value)} />
      </div>
      <div className="mt-2 flex gap-2">
        <button disabled={busy} className="rounded bg-[var(--color-accent)] px-3 py-1 text-sm text-white" onClick={() => run(() => getTransport().invoke('approve_category', { id: category.id }))}>Approve</button>
        <button disabled={busy} className="rounded border border-[var(--color-border)] px-3 py-1 text-sm" onClick={() => run(() => getTransport().invoke('update_category_summary', { id: category.id, summary }))}>Save + approve</button>
        <button disabled className="rounded border border-[var(--color-border)] px-3 py-1 text-sm opacity-50">Delete disabled</button>
      </div>
    </article>
  );
}
