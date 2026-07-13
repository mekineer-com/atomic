import { useEffect, useRef } from 'react';
import { toast } from 'sonner';
import {
  getTransport,
  TRANSPORT_CHANGED_EVENT,
  TRANSPORT_CONNECTION_EVENT,
} from '../lib/transport';
import { useAtomsStore } from '../stores/atoms';
import { useTagsStore } from '../stores/tags';
import { useUIStore } from '../stores/ui';
import { useEmbeddingProgressStore } from '../stores/embedding-progress';
import { useCanvasStore } from '../stores/canvas';
import type { AtomWithTags } from '../stores/atoms';

interface EmbeddingCompletePayload {
  atom_id: string;
  status: 'complete' | 'failed';
  error?: string;
}

interface TaggingCompletePayload {
  atom_id: string;
  status: 'complete' | 'failed' | 'skipped';
  error?: string;
  tags_extracted: string[];
  new_tags_created: string[];
}

interface EmbeddingsResetPayload {
  pending_count: number;
  reason: string;
}

interface PipelineQueueCompletedPayload {
  run_id: string;
  total_jobs: number;
  failed_jobs: number;
}

interface PipelineStatusSnapshot {
  pending: number;
  processing: number;
  queued_embedding?: number;
  queued_tagging?: number;
  tagging_pending: number;
  tagging_processing: number;
}

interface AllPipelineStatusesPayload {
  databases: Array<{ status: PipelineStatusSnapshot }>;
}

const DEBOUNCE_MS = 2000;
const STATUS_BATCH_MS = 500;
const STATUS_RECONCILE_MS = 600;

export function useEmbeddingEvents() {
  const pendingStatusUpdates = useRef<Array<{atomId: string, status: string}>>([]);
  const statusBatchTimer = useRef<ReturnType<typeof setTimeout>>();

  const needsAtomRefresh = useRef(false);
  const needsTagRefresh = useRef(false);
  const refetchDebounceTimer = useRef<ReturnType<typeof setTimeout>>();
  const statusReconcileTimer = useRef<ReturnType<typeof setTimeout>>();

  useEffect(() => {
    let unsubs: Array<() => void> = [];
    let disposed = false;

    const bindTransport = () => {
      unsubs.forEach(unsub => unsub());
      unsubs = [];
      if (disposed) return;

      const transport = getTransport();

      const reconcilePipelineStatus = () => {
        transport.invoke<AllPipelineStatusesPayload>('get_all_pipeline_statuses')
          .then((payload) => {
            const totals = payload.databases.reduce(
              (acc, item) => {
                acc.embedding += item.status.queued_embedding
                  ?? (item.status.pending + item.status.processing);
                acc.tagging += item.status.queued_tagging
                  ?? (item.status.tagging_pending + item.status.tagging_processing);
                return acc;
              },
              { embedding: 0, tagging: 0 },
            );

            useEmbeddingProgressStore.getState().setRemaining(totals);
          })
          .catch((e: unknown) => console.error('Failed to reconcile pipeline status:', e));
      };

      const scheduleStatusReconcile = () => {
        clearTimeout(statusReconcileTimer.current);
        statusReconcileTimer.current = setTimeout(reconcilePipelineStatus, STATUS_RECONCILE_MS);
      };

      reconcilePipelineStatus();

      unsubs.push(transport.subscribe<AtomWithTags>('atom-created', (payload) => {
        useAtomsStore.getState().addAtom(payload);
        useCanvasStore.getState().invalidateCanvasData();
        scheduleStatusReconcile();
      }));

      unsubs.push(transport.subscribe<AtomWithTags>('atom-updated', (payload) => {
        useAtomsStore.getState().addAtom(payload);
        useCanvasStore.getState().invalidateCanvasData();
        scheduleStatusReconcile();
      }));

      unsubs.push(transport.subscribe<{ atom_id: string }>('ingestion-complete', (payload) => {
        transport.invoke('get_atom', { id: payload.atom_id })
          .then((atom) => useAtomsStore.getState().addAtom(atom as AtomWithTags))
          .catch((e: unknown) => console.error('Failed to fetch ingested atom:', e));
        useCanvasStore.getState().invalidateCanvasData();
        scheduleStatusReconcile();
      }));

      unsubs.push(transport.subscribe<EmbeddingCompletePayload>('embedding-complete', (payload) => {
        useCanvasStore.getState().invalidateCanvasData();
        if (payload.status === 'failed') {
          toast.error('Embedding failed', { id: 'embedding-failure', description: payload.error });
        }

        pendingStatusUpdates.current.push({
          atomId: payload.atom_id,
          status: payload.status,
        });

        clearTimeout(statusBatchTimer.current);
        statusBatchTimer.current = setTimeout(() => {
          const updates = pendingStatusUpdates.current;
          if (updates.length > 0) {
            pendingStatusUpdates.current = [];
            useAtomsStore.getState().batchUpdateAtomStatuses(updates);
          }
        }, STATUS_BATCH_MS);

        scheduleStatusReconcile();
      }));

      unsubs.push(transport.subscribe<TaggingCompletePayload>('tagging-complete', (payload) => {
        useCanvasStore.getState().invalidateCanvasData();
        if (payload.status === 'failed') {
          console.error(`Tagging failed for atom ${payload.atom_id}:`, payload.error);
          toast.error('Tagging failed', { id: 'tagging-failure', description: payload.error });
        }

        useAtomsStore.getState().updateTaggingStatus(payload.atom_id, payload.status);

        if (payload.new_tags_created && payload.new_tags_created.length > 0) {
          needsTagRefresh.current = true;
        }
        needsAtomRefresh.current = true;

        clearTimeout(refetchDebounceTimer.current);
        refetchDebounceTimer.current = setTimeout(() => {
          const { addLoadingOperation, removeLoadingOperation } = useUIStore.getState();

          if (needsAtomRefresh.current) {
            needsAtomRefresh.current = false;
            const opId = `fetch-atoms-${Date.now()}`;
            addLoadingOperation(opId, 'Updating atoms...');
            useAtomsStore.getState().fetchAtoms().finally(() => removeLoadingOperation(opId));
          }

          if (needsTagRefresh.current) {
            needsTagRefresh.current = false;
            const opId = `fetch-tags-${Date.now()}`;
            addLoadingOperation(opId, 'Refreshing tags...');
            useTagsStore.getState().fetchTags().finally(() => removeLoadingOperation(opId));
          }
        }, DEBOUNCE_MS);

        scheduleStatusReconcile();
      }));

      unsubs.push(transport.subscribe<{ request_id: string; url: string; error: string }>('ingestion-failed', (payload) => {
        toast.error('Ingestion failed', { id: `ingestion-failed-${payload.request_id}`, description: `${payload.url}: ${payload.error}` });
      }));

      unsubs.push(transport.subscribe<{ url: string; request_id: string; error: string }>('ingestion-fetch-failed', (payload) => {
        toast.error('Failed to fetch URL', { id: `fetch-failed-${payload.request_id}`, description: `${payload.url}: ${payload.error}` });
      }));

      unsubs.push(transport.subscribe<{ feed_id: string; error: string }>('feed-poll-failed', (payload) => {
        toast.error('Feed poll failed', { id: `feed-poll-failed-${payload.feed_id}`, description: payload.error });
      }));

      unsubs.push(transport.subscribe<PipelineQueueCompletedPayload>('pipeline-queue-completed', (payload) => {
        if (payload.failed_jobs > 0) {
          toast.error('Pipeline completed with failures', {
            id: `pipeline-failed-${payload.run_id}`,
            description: `${payload.failed_jobs} of ${payload.total_jobs} jobs failed.`,
          });
        }
        scheduleStatusReconcile();
      }));

      const scheduleOnlyEvents = [
        'batch-progress',
        'pipeline-queue-started',
        'pipeline-queue-progress',
        'server-events-lagged',
      ];
      for (const event of scheduleOnlyEvents) {
        unsubs.push(transport.subscribe(event, scheduleStatusReconcile));
      }

      unsubs.push(transport.subscribe<EmbeddingsResetPayload>('embeddings-reset', (payload) => {
        useCanvasStore.getState().invalidateCanvasData();
        const { addLoadingOperation, removeLoadingOperation } = useUIStore.getState();
        const opId = `fetch-atoms-reset-${Date.now()}`;
        addLoadingOperation(opId, `Re-embedding ${payload.pending_count} atoms...`);
        useAtomsStore.getState().fetchAtoms().finally(() => removeLoadingOperation(opId));
        scheduleStatusReconcile();
      }));
    };

    const handleTransportChanged = () => bindTransport();
    const handleConnection = (event: Event) => {
      const connected = (event as CustomEvent<{ connected?: boolean }>).detail?.connected;
      if (connected) {
        bindTransport();
      }
    };

    bindTransport();
    window.addEventListener(TRANSPORT_CHANGED_EVENT, handleTransportChanged);
    window.addEventListener(TRANSPORT_CONNECTION_EVENT, handleConnection);

    return () => {
      disposed = true;
      clearTimeout(statusBatchTimer.current);
      clearTimeout(refetchDebounceTimer.current);
      clearTimeout(statusReconcileTimer.current);
      window.removeEventListener(TRANSPORT_CHANGED_EVENT, handleTransportChanged);
      window.removeEventListener(TRANSPORT_CONNECTION_EVENT, handleConnection);
      unsubs.forEach(unsub => unsub());
      unsubs = [];
    };
  }, []);
}
