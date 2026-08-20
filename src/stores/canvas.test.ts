import { describe, expect, it } from 'vitest';
import { useCanvasStore } from './canvas';

describe('canvas cache', () => {
  it('invalidates base and filtered layout data together', () => {
    const data = { atoms: [], edges: [], clusters: [] };
    const store = useCanvasStore.getState();
    const generation = store.canvasInvalidation;
    store.setCanvasData(data, 'db-1');
    store.setCanvasRebuildData(data, 'db-1:visible');

    store.invalidateCanvasData();

    expect(useCanvasStore.getState()).toMatchObject({
      canvasData: null,
      canvasDataDbId: null,
      canvasRebuildData: null,
      canvasRebuildKey: null,
      canvasInvalidation: generation + 1,
    });
  });
});
