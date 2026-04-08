import { createServer, type Server, type IncomingMessage, type ServerResponse } from 'http';

interface MockServerResult {
  url: string;
  stop: () => Promise<void>;
  server: Server;
}

const GID = '2089b05ecca3d829';

const METHOD_HANDLERS: Record<string, (params: unknown[]) => unknown> = {
  'aria2.addUri': () => GID,
  'aria2.addTorrent': () => GID,
  'aria2.addMetalink': () => GID,
  'aria2.remove': (params) => String(params[0]),
  'aria2.pause': (params) => String(params[0]),
  'aria2.unpause': (params) => String(params[0]),
  'aria2.forcePause': (params) => String(params[0]),
  'aria2.forceRemove': (params) => String(params[0]),
  'aria2.forceUnpause': (params) => String(params[0]),
  'aria2.tellStatus': (params) => ({
    gid: String(params[0]),
    status: 'active',
    totalLength: '34896136',
    completedLength: '0',
    downloadSpeed: '0',
    uploadSpeed: '0',
  }),
  'aria2.tellActive': () => [
    {
      gid: GID,
      status: 'active',
      totalLength: '34896136',
      completedLength: '0',
      downloadSpeed: '0',
      uploadSpeed: '0',
    },
  ],
  'aria2.tellWaiting': () => [
    {
      gid: 'waiting1',
      status: 'waiting',
      totalLength: '1024',
      completedLength: '0',
      downloadSpeed: '0',
      uploadSpeed: '0',
    },
  ],
  'aria2.tellStopped': () => [
    {
      gid: 'stopped1',
      status: 'complete',
      totalLength: '1024',
      completedLength: '1024',
      downloadSpeed: '0',
      uploadSpeed: '0',
    },
  ],
  'aria2.getGlobalStat': () => ({
    downloadSpeed: '20480',
    uploadSpeed: '0',
    numActive: '1',
    numWaiting: '0',
    numStopped: '0',
    numStoppedTotal: '0',
  }),
  'aria2.getVersion': () => ({
    version: '1.37.0',
    enabledFeatures: ['Async DNS', 'BitTorrent', 'Firefox3 Cookie'],
  }),
  'aria2.getSessionInfo': () => ({
    sessionId: 'abc123',
  }),
  'aria2.getGlobalOption': () => ({
    'max-overall-download-limit': '0',
    'max-overall-upload-limit': '0',
  }),
  'aria2.changeGlobalOption': () => 'OK',
  'aria2.getOption': () => ({
    dir: '/tmp',
    'max-download-limit': '0',
  }),
  'aria2.changeOption': () => 'OK',
  'aria2.purgeDownloadResult': () => 'OK',
  'aria2.removeDownloadResult': () => 'OK',
  'aria2.shutdown': () => 'OK',
  'aria2.forceShutdown': () => 'OK',
  'aria2.saveSession': () => 'OK',
};

export interface MockServerOptions {
  token?: string;
}

export async function startMockServer(
  options: MockServerOptions = {},
): Promise<MockServerResult> {
  const server = createServer(
    (req: IncomingMessage, res: ServerResponse) => {
      if (req.method !== 'POST') {
        res.writeHead(405);
        res.end();
        return;
      }

      let body = '';
      req.on('data', (chunk: Buffer) => {
        body += chunk.toString();
      });

      req.on('end', () => {
        let parsed: Record<string, unknown>;
        try {
          parsed = JSON.parse(body);
        } catch {
          res.writeHead(400, { 'Content-Type': 'application/json' });
          res.end(JSON.stringify({ jsonrpc: '2.0', error: { code: -32700, message: 'Parse error' }, id: null }));
          return;
        }

        const method = parsed.method as string;
        const params = (parsed.params as unknown[]) ?? [];
        const id = parsed.id;

        if (options.token) {
          const firstParam = params[0];
          if (firstParam !== `token:${options.token}`) {
            res.writeHead(200, { 'Content-Type': 'application/json' });
            res.end(
              JSON.stringify({
                jsonrpc: '2.0',
                error: { code: 1, message: 'Unauthorized' },
                id,
              }),
            );
            return;
          }
        }

        const handler = METHOD_HANDLERS[method];
        if (!handler) {
          res.writeHead(200, { 'Content-Type': 'application/json' });
          res.end(
            JSON.stringify({
              jsonrpc: '2.0',
              error: { code: -32601, message: 'Method not found' },
              id,
            }),
          );
          return;
        }

        try {
          const actualParams = options.token ? params.slice(1) : params;
          const result = handler(actualParams);
          res.writeHead(200, { 'Content-Type': 'application/json' });
          res.end(JSON.stringify({ jsonrpc: '2.0', result, id }));
        } catch (err: unknown) {
          res.writeHead(200, { 'Content-Type': 'application/json' });
          res.end(
            JSON.stringify({
              jsonrpc: '2.0',
              error: { code: 1, message: err instanceof Error ? err.message : String(err) },
              id,
            }),
          );
        }
      });
    },
  );

  return new Promise((resolve) => {
    server.listen(0, () => {
      const addr = server.address();
      const port = typeof addr === 'object' && addr ? addr.port : 6800;
      resolve({
        url: `http://localhost:${port}/jsonrpc`,
        stop: () =>
          new Promise<void>((res, rej) => {
            server.close((err) => (err ? rej(err) : res()));
          }),
        server,
      });
    });
  });
}
