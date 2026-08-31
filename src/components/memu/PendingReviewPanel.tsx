import { useCallback, useEffect, useState } from 'react';
import { getTransport } from '../../lib/transport';
import { useCanvasStore } from '../../stores/canvas';
import type { MemoryCitation } from '../../stores/atoms';
import { formatDate } from '../../lib/date';
import { DossierMarkdown, MemoryCitationLinks } from './DossierMarkdown';

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
  description?: string | null;
  approved_description?: string | null;
  category_kind?: 'lore' | 'topic' | 'goal' | null;
  active?: boolean;
  last_evidence_at?: string | null;
  last_revised_at?: string | null;
  citations?: MemoryCitation[];
};

type SoulSummaryReview = CategoryReview & {
  kind: string;
};

type SummaryMutationResponse = CategoryReview & {
  kind?: string;
  summaries_revision: number;
};

type PendingReviews = {
  items: MemoryReview[];
  categories: CategoryReview[];
  soul_summaries: SoulSummaryReview[];
  summaries_revision: number;
};

export function PendingReviewPanel() {
  const [reviews, setReviews] = useState<PendingReviews>({ items: [], categories: [], soul_summaries: [], summaries_revision: 0 });
  const [clusterColors, setClusterColors] = useState<Record<string, number>>({});
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [loadFailed, setLoadFailed] = useState(false);
  const [summariesStale, setSummariesStale] = useState(false);
  const [summaryBusy, setSummaryBusy] = useState(false);

  const loadReviews = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const fetched = await getTransport().invoke<PendingReviews>('list_pending_memu_reviews');
      setReviews(fetched);
      setClusterColors(assignClusterColors(fetched.items));
      setLoadFailed(false);
      setSummariesStale(false);
    } catch (err) {
      setError(String(err));
      setLoadFailed(true);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadReviews();
  }, [loadReviews]);

  const removeCategory = (id: string) => setReviews((r) => ({ ...r, categories: r.categories.filter((cat) => cat.id !== id) }));
  const summaryActionsDisabled = loading || loadFailed || summariesStale || summaryBusy;
  // Remove the acted-on row in place (no refetch: reordering would scatter its cluster
  // mates). Survivors keep their badge/color even when the last cluster mate goes —
  // consistent visuals beat live-updating cluster membership mid-review.
  const removeMemory = (id: string) => {
    useCanvasStore.getState().invalidateCanvasData();
    setReviews((r) => ({ ...r, items: r.items.filter((item) => item.id !== id) }));
  };
  const reportError = (err: unknown) => setError(String(err));

  return (
    <div className="h-full overflow-y-auto bg-[var(--color-bg-main)] p-4">
        <div className="mb-4 flex items-start justify-between gap-3">
          <div>
            <h2 className="text-lg font-semibold text-[var(--color-text-primary)]">memU review</h2>
            <p className="text-sm text-[var(--color-text-secondary)]">Approve agent edits and pending memories.</p>
          </div>
          <button type="button" disabled={loading} onClick={() => void loadReviews()} className="rounded border border-[var(--color-border)] px-3 py-1.5 text-sm disabled:opacity-50">Refresh</button>
        </div>

        {loading && <p className="text-sm text-[var(--color-text-secondary)]">Loading...</p>}
        {error && <p className="mb-3 rounded border border-red-500/40 bg-red-500/10 p-2 text-sm text-red-500">{error}</p>}
        {!loading && reviews.items.length === 0 && reviews.categories.length === 0 && reviews.soul_summaries.length === 0 && (
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
              <GeneratedSummaryRow
                key={category.id}
                review={category}
                kind="category"
                revision={reviews.summaries_revision}
                disabled={summaryActionsDisabled}
                stale={summariesStale}
                onStale={() => setSummariesStale(true)}
                onBusyChange={setSummaryBusy}
                onDone={(result) => {
                  useCanvasStore.getState().invalidateCanvasData();
                  removeCategory(category.id);
                  setReviews((r) => ({ ...r, summaries_revision: result.summaries_revision }));
                }}
                onError={reportError}
              />
            ))}
            {reviews.soul_summaries.map((summary) => (
              <GeneratedSummaryRow
                key={summary.id}
                review={summary}
                kind="soul"
                revision={reviews.summaries_revision}
                disabled={summaryActionsDisabled}
                stale={summariesStale}
                onStale={() => setSummariesStale(true)}
                onBusyChange={setSummaryBusy}
                onDone={(result) => setReviews((r) => ({
                  ...r,
                  summaries_revision: result.summaries_revision,
                  soul_summaries: r.soul_summaries.map((row) => row.kind === summary.kind ? { ...row, ...result, kind: summary.kind } : row),
                }))}
                onError={reportError}
              />
            ))}
          </section>
        </div>
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

function GeneratedSummaryRow({
  review,
  kind,
  revision,
  disabled,
  stale,
  onStale,
  onBusyChange,
  onDone,
  onError,
}: {
  review: CategoryReview | SoulSummaryReview;
  kind: 'category' | 'soul';
  revision: number;
  disabled: boolean;
  stale: boolean;
  onStale: () => void;
  onBusyChange: (busy: boolean) => void;
  onDone: (result: SummaryMutationResponse) => void;
  onError: (err: unknown) => void;
}) {
  const [summary, setSummary] = useState(review.summary);
  const [title, setTitle] = useState(review.label ?? '');
  const [description, setDescription] = useState(review.description ?? '');
  const [busy, setBusy] = useState(false);
  const edited = summary !== review.summary
    || (kind === 'category' && title !== (review.label ?? ''))
    || (kind === 'category' && description !== (review.description ?? ''));
  useEffect(() => {
    setSummary(review.summary);
    setTitle(review.label ?? '');
    setDescription(review.description ?? '');
  }, [review]);
  const run = async () => {
    if (disabled) return;
    setBusy(true);
    onBusyChange(true);
    try {
      const command = kind === 'category'
        ? (edited ? 'update_category_summary' : 'approve_category')
        : (edited ? 'update_soul_summary' : 'approve_soul_summary');
      const target = kind === 'category' ? { id: review.id } : { kind: (review as SoulSummaryReview).kind };
      const changes = kind === 'category' ? {
        ...(summary !== review.summary ? { summary } : {}),
        ...(title !== (review.label ?? '') ? { title } : {}),
        ...(description !== (review.description ?? '') ? { description } : {}),
      } : { summary };
      const result = await getTransport().invoke<SummaryMutationResponse>(command, {
        ...target,
        ...(edited ? changes : {}),
        displayed_summary: review.summary,
        summaries_revision: revision,
      });
      onDone(result);
    } catch (err) {
      if (String(err) === 'summary_snapshot_stale') onStale();
      else onError(err);
    } finally {
      setBusy(false);
      onBusyChange(false);
    }
  };

  return (
    <article className="rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-card)] p-3">
      {stale && <p className="mb-2 rounded border border-amber-500/40 bg-amber-500/10 p-2 text-sm text-amber-500">Summaries changed during a memorize cycle. Save any edits to another file, then refresh this page.</p>}
      {kind === 'category' ? (
        <div className="mb-3 space-y-2">
          <div className="flex flex-wrap items-center gap-2 text-xs text-[var(--color-text-tertiary)]">
            <span className="rounded-full border border-[var(--color-border)] px-2 py-0.5 capitalize">{review.category_kind ?? 'topic'}</span>
            <span>{review.active === false ? 'Inactive' : 'Active'}</span>
            {review.last_evidence_at && <span>Evidence: {formatDate(review.last_evidence_at)}</span>}
            {review.last_revised_at && <span>Revised: {formatDate(review.last_revised_at)}</span>}
          </div>
          <input className="w-full rounded border border-[var(--color-border)] bg-transparent p-2 text-sm font-medium" value={title} onChange={(e) => setTitle(e.target.value)} />
          <div className="grid gap-2 md:grid-cols-2">
            <p className="min-h-20 whitespace-pre-wrap rounded border border-[var(--color-border)] p-2 text-sm text-[var(--color-text-secondary)]">{review.approved_description ?? ''}</p>
            <textarea className="min-h-20 rounded border border-[var(--color-border)] bg-transparent p-2 text-sm" value={description} onChange={(e) => setDescription(e.target.value)} />
          </div>
        </div>
      ) : <div className="mb-2 text-sm font-medium">{review.label ?? review.id}</div>}
      <div className="grid gap-2 md:grid-cols-2">
        <div className={`prose prose-invert max-w-none overflow-y-auto rounded border border-[var(--color-border)] p-2 text-sm leading-5 [scrollbar-gutter:stable] text-[var(--color-text-secondary)] ${kind === 'category' ? 'min-h-[21rem]' : 'min-h-28'}`}>
          <DossierMarkdown citations={review.citations}>{review.approved_summary ?? ''}</DossierMarkdown>
        </div>
        <textarea className={`overflow-y-scroll rounded border border-[var(--color-border)] bg-transparent p-2 font-sans text-sm leading-5 tracking-normal [scrollbar-gutter:stable] ${kind === 'category' ? 'min-h-[21rem]' : 'min-h-28'}`} value={summary} onChange={(e) => setSummary(e.target.value)} />
      </div>
      {kind === 'category' && Boolean(review.citations?.length) && (
        <div className="mt-2">
          <MemoryCitationLinks citations={review.citations} />
        </div>
      )}
      <div className="mt-2 flex gap-2">
        <button disabled={busy || disabled} className="rounded bg-[var(--color-accent)] px-3 py-1 text-sm text-white transition enabled:hover:brightness-110 enabled:focus-visible:outline enabled:focus-visible:outline-2 enabled:focus-visible:outline-offset-2 enabled:focus-visible:outline-[var(--color-accent)] disabled:cursor-not-allowed disabled:opacity-[0.45]" onClick={() => void run()}>{edited ? 'Save + approve' : 'Approve'}</button>
        {kind === 'category' && <button disabled title="Delete not implemented" className="rounded border border-[var(--color-border)] px-3 py-1 text-sm opacity-50">Delete</button>}
      </div>
    </article>
  );
}
