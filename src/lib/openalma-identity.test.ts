import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { Transport } from './transport/types';
import { createOpenAlmaSoul } from './openalma-identity';

describe('createOpenAlmaSoul', () => {
  beforeEach(() => {
    sessionStorage.clear();
    localStorage.clear();
  });

  it('returns only the exact server-published soul', async () => {
    const invoke = vi.fn()
      .mockResolvedValueOnce({ soul_id: 'Fable', created: true })
      .mockResolvedValueOnce({ souls: ['Fable'] });

    await expect(createOpenAlmaSoul({ invoke } as unknown as Transport, 'Fable')).resolves.toBe('Fable');
    expect(invoke).toHaveBeenNthCalledWith(1, 'create_openalma_soul', {
      soul_id: 'Fable',
      use_existing: false,
    });
    expect(invoke).toHaveBeenNthCalledWith(2, 'get_openalma_souls');
  });

  it('does not select a soul absent from refreshed discovery', async () => {
    const invoke = vi.fn()
      .mockResolvedValueOnce({ soul_id: 'Fable', created: true })
      .mockResolvedValueOnce({ souls: ['Quill'] });

    await expect(createOpenAlmaSoul({ invoke } as unknown as Transport, 'Fable')).rejects.toThrow(
      'Created Soul was not returned by the server',
    );
    expect(sessionStorage.getItem('openalma.soul')).toBeNull();
  });
});
