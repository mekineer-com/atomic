import { afterEach, describe, expect, it, vi } from 'vitest';
import { HttpTransport } from './http';

describe('HttpTransport cancellation', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('connects its socket with the exact selected owner, Soul, database, and token', async () => {
    const socket = { onopen: null as (() => void) | null, close: vi.fn() };
    const websocket = vi.fn(function (_url: string) { return socket; });
    vi.stubGlobal('WebSocket', websocket);
    const previousUser = sessionStorage.getItem('openalma.user');
    const previousSoul = sessionStorage.getItem('openalma.soul');
    const transport = new HttpTransport({ baseUrl: 'https://atomic.invalid/', authToken: 'fictional+token&' });
    try {
      sessionStorage.setItem('openalma.user', 'Test Owner & Co');
      sessionStorage.setItem('openalma.soul', 'Paper Moon');
      transport.setDatabaseId('workspace+1');
      const connected = transport.connect();
      socket.onopen!();
      await connected;
      const url = new URL(websocket.mock.calls[0][0]);
      expect(url.origin + url.pathname).toBe('wss://atomic.invalid/ws');
      expect(Object.fromEntries(url.searchParams)).toEqual({
        token: 'fictional+token&', user_id: 'Test Owner & Co', soul_id: 'Paper Moon', db: 'workspace+1',
      });
    } finally {
      transport.disconnect();
      for (const [key, value] of [['openalma.user', previousUser], ['openalma.soul', previousSoul]] as const) {
        if (value === null) sessionStorage.removeItem(key);
        else sessionStorage.setItem(key, value);
      }
    }
  });

  it('returns null only for a missing conversation read, without granting resend permission', async () => {
    const fetchMock = vi.fn(async () => new Response('{"error":"missing"}', { status: 404 }));
    vi.stubGlobal('fetch', fetchMock);
    const transport = new HttpTransport({ baseUrl: 'http://localhost', authToken: 'test' });
    await expect(transport.invoke('get_conversation', { conversationId: 'deleted-chat' })).resolves.toBeNull();
    await expect(transport.invoke('send_chat_message', { conversationId: 'deleted-chat', content: 'Draft' })).rejects.toBe('missing');
    fetchMock.mockImplementation(async () => new Response('{"error":"unavailable"}', { status: 503 }));
    await expect(transport.invoke('get_conversation', { conversationId: 'deleted-chat' })).rejects.toBe('unavailable');
  });

  it('passes the caller signal to fetch', async () => {
    vi.stubGlobal('fetch', vi.fn((_url, init: RequestInit) => new Promise((_resolve, reject) => {
      init.signal?.addEventListener('abort', () => reject(new DOMException('Aborted', 'AbortError')));
    })));
    const transport = new HttpTransport({ baseUrl: 'http://localhost', authToken: 'test' });
    (transport as unknown as { connected: boolean }).connected = true;
    const controller = new AbortController();

    const request = transport.invoke('search_global_keyword', { query: 'fictional' }, { signal: controller.signal });
    controller.abort();

    await expect(request).rejects.toMatchObject({ name: 'AbortError' });
  });

  it('can identify a request as a Workspace operation independently of the selected source', async () => {
    const fetchMock = vi.fn(async (_url, _init: RequestInit) => new Response('{}', {
      status: 200,
      headers: { 'Content-Type': 'application/json' },
    }));
    vi.stubGlobal('fetch', fetchMock);
    const transport = new HttpTransport({ baseUrl: 'http://localhost', authToken: 'test' });

    await transport.invoke('update_atom_content_only', {
      id: 'fictional-note', content: 'Fictional content', tagIds: [],
    }, { workspace: true });

    expect(fetchMock.mock.calls[0][1].headers).toMatchObject({ 'X-Atomic-Source': 'workspace' });

    transport.setWorkspaceMode(true);
    await transport.invoke('search_atoms_hybrid', { query: 'Fictional', limit: 8, memoryOnly: true }, { workspace: false });
    expect(fetchMock.mock.calls[1][1].headers).not.toHaveProperty('X-Atomic-Source');
    await transport.invoke('list_atoms');
    expect(fetchMock.mock.calls[2][1].headers).toMatchObject({ 'X-Atomic-Source': 'workspace' });
  });

  it('does not send the Workspace header unless requested', async () => {
    const fetchMock = vi.fn(async (_url: string, _init: RequestInit) => new Response('{}', {
      status: 200,
      headers: { 'Content-Type': 'application/json' },
    }));
    vi.stubGlobal('fetch', fetchMock);
    const transport = new HttpTransport({ baseUrl: 'http://localhost', authToken: 'test' });

    await transport.invoke('list_atoms');

    expect(fetchMock.mock.calls[0][1].headers).not.toHaveProperty('X-Atomic-Source');
  });

  it('surfaces cited-memory conflicts with dossier names', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response(JSON.stringify({
      error: {
        message: 'Memory is cited in dossier prose',
        dossiers: [{ name: 'Fictional dossier' }],
      },
    }), {
      status: 409,
      headers: { 'Content-Type': 'application/json' },
    })));
    const transport = new HttpTransport({ baseUrl: 'http://localhost', authToken: 'test' });

    await expect(transport.invoke('delete_memory', {
      id: 'memory:fictional', displayed_summary: 'Fictional memory',
    }))
      .rejects.toBe('Memory is cited in dossier prose: Fictional dossier');
  });

  it('shows the launcher-directed pause without serializing internal state', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response(JSON.stringify({
      error: { code: 'soul_paused', message: 'TestSoul is paused. Retry in OpenAlma launcher.', reason: 'Failed' },
    }), { status: 409 })));
    const transport = new HttpTransport({ baseUrl: 'http://localhost', authToken: 'test' });
    await expect(transport.invoke('send_chat_message', { conversationId: 'test-chat', content: 'Draft' }))
      .rejects.toMatchObject({ code: 'soul_paused', message: 'TestSoul is paused. Retry in OpenAlma launcher.' });
  });

  it('sends memory baselines in update, approval, and delete requests', async () => {
    const fetchMock = vi.fn(async (_url: string, _init: RequestInit) => new Response('{}', {
      status: 200,
      headers: { 'Content-Type': 'application/json' },
    }));
    vi.stubGlobal('fetch', fetchMock);
    const transport = new HttpTransport({ baseUrl: 'http://localhost', authToken: 'test' });

    await transport.invoke('update_memory_summary', { id: 'memory:m1', summary: 'new', displayed_summary: 'old' });
    await transport.invoke('approve_memory', { id: 'memory:m1', displayed_summary: 'old' });
    await transport.invoke('delete_memory', { id: 'memory:m1', displayed_summary: 'old' });

    expect(fetchMock.mock.calls[0][1].body).toBe(JSON.stringify({ summary: 'new', displayed_summary: 'old' }));
    expect(fetchMock.mock.calls[1][1].body).toBe(JSON.stringify({ displayed_summary: 'old' }));
    expect(fetchMock.mock.calls[2][0]).toBe('http://localhost/api/memu/reviews/memory/memory%3Am1?displayed_summary=old');
  });
});
