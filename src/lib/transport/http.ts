import type { Transport, HttpTransportConfig } from './types';
import { COMMAND_MAP } from './command-map';
import { normalizeServerEvent } from './event-normalizer';

export class HttpTransport implements Transport {
  readonly mode = 'http' as const;
  private config: HttpTransportConfig;
  private ws: WebSocket | null = null;
  private listeners = new Map<string, Set<(payload: any) => void>>();
  private connected = false;
  private shouldReconnect = false;
  private reconnectDelay = 1000;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private wsUrl: string | null = null;
  private authExpired = false;
  private visibilityHandler: (() => void) | null = null;
  private onlineHandler: (() => void) | null = null;
  onConnectionChange?: (connected: boolean) => void;

  constructor(config: HttpTransportConfig) {
    this.config = config;
  }

  getConfig(): HttpTransportConfig {
    return this.config;
  }

  async connect(): Promise<void> {
    if (!this.config.baseUrl) return;
    this.shouldReconnect = true;
    this.wsUrl = this.config.baseUrl
      .replace(/^http/, 'ws')
      .replace(/\/$/, '')
      + `/ws?token=${encodeURIComponent(this.config.authToken)}`;
    this.attachLifecycleListeners();
    try {
      await this.connectWs();
    } catch {
      // WebSocket failed (stale token, server down, etc.) — don't block app startup.
      // HTTP calls will detect auth issues; reconnect will retry in background.
      this.scheduleReconnect();
    }
  }

  /// On mobile (and whenever a browser tab is backgrounded) the OS will kill
  /// the WebSocket silently. When we come back to foreground or the network
  /// returns, we want to reconnect immediately instead of waiting out the
  /// current exponential-backoff delay (which can be up to 30s).
  private attachLifecycleListeners(): void {
    if (typeof window === 'undefined') return;
    if (this.visibilityHandler || this.onlineHandler) return; // already attached

    const wakeUp = () => {
      if (!this.shouldReconnect || this.connected) return;
      // If a connection attempt is already in flight, don't start another
      // one — overwriting `this.ws` would orphan the pending socket, which
      // could then resolve later, fire a spurious onConnectionChange, and
      // leak an open WebSocket we'll never close.
      if (this.ws && this.ws.readyState === WebSocket.CONNECTING) return;
      this.forceReconnectSoon();
    };

    this.visibilityHandler = () => {
      if (document.visibilityState === 'visible') wakeUp();
    };
    this.onlineHandler = wakeUp;

    document.addEventListener('visibilitychange', this.visibilityHandler);
    window.addEventListener('online', this.onlineHandler);
  }

  private detachLifecycleListeners(): void {
    if (typeof window === 'undefined') return;
    if (this.visibilityHandler) {
      document.removeEventListener('visibilitychange', this.visibilityHandler);
      this.visibilityHandler = null;
    }
    if (this.onlineHandler) {
      window.removeEventListener('online', this.onlineHandler);
      this.onlineHandler = null;
    }
  }

  private forceReconnectSoon(): void {
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.reconnectDelay = 1000; // reset backoff — we have a fresh reason to hope
    // Fire on next tick rather than immediately, so multiple wake signals
    // (visibility + online firing back-to-back) collapse into one attempt.
    this.reconnectTimer = setTimeout(async () => {
      this.reconnectTimer = null;
      try {
        await this.connectWs();
      } catch {
        this.reconnectDelay = Math.min(this.reconnectDelay * 2, 30000);
        this.scheduleReconnect();
      }
    }, 0);
  }

  private connectWs(timeoutMs = 5000): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      if (!this.wsUrl) return reject(new Error('No WebSocket URL'));
      const socket = new WebSocket(this.wsUrl);
      this.ws = socket;

      let opened = false;
      let settled = false;
      let timeout: ReturnType<typeof setTimeout> | null = null;

      const isCurrentSocket = () => this.ws === socket;
      const settleResolve = () => {
        if (settled) return;
        settled = true;
        if (timeout) clearTimeout(timeout);
        resolve();
      };
      const settleReject = (error: Error) => {
        if (settled) return;
        settled = true;
        if (timeout) clearTimeout(timeout);
        reject(error);
      };

      timeout = setTimeout(() => {
        if (socket.readyState === WebSocket.CONNECTING) {
          socket.close();
        }
        settleReject(new Error('WebSocket connection timed out'));
      }, timeoutMs);

      socket.onmessage = (msg) => {
        if (!isCurrentSocket()) return;
        try {
          const data = JSON.parse(msg.data);
          const normalized = normalizeServerEvent(data);
          if (normalized) {
            const subs = this.listeners.get(normalized.event);
            if (subs) subs.forEach((cb) => cb(normalized.payload));
          }
        } catch {
          // ignore malformed messages
        }
      };
      socket.onopen = () => {
        if (!isCurrentSocket() || settled) {
          socket.close();
          return;
        }
        opened = true;
        this.connected = true;
        this.reconnectDelay = 1000; // reset backoff
        this.onConnectionChange?.(true);
        settleResolve();
      };
      socket.onclose = () => {
        if (!isCurrentSocket()) return;
        const wasConnected = this.connected;
        this.connected = false;
        if (wasConnected) {
          this.onConnectionChange?.(false);
        }
        if (!opened) {
          settleReject(new Error('WebSocket connection closed before opening'));
          return;
        }
        this.scheduleReconnect();
      };
      socket.onerror = () => {
        if (!opened) settleReject(new Error('WebSocket connection failed'));
      };
    });
  }

  private scheduleReconnect(): void {
    if (!this.shouldReconnect) return;
    if (this.reconnectTimer) return;
    this.reconnectTimer = setTimeout(async () => {
      this.reconnectTimer = null;
      try {
        await this.connectWs();
      } catch {
        this.reconnectDelay = Math.min(this.reconnectDelay * 2, 30000);
        this.scheduleReconnect();
      }
    }, this.reconnectDelay);
  }

  disconnect(): void {
    this.shouldReconnect = false;
    this.detachLifecycleListeners();
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.ws) {
      this.ws.close();
      this.ws = null;
    }
    this.connected = false;
  }

  isConnected(): boolean {
    return this.connected;
  }

  async invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
    if (this.authExpired) {
      throw new Error('Authentication expired. Please reconnect with a valid token.');
    }

    if (!this.config.baseUrl) {
      throw new Error('Not connected to a server');
    }

    const spec = COMMAND_MAP[command];
    if (!spec) throw new Error(`Unknown command: ${command}`);

    const path = typeof spec.path === 'function' ? spec.path(args ?? {}) : spec.path;
    const url = `${this.config.baseUrl}${path}`;

    const headers: Record<string, string> = {
      'Authorization': `Bearer ${this.config.authToken}`,
    };

    const fetchOpts: RequestInit = { method: spec.method, headers };

    if (spec.argsMode === 'body' && args) {
      headers['Content-Type'] = 'application/json';
      fetchOpts.body = JSON.stringify(spec.transformArgs ? spec.transformArgs(args) : args);
    }

    const resp = await fetch(url, fetchOpts);

    if (!resp.ok) {
      if (resp.status === 401) {
        // Token is invalid or revoked — stop all activity and trigger logout
        this.authExpired = true;
        this.disconnect();
        localStorage.removeItem('atomic-server-config');
        window.dispatchEvent(new CustomEvent('atomic:auth-expired'));
        throw new Error('Authentication expired. Please reconnect with a valid token.');
      }
      const text = await resp.text();
      let errorMsg: string;
      try {
        const errJson = JSON.parse(text);
        const error = errJson.error;
        errorMsg = typeof error === 'string'
          ? error
          : Array.isArray(error?.conflicts)
            ? error.conflicts.join('; ')
            : JSON.stringify(error ?? errJson);
      } catch {
        errorMsg = text;
      }
      throw errorMsg;
    }

    // Some endpoints return no body (204 or empty)
    const contentType = resp.headers.get('content-type') ?? '';
    if (!contentType.includes('json')) {
      return undefined as T;
    }

    const data = await resp.json();
    return (spec.transformResponse ? spec.transformResponse(data) : data) as T;
  }

  subscribe<T>(event: string, callback: (payload: T) => void): () => void {
    if (!this.listeners.has(event)) {
      this.listeners.set(event, new Set());
    }
    const subs = this.listeners.get(event)!;
    subs.add(callback);
    return () => { subs.delete(callback); };
  }
}
