// http-util.ts — small HTTP helpers shared by every surface: body reading and
// JSON / buffer answers. Every reply carries a non-empty X-GitHub-Request-Id.

import type { IncomingMessage, ServerResponse } from 'node:http';

let requestCounter = 0;

export function nextRequestId(): string {
  requestCounter += 1;
  return `req-${requestCounter.toString(16).padStart(8, '0')}`;
}

const MAX_BODY_BYTES = 64 * 1024 * 1024;

/** Read a request body whole (bounded); rejects on overflow. */
export function readBody(req: IncomingMessage): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    let size = 0;
    req.on('data', (chunk: Buffer) => {
      size += chunk.length;
      if (size > MAX_BODY_BYTES) {
        reject(new Error('request body too large'));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on('end', () => resolve(Buffer.concat(chunks)));
    req.on('error', reject);
  });
}

export function sendJson(
  res: ServerResponse,
  status: number,
  body: unknown,
  headers: Readonly<Record<string, string>> = {},
): void {
  const text = `${JSON.stringify(body)}\n`;
  res.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'content-length': Buffer.byteLength(text),
    'x-github-request-id': nextRequestId(),
    ...headers,
  });
  res.end(text);
}

export function sendBuffer(
  res: ServerResponse,
  status: number,
  body: Buffer,
  contentType: string,
  headers: Readonly<Record<string, string>> = {},
): void {
  res.writeHead(status, {
    'content-type': contentType,
    'content-length': body.length,
    'x-github-request-id': nextRequestId(),
    ...headers,
  });
  res.end(body);
}

/** Parse a JSON body; returns null (and the error) when it is not JSON. */
export function parseJsonBody(body: Buffer): { value: unknown; error: string | null } {
  if (body.length === 0) return { value: undefined, error: null };
  try {
    return { value: JSON.parse(body.toString('utf8')) as unknown, error: null };
  } catch (err) {
    return { value: undefined, error: err instanceof Error ? err.message : String(err) };
  }
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}
