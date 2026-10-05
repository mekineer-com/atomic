import { describe, it, expect, beforeEach, vi } from 'vitest';

// Transport module is a singleton wrapper over an HTTP client — the
// unit-under-test only calls `getTransport().invoke(...)`, so we mock the
// module entirely.
const invokeMock = vi.fn();

vi.mock('./transport', () => ({
  getTransport: () => ({ invoke: invokeMock }),
}));

import { createTagCache, getOrCreateTag, resolveTagIds } from './import-tags';

beforeEach(() => {
  invokeMock.mockReset();
  // Default: each create_tag call returns a unique id derived from the args.
  let counter = 0;
  invokeMock.mockImplementation(async (_name: string, args: { name: string; parentId: string | null }) => {
    counter++;
    return { id: `${args.parentId ?? 'root'}:${args.name}:${counter}` };
  });
});

describe('getOrCreateTag', () => {
  it('reuses a cached tag id for the same (name, parent)', async () => {
    const cache = createTagCache();
    const a = await getOrCreateTag('Work', null, cache);
    expect(a).toMatch(/^root:Work:/);
    expect(invokeMock).toHaveBeenCalledWith('create_tag', { name: 'Work', parentId: null });
    const b = await getOrCreateTag('Work', null, cache);
    expect(a).toBe(b);
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });

  it('treats the same name under different parents as distinct', async () => {
    const cache = createTagCache();
    const a = await getOrCreateTag('Tasks', 'p1', cache);
    const b = await getOrCreateTag('Tasks', 'p2', cache);
    expect(a).not.toBe(b);
    expect(invokeMock).toHaveBeenCalledTimes(2);
  });
});

describe('resolveTagIds', () => {
  it('builds nested tags bottom-up and returns only the leaf id', async () => {
    const cache = createTagCache();
    const ids = await resolveTagIds(
      [{ name: 'Tasks', parentPath: ['Projects', 'Work'] }],
      [],
      cache,
    );
    // Projects (root), Work (under Projects), Tasks (under Work)
    expect(invokeMock).toHaveBeenCalledTimes(3);
    expect(invokeMock.mock.calls[0][1]).toEqual({ name: 'Projects', parentId: null });
    expect(invokeMock.mock.calls[1][1].name).toBe('Work');
    expect(invokeMock.mock.calls[2][1].name).toBe('Tasks');
    expect(ids).toHaveLength(1);
  });

  it('adds flat tags at the root level', async () => {
    const cache = createTagCache();
    const ids = await resolveTagIds([], ['a', 'b'], cache);
    expect(ids).toHaveLength(2);
    expect(invokeMock).toHaveBeenCalledTimes(2);
  });

  it('deduplicates within a single call', async () => {
    const cache = createTagCache();
    const ids = await resolveTagIds(
      [
        { name: 'X', parentPath: [] },
        { name: 'X', parentPath: [] },
      ],
      ['X'],
      cache,
    );
    expect(ids).toHaveLength(1);
    // One creation, rest served from cache.
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });

  it('reuses cached ancestors across calls', async () => {
    const cache = createTagCache();
    await resolveTagIds([{ name: 'A', parentPath: ['Root'] }], [], cache);
    invokeMock.mockClear();
    await resolveTagIds([{ name: 'B', parentPath: ['Root'] }], [], cache);
    // 'Root' must come from cache; only 'B' is new.
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock.mock.calls[0][1].name).toBe('B');
  });
});
