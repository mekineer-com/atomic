import { beforeEach, describe, expect, it, vi } from 'vitest';

const { readCache, writeCache, invoke } = vi.hoisted(() => ({
  readCache: vi.fn(),
  writeCache: vi.fn(),
  invoke: vi.fn(),
}));

vi.mock('../lib/cache/idb', () => ({
  cacheKey: (kind: string, dbId: string) => `${kind}:${dbId}`,
  readCache,
  writeCache,
}));
vi.mock('../lib/transport', () => ({ getTransport: () => ({ invoke }) }));

import { useAtomsStore } from './atoms';
import { useDatabasesStore } from './databases';
import { useTagsStore } from './tags';

describe('integrated cache isolation', () => {
  beforeEach(() => {
    sessionStorage.clear();
    readCache.mockReset();
    writeCache.mockReset();
    invoke.mockReset();
    useAtomsStore.getState().reset();
    useTagsStore.getState().reset();
    useDatabasesStore.setState({ activeId: 'test-db' });
  });

  it('bypasses database-only cache reads and writes in OpenAlma mode', async () => {
    sessionStorage.setItem('openalma.user', 'TestOwner');
    sessionStorage.setItem('openalma.soul', 'TestSoul');
    invoke.mockImplementation((command: string) => Promise.resolve(
      command === 'list_atoms'
        ? { atoms: [], total_count: 0, next_cursor: null, next_cursor_id: null }
        : [],
    ));

    await Promise.all([
      useAtomsStore.getState().hydrateFromCache(),
      useTagsStore.getState().hydrateFromCache(),
      useAtomsStore.getState().fetchAtoms(),
      useTagsStore.getState().fetchTags(),
    ]);

    expect(readCache).not.toHaveBeenCalled();
    expect(writeCache).not.toHaveBeenCalled();
  });

  it('keeps standalone cache writes', async () => {
    invoke.mockImplementation((command: string) => Promise.resolve(
      command === 'list_atoms'
        ? { atoms: [], total_count: 0, next_cursor: null, next_cursor_id: null }
        : [],
    ));

    await Promise.all([
      useAtomsStore.getState().fetchAtoms(),
      useTagsStore.getState().fetchTags(),
    ]);

    expect(writeCache).toHaveBeenCalledTimes(2);
  });
});
