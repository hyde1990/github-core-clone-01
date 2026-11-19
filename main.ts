// main.ts — entrypoint for the github-core-clone-01 forge.
//
// Lifecycle (per RULES.txt):
//   1. bind an ephemeral port (port 0, never a fixed one);
//   2. POST {"port": N} to $MAKEFUN_ENV_URL/register so the suite can reach us;
//   3. poll GET $MAKEFUN_ENV_URL/done until it answers {"done": true}, then exit 0.
// The process is one Node process serving git smart HTTP, REST v3, GraphQL and
// MCP from a single in-memory store. When MAKEFUN_ENV_URL is unset (local
// development) the server simply keeps serving on an ephemeral port and prints it.

import { createApp } from './src/app.ts';

const envUrl = process.env.MAKEFUN_ENV_URL ?? '';

async function postJson(url: string, payload: unknown): Promise<unknown> {
  const response = await fetch(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(payload),
  });
  const text = await response.text();
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return text;
  }
}

async function waitForDone(): Promise<void> {
  for (;;) {
    await new Promise((resolve) => setTimeout(resolve, 500));
    try {
      const response = await fetch(`${envUrl}/done`);
      const payload = (await response.json()) as { done?: boolean };
      if (payload.done === true) return;
    } catch {
      // The env is not up yet; keep polling.
    }
  }
}

async function run(): Promise<void> {
  const server = createApp();
  const port = await new Promise<number>((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const address = server.address();
      if (address === null || typeof address === 'string') {
        reject(new Error('could not bind an ephemeral port'));
        return;
      }
      resolve(address.port);
    });
  });

  if (envUrl === '') {
    // Local run: no harness to register with. Keep serving.
    process.stdout.write(`github-core-clone listening on 127.0.0.1:${port}\n`);
    return;
  }

  await postJson(`${envUrl}/register`, { port });
  await waitForDone();
  server.close();
  process.exit(0);
}

run().catch((err: unknown) => {
  process.stderr.write(`fatal: ${err instanceof Error ? err.message : String(err)}\n`);
  process.exit(1);
});
