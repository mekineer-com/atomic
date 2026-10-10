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

export async function createOpenAlmaSoul(transport: Transport, soulId: string, userName: string): Promise<string> {
  userName = userName.trim();
  if (!userName) throw new Error('Enter what this Soul should call you');
  const created = await transport.invoke<{ soul_id: string }>('create_openalma_soul', {
    soul_id: soulId,
    use_existing: false,
    user_name: userName,
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
    const userName = proposedUser && proposedSoul ? window.prompt(`What should ${proposedSoul} call you?`)?.trim() || '' : '';
    if (!proposedUser || !proposedSoul || !userName || !window.confirm(`Create OpenAlma for ${proposedUser} with ${proposedSoul} calling you ${userName}?`)) {
      throw new Error('OpenAlma owner confirmation is required');
    }
    userId = (await transport.invoke<{ user_id: string }>('create_openalma_owner', { user_id: proposedUser })).user_id;
    await createOpenAlmaSoul(transport, proposedSoul, userName);
    souls = (await transport.invoke<{ souls: string[] }>('get_openalma_souls')).souls;
  }

  if (souls.length === 0) {
    const proposed = window.prompt('What is your first Soul\'s name?')?.trim() || '';
    const userName = proposed ? window.prompt(`What should ${proposed} call you?`)?.trim() || '' : '';
    if (!proposed || !userName || !window.confirm(`Create ${proposed} as your first Soul, calling you ${userName}?`)) {
      throw new Error('First Soul confirmation is required');
    }
    await createOpenAlmaSoul(transport, proposed, userName);
    souls = (await transport.invoke<{ souls: string[] }>('get_openalma_souls')).souls;
  }

  if (!userId) throw new Error('OpenAlma owner is missing');

  const selected = currentIdentity()?.soulId || localStorage.getItem(SOUL_KEY) || '';
  selectSoul(userId, souls.includes(selected) ? selected : souls[0]);
  return { userId, souls };
}
