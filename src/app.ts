// app.ts — HTTP wiring: one node:http server serving git smart HTTP, REST v3,
// GraphQL and MCP from the same in-memory store. Every request is wrapped so a
// handler can never throw into the socket: a catch-all answers 500 JSON and the
// process keeps serving.

import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http';
import { handleInfoRefs, handleReceivePack, handleUploadPack } from './git-http.ts';
import { handleRest } from './rest.ts';
import { handleGraphQL } from './graphql.ts';
import { handleMcp } from './mcp.ts';
import { nextRequestId, readBody, sendJson } from './http-util.ts';

const GIT_ROUTE = /^\/([^/]+)\/([^/]+)\.git\/(info\/refs|git-upload-pack|git-receive-pack)$/u;

export function createApp(): Server {
  return createServer((req: IncomingMessage, res: ServerResponse) => {
    void handleRequest(req, res).catch((err: unknown) => {
      try {
        sendJson(res, 500, { message: err instanceof Error ? err.message : 'Internal Server Error' });
      } catch {
        try {
          res.end();
        } catch {
          // Socket already gone; nothing left to do.
        }
      }
    });
  });
}

async function handleRequest(req: IncomingMessage, res: ServerResponse): Promise<void> {
  const rawUrl = req.url ?? '/';
  const url = new URL(rawUrl, 'http://127.0.0.1');
  const method = (req.method ?? 'GET').toUpperCase();
  const pathname = url.pathname;

  if (pathname === '/healthz') {
    sendJson(res, 200, { ok: true });
    return;
  }

  const body = method === 'GET' || method === 'HEAD' ? Buffer.alloc(0) : await readBody(req);

  const gitMatch = GIT_ROUTE.exec(pathname);
  if (gitMatch !== null) {
    const owner = decodeURIComponent(gitMatch[1]!);
    const repoName = decodeURIComponent(gitMatch[2]!);
    const service = gitMatch[3]!;
    if (service === 'info/refs') {
      handleInfoRefs(req, res, owner, repoName, url.searchParams.get('service'));
      return;
    }
    if (service === 'git-upload-pack') {
      handleUploadPack(req, res, owner, repoName, body);
      return;
    }
    handleReceivePack(req, res, owner, repoName, body);
    return;
  }

  if (pathname === '/api/graphql') {
    handleGraphQL(req, res, body);
    return;
  }

  if (pathname === '/mcp') {
    handleMcp(req, res, body);
    return;
  }

  if (pathname === '/api/v3' || pathname.startsWith('/api/v3/')) {
    const restPath = pathname.slice('/api/v3'.length);
    const segments = restPath.split('/').filter((part) => part !== '').map((part) => decodeURIComponent(part));
    handleRest({ method, segments, query: url.searchParams, body, req, res });
    return;
  }

  res.writeHead(404, { 'content-type': 'application/json', 'x-github-request-id': nextRequestId() });
  res.end(JSON.stringify({ message: 'Not Found' }));
}
