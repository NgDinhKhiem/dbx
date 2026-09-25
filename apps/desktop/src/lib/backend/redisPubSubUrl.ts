/** URL of the desktop app's local Redis PubSub WebSocket, carrying the per-launch token. */
export function redisPubSubWebSocketUrl(endpoint: { port: number; token: string }, connectionId: string, monitor = false): string {
  const params = new URLSearchParams({ connectionId, monitor: String(monitor), token: endpoint.token });
  return `ws://127.0.0.1:${endpoint.port}/api/redis/pubsub/ws?${params.toString()}`;
}
