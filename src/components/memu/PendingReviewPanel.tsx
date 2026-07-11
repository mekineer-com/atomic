import { useCallback, useEffect, useState } from 'react';
import { getTransport } from '../../lib/transport';

type MemoryReview = {
  id: string;
  summary: string;
  category_names?: string[];
  similar_to?: string[];
  similarity?: number;
};

const CLUSTER_COLORS = [
  { bar: '#f59e0b', badgeBg: 'rgba(245, 158, 11, 0.2)', badgeText: '#fcd34d' },
  { bar: '#0ea5e9', badgeBg: 'rgba(14, 165, 233, 0.2)', badgeText: '#7dd3fc' },
];

/** Assign each clustered item a color index once per fetch; removals never re-deal colors. */
function assignClusterColors(items: MemoryReview[]): Record<string, number> {
  const colorByKey: Record<string, number> = {};
  const colorByItem: Record<string, number> = {};
  let next = 0;
  for (const item of items) {
    if (!item.similar_to?.length) continue;
    const clusterKey = [item.id, ...item.similar_to].sort()[0];
    if (!(clusterKey in colorByKey)) colorByKey[clusterKey] = next++ % CLUSTER_COLORS.length;
    colorByItem[item.id] = colorByKey[clusterKey];
  }
  return colorByItem;
}

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
  const [clusterColors, setClusterColors] = useState<Record<string, number>>({});
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const loadReviews = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const fetched = await getTransport().invoke<PendingReviews>('list_pending_memu_reviews');
      setReviews(fetched);
      setClusterColors(assignClusterColors(fetched.items));
    } catch (err) {
      setError(String(err));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (!isOpen) return;
    void loadReviews();
  }, [isOpen, loadReviews]);

  if (!isOpen) return null;

  const removeCategory = (id: string) => setReviews((r) => ({ ...r, categories: r.categories.filter((cat) => cat.id !== id) }));
  // Remove the acted-on row in place (no refetch: reordering would scatter its cluster
  // mates). Survivors keep their badge/color even when the last cluster mate goes —
  // consistent visuals beat live-updating cluster membership mid-review.
  const removeMemory = (id: string) =>
    setReviews((r) => ({ ...r, items: r.items.filter((item) => item.id !== id) }));
  const reportError = (err: unknown) => setError(String(err));

  return (
    <div className="fixed inset-0 z-50 bg-black" onClick={onClose}>
      <aside
        className="h-full w-full overflow-y-auto bg-[var(--color-bg-main)] p-4"
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

        <div className="grid gap-4 lg:grid-cols-[2fr_3fr]">
          <section className="space-y-3">
            <h3 className="font-medium text-[var(--color-text-primary)]">Memories</h3>
            {reviews.items.map((item) => (
              <MemoryRow
                key={item.id}
                item={item}
                accentClass={item.similar_to?.length && clusterColors[item.id] != null ? CLUSTER_COLORS[clusterColors[item.id]] : null}
                onDone={() => removeMemory(item.id)}
                onError={reportError}
              />
            ))}
          </section>

          <section className="space-y-3">
            <h3 className="font-medium text-[var(--color-text-primary)]">Categories</h3>
            {reviews.categories.map((category) => (
              <CategoryRow key={category.id} category={category} onDone={() => removeCategory(category.id)} onError={reportError} />
            ))}
          </section>
        </div>
      </aside>
    </div>
  );
}

function MemoryRow({
  item,
  accentClass,
  onDone,
  onError,
}: {
  item: MemoryReview;
  accentClass: (typeof CLUSTER_COLORS)[number] | null;
  onDone: () => void;
  onError: (err: unknown) => void;
}) {
  const [summary, setSummary] = useState(item.summary);
  const [busy, setBusy] = useState(false);
  const edited = summary !== item.summary;
  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await fn();
      await onDone();
    } catch (err) {
      onError(err);
    } finally {
      setBusy(false);
    }
  };

  return (
    <article
      className="rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-card)] p-3"
      style={accentClass ? { borderLeft: `4px solid ${accentClass.bar}` } : undefined}
    >
      <div className="mb-2 flex items-center gap-2 text-xs text-[var(--color-text-tertiary)]">
        <span>{item.category_names?.join(', ')}</span>
        {item.similarity != null && (
          <span
            className="rounded-full px-2 py-0.5 font-medium"
            style={accentClass ? { backgroundColor: accentClass.badgeBg, color: accentClass.badgeText } : undefined}
          >
            ≈ {Math.round(item.similarity * 100)}%
          </span>
        )}
      </div>
      <textarea className="min-h-28 w-full rounded border border-[var(--color-border)] bg-transparent p-2 text-sm" value={summary} onChange={(e) => setSummary(e.target.value)} />
      <div className="mt-2 flex gap-2">
        <button disabled={busy} className="rounded bg-[var(--color-accent)] px-3 py-1 text-sm text-white transition enabled:hover:brightness-110 enabled:focus-visible:outline enabled:focus-visible:outline-2 enabled:focus-visible:outline-offset-2 enabled:focus-visible:outline-[var(--color-accent)] disabled:cursor-not-allowed disabled:opacity-[0.45]" onClick={() => run(() => getTransport().invoke(edited ? 'update_memory_summary' : 'approve_memory', edited ? { id: item.id, summary } : { id: item.id }))}>{edited ? 'Save + approve' : 'Approve'}</button>
        <button disabled={busy} className="rounded border border-red-500/50 px-3 py-1 text-sm text-red-500 transition-colors enabled:hover:border-red-500 enabled:hover:bg-red-500/10 enabled:focus-visible:outline enabled:focus-visible:outline-2 enabled:focus-visible:outline-offset-2 enabled:focus-visible:outline-red-500 disabled:cursor-not-allowed disabled:opacity-[0.45]" onClick={() => run(() => getTransport().invoke('delete_memory', { id: item.id }))}>Delete</button>
      </div>
    </article>
  );
}

function CategoryRow({ category, onDone, onError }: { category: CategoryReview; onDone: () => void; onError: (err: unknown) => void }) {
  const [summary, setSummary] = useState(category.summary);
  const [busy, setBusy] = useState(false);
  const edited = summary !== category.summary;
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
        <button disabled={busy} className="rounded bg-[var(--color-accent)] px-3 py-1 text-sm text-white transition enabled:hover:brightness-110 enabled:focus-visible:outline enabled:focus-visible:outline-2 enabled:focus-visible:outline-offset-2 enabled:focus-visible:outline-[var(--color-accent)] disabled:cursor-not-allowed disabled:opacity-[0.45]" onClick={() => run(() => getTransport().invoke(edited ? 'update_category_summary' : 'approve_category', edited ? { id: category.id, summary } : { id: category.id }))}>{edited ? 'Save + approve' : 'Approve'}</button>
        <button disabled className="rounded border border-[var(--color-border)] px-3 py-1 text-sm opacity-50">Delete disabled</button>
      </div>
    </article>
  );
}
