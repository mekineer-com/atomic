import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { Transport } from './transport/types';
import { createOpenAlmaSoul, ensureOpenAlmaIdentity } from './openalma-identity';

describe('createOpenAlmaSoul', () => {
  beforeEach(() => {
    sessionStorage.clear();
    localStorage.clear();
  });
  afterEach(() => vi.restoreAllMocks());

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

  it.each([null, 'AccountKey'])('creates the first Soul without a separate human name with owner %s', async (owner) => {
    let published = false;
    const invoke = vi.fn(async (command: string) => {
      if (command === 'get_memu_review_status') return { enabled: true };
      if (command === 'get_openalma_owner') return { user_id: owner };
      if (command === 'get_openalma_souls') return { souls: published ? ['Fable'] : [] };
      if (command === 'create_openalma_owner') return { user_id: 'AccountKey' };
      if (command === 'create_openalma_soul') { published = true; return { soul_id: 'Fable' }; }
      throw new Error(command);
    });
    const prompt = vi.spyOn(window, 'prompt');
    if (!owner) prompt.mockReturnValueOnce('AccountKey');
    prompt.mockReturnValueOnce('Fable');
    vi.spyOn(window, 'confirm').mockReturnValue(true);
    await expect(ensureOpenAlmaIdentity({ invoke } as unknown as Transport)).resolves.toEqual({ userId: 'AccountKey', souls: ['Fable'] });
    expect(invoke).toHaveBeenCalledWith('create_openalma_soul', { soul_id: 'Fable', use_existing: false });
    expect(prompt).toHaveBeenCalledTimes(owner ? 1 : 2);
    expect(sessionStorage.getItem('openalma.user')).toBe('AccountKey');
  });

  it('does not publish an owner or Soul after cancelling the Soul-name prompt', async () => {
    const invoke = vi.fn(async (command: string) => {
      if (command === 'get_memu_review_status') return { enabled: true };
      if (command === 'get_openalma_owner') return { user_id: null };
      return { souls: [] };
    });
    vi.spyOn(window, 'prompt').mockReturnValueOnce('AccountKey').mockReturnValueOnce(null);
    await expect(ensureOpenAlmaIdentity({ invoke } as unknown as Transport)).rejects.toThrow('confirmation');
    expect(invoke.mock.calls.some(([command]) => command.startsWith('create_'))).toBe(false);
    expect(sessionStorage.getItem('openalma.soul')).toBeNull();
  });

  it('selects an existing Soul without asking for a new human name', async () => {
    const invoke = vi.fn(async (command: string) => {
      if (command === 'get_memu_review_status') return { enabled: true };
      if (command === 'get_openalma_owner') return { user_id: 'AccountKey' };
      return { souls: ['Fable'] };
    });
    const prompt = vi.spyOn(window, 'prompt');
    await ensureOpenAlmaIdentity({ invoke } as unknown as Transport);
    expect(prompt).not.toHaveBeenCalled();
    expect(invoke.mock.calls.some(([command]) => command.startsWith('create_'))).toBe(false);
  });
});
