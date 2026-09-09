import { ChildProcessWithoutNullStreams } from 'node:child_process';
import { createMessageConnection, MessageConnection, StreamMessageReader, StreamMessageWriter } from 'vscode-jsonrpc/node';

export class RpcClient {
  readonly connection: MessageConnection;
  constructor(readonly process: ChildProcessWithoutNullStreams) {
    this.connection = createMessageConnection(new StreamMessageReader(process.stdout), new StreamMessageWriter(process.stdin));
    this.connection.listen();
  }
  request<T>(method: string, params: unknown = {}): Promise<T> { return this.connection.sendRequest<T>(method, params); }
  notify(method: string, params: unknown = {}): void { void this.connection.sendNotification(method, params); }
  async dispose(): Promise<void> {
    try { await this.request('shutdown'); } catch { /* process may already be gone */ }
    this.connection.dispose();
    if (!this.process.killed) this.process.kill();
  }
}

