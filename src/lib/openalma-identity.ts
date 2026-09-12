import type { Transport } from './transport/types';

const USER_KEY = 'openalma.user';
const SOUL_KEY = 'openalma.soul';

export function currentIdentity(): { userId: string; soulId: string } | null {
  const userId = sessionStorage.getItem(USER_KEY) || '';
  const soulId = sessionStorage.getItem(SOUL_KEY) || '';
  return userId && soulId ? { userId, soulId } : null;
}

export function selectSoul(userId: string, soulId: string): void {
  sessionStorage.setItem(USER_KEY, userId);
  sessionStorage.setItem(SOUL_KEY, soulId);
  localStorage.setItem(SOUL_KEY, soulId);
}

export async function createOpenAlmaSoul(transport: Transport, soulId: string): Promise<string> {
  const created = await transport.invoke<{ soul_id: string }>('create_openalma_soul', {
    soul_id: soulId,
    use_existing: false,
  });
  const { souls } = await transport.invoke<{ souls: string[] }>('get_openalma_souls');
  if (!souls.includes(created.soul_id)) throw new Error('Created Soul was not returned by the server');
  return created.soul_id;
}

export async function ensureOpenAlmaIdentity(transport: Transport): Promise<{ userId: string; souls: string[] } | null> {
  const status = await transport.invoke<{ enabled: boolean }>('get_memu_review_status');
  if (!status.enabled) return null;
  let [{ user_id: userId }, { souls }] = await Promise.all([
    transport.invoke<{ user_id: string | null }>('get_openalma_owner'),
    transport.invoke<{ souls: string[] }>('get_openalma_souls'),
  ]);
  if (!userId && souls.length === 0) {
    const proposedUser = window.prompt('What is your name?')?.trim() || '';
    const proposedSoul = window.prompt('What is your first Soul\'s name?')?.trim() || '';
    if (!proposedUser || !proposedSoul || !window.confirm(`Create OpenAlma for ${proposedUser} with ${proposedSoul} as the first Soul?`)) {
      throw new Error('OpenAlma owner confirmation is required');
    }
    userId = (await transport.invoke<{ user_id: string }>('create_openalma_owner', { user_id: proposedUser })).user_id;
    await transport.invoke('create_openalma_soul', { soul_id: proposedSoul, use_existing: false });
    souls = (await transport.invoke<{ souls: string[] }>('get_openalma_souls')).souls;
  }

  if (souls.length === 0) {
    const proposed = window.prompt('What is your first Soul\'s name?')?.trim() || '';
    if (!proposed || !window.confirm(`Create ${proposed} as your first Soul?`)) {
      throw new Error('First Soul confirmation is required');
    }
    await transport.invoke('create_openalma_soul', { soul_id: proposed, use_existing: false });
    souls = (await transport.invoke<{ souls: string[] }>('get_openalma_souls')).souls;
  }

  if (!userId) throw new Error('OpenAlma owner is missing');

  const selected = currentIdentity()?.soulId || localStorage.getItem(SOUL_KEY) || '';
  selectSoul(userId, souls.includes(selected) ? selected : souls[0]);
  return { userId, souls };
}
