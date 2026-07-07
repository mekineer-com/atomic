import type { Transport, HttpTransportConfig } from './types';
import { HttpTransport } from './http';
import { useUIStore } from '../../stores/ui';
import { syncSharedConfig, clearSharedConfig } from '../mobile/shared-config';
export type { Transport, HttpTransportConfig };

let activeTransport: Transport | null = null;
let localServerConfig: HttpTransportConfig | null = null;

export const TRANSPORT_CHANGED_EVENT = 'atomic:transport-changed';
export const TRANSPORT_CONNECTION_EVENT = 'atomic:transport-connection';

function dispatchTransportChanged(): void {
  if (typeof window === 'undefined') return;
  window.dispatchEvent(new CustomEvent(TRANSPORT_CHANGED_EVENT));
}

function dispatchTransportConnection(connected: boolean): void {
  if (typeof window === 'undefined') return;
  window.dispatchEvent(new CustomEvent(TRANSPORT_CONNECTION_EVENT, {
    detail: { connected },
  }));
}

function wireConnectionCallback(transport: Transport): void {
  (transport as HttpTransport).onConnectionChange = (connected) => {
    useUIStore.getState().setServerConnected(connected);
    dispatchTransportConnection(connected);
  };
}

function connectInBackground(transport: Transport): void {
  void transport.connect().catch((err) => {
    console.error('Transport connection failed:', err);
  });
}

export function getTransport(): Transport {
  if (!activeTransport) throw new Error('Transport not initialized. Call initTransport() first.');
  return activeTransport;
}

export function hasServerConfig(): boolean {
  if (!activeTransport) return false;
  return Boolean((activeTransport as HttpTransport).getConfig().baseUrl);
}

export async function initTransport(): Promise<void> {
  if (typeof window !== 'undefined' && (window as any).__TAURI_INTERNALS__) {
    // Desktop app: get sidecar config via single Tauri IPC command
    const { invoke } = await import('@tauri-apps/api/core');
    localServerConfig = await invoke<HttpTransportConfig>('get_local_server_config');

    // Check if user has saved a remote server config
    const saved = localStorage.getItem('atomic-server-config');
    const config = saved ? JSON.parse(saved) as HttpTransportConfig : localServerConfig;

    activeTransport = new HttpTransport(config);
    wireConnectionCallback(activeTransport);
    connectInBackground(activeTransport);
  } else {
    // Web SPA — OpenAlma local launch can provide its server config via Vite env.
    const saved = localStorage.getItem('atomic-server-config');
    if (import.meta.env.VITE_ATOMIC_SERVER_URL && import.meta.env.VITE_ATOMIC_AUTH_TOKEN) {
      const config: HttpTransportConfig = {
        baseUrl: import.meta.env.VITE_ATOMIC_SERVER_URL.replace(/\/$/, ''),
        authToken: import.meta.env.VITE_ATOMIC_AUTH_TOKEN,
      };
      activeTransport = new HttpTransport(config);
      wireConnectionCallback(activeTransport);
      connectInBackground(activeTransport);
      void syncSharedConfig({ serverURL: config.baseUrl, apiToken: config.authToken });
    } else if (saved) {
      const config: HttpTransportConfig = JSON.parse(saved);
      activeTransport = new HttpTransport(config);
      wireConnectionCallback(activeTransport);
      connectInBackground(activeTransport);
      void syncSharedConfig({ serverURL: config.baseUrl, apiToken: config.authToken });
    } else {
      // Create a disconnected HttpTransport — user must configure via settings
      activeTransport = new HttpTransport({ baseUrl: '', authToken: '' });
    }
  }
}

/// Switch to a remote server (saves config to localStorage)
export async function switchTransport(config: HttpTransportConfig): Promise<void> {
  if (activeTransport) activeTransport.disconnect();
  activeTransport = new HttpTransport(config);
  wireConnectionCallback(activeTransport);
  await activeTransport.connect();
  localStorage.setItem('atomic-server-config', JSON.stringify(config));
  void syncSharedConfig({ serverURL: config.baseUrl, apiToken: config.authToken });
  dispatchTransportChanged();
}

/// Switch back to the local sidecar server (desktop only)
export async function switchToLocal(): Promise<void> {
  if (!localServerConfig) {
    throw new Error('No local server config available — not running in desktop app');
  }
  if (activeTransport) activeTransport.disconnect();
  activeTransport = new HttpTransport(localServerConfig);
  wireConnectionCallback(activeTransport);
  await activeTransport.connect();
  localStorage.removeItem('atomic-server-config');
  void clearSharedConfig();
  dispatchTransportChanged();
}

/// True when running inside the Tauri desktop app (sidecar available)
export function isDesktopApp(): boolean {
  return localServerConfig !== null;
}

/// True when connected to the embedded local sidecar (not a remote server)
export function isLocalServer(): boolean {
  if (!localServerConfig || !activeTransport) return false;
  const currentConfig = (activeTransport as HttpTransport).getConfig();
  return currentConfig.baseUrl === localServerConfig.baseUrl;
}

/// Get the local server config (for MCP setup display, etc.)
export function getLocalServerConfig(): HttpTransportConfig | null {
  return localServerConfig;
}

/// Get the resolved path to the bundled atomic-mcp-bridge binary (desktop only).
export async function getMcpBridgePath(): Promise<string | null> {
  if (!isDesktopApp()) return null;
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    return await invoke<string>('get_mcp_bridge_path');
  } catch {
    return null;
  }
}
