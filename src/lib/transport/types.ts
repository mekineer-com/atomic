export interface Transport {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  subscribe<T>(event: string, callback: (payload: T) => void): () => void;
  connect(): Promise<void>;
  disconnect(): void;
  isConnected(): boolean;
  setDatabaseId(databaseId: string | null): void;
  setWorkspaceMode(enabled: boolean): void;
  readonly mode: 'http';
  onConnectionChange?: (connected: boolean) => void;
}

export interface HttpTransportConfig {
  baseUrl: string;
  authToken: string;
}
