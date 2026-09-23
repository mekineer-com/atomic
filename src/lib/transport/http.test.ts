import { afterEach, describe, expect, it, vi } from 'vitest';
import { HttpTransport } from './http';

describe('HttpTransport cancellation', () => {
  afterEach(() => vi.unstubAllGlobals());

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
