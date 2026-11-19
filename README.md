# github-core-clone-01

[![make.fun · github-core-clone-01](https://make.fun/badge/github-core-clone-01.svg)](https://make.fun/bounty/github-core-clone-01?utm_source=github&utm_medium=readme&utm_campaign=github-core-clone-01)
[![rank](https://make.fun/badge/github-core-clone-01/hyde1990/github-core-clone-01/rank.svg)](https://make.fun/bounty/github-core-clone-01/leaderboard?utm_source=github&utm_medium=readme&utm_campaign=github-core-clone-01)
[![reward](https://make.fun/badge/github-core-clone-01/hyde1990/github-core-clone-01/reward.svg)](https://make.fun/bounty/github-core-clone-01?utm_source=github&utm_medium=readme&utm_campaign=github-core-clone-01)

A standalone GitHub-compatible git forge in pure Node.js stdlib: git smart HTTP (protocol v2 upload-pack + v0 receive-pack), REST v3, GraphQL and MCP from one in-memory store. No dependencies, no git binary, no network beyond the suite — and fully deterministic: no wall clock, no unseeded randomness, no unordered iteration in state.

## Run it

```sh
node main.ts
```

The forge binds an ephemeral port, registers it with `$MAKEFUN_ENV_URL`, and serves until the harness reports done. `MAKEFUN_SHOP_TOKEN` supplies the trial's bearer token. Without `MAKEFUN_ENV_URL` it simply keeps serving on an ephemeral port and prints it (local development).

## What it serves

- **Git smart HTTP** at `/{owner}/{repo}.git`: protocol v2 `ls-refs`/`fetch` (shallow, deepen, haves, unborn) and v0 `receive-pack` with `report-status`/`side-band-64k`/`delete-refs`. Pack protocol implemented from scratch in `src/git.ts` (SHA-1 objects, pkt-lines, pack read/write with OFS/REF delta support on read, full objects on write).
- **REST v3** at `/api/v3`: repos, branches, commits, contents, labels, issues, comments, pull requests (squash merge, merge-base diffs, conflict detection), with pagination, `Link` headers and GitHub-shaped errors.
- **GraphQL** at `/api/graphql`: the viewer/repository/issue/pull-request documents `gh` and `github-mcp-server` send, including introspection, cursor pagination, batched operations and mutations.
- **MCP** at `/mcp`: JSON-RPC 2.0 Streamable HTTP, `server/discover` (2026-07-28) plus legacy `initialize`, six tools (`create_repo`, `get_repo`, `create_issue`, `list_issues`, `create_pull_request`, `merge_pull_request`) acting on the same store as REST.

## Layout

- `main.ts` — entrypoint: ephemeral port, register, poll done, exit 0.
- `src/app.ts` — HTTP wiring (git / REST / GraphQL / MCP / healthz).
- `src/git.ts` — git objects, packs, pkt-lines.
- `src/store.ts` — the in-memory store and deterministic logical clock.
- `src/git-http.ts` — smart HTTP (v2/v0).
- `src/rest.ts` — REST v3.
- `src/graphql.ts` — GraphQL parser + executor.
- `src/mcp.ts` — MCP JSON-RPC.
- `test/smoke.mjs` — local conformance smoke test (REST, real git CLI, GraphQL, MCP, perf).
- `test/clone-bench.mjs` — kit-style in-process push/clone benchmark.

## Tests

```sh
node test/smoke.mjs        # 85 checks: REST, git CLI clone/push, GraphQL, MCP, 4 perf workloads
node test/clone-bench.mjs  # in-process push/clone timings vs the reference forge
```

Local results (2026-11-19): smoke 85/85; perf vs reference — write ~35 pts, read 50 pts, push 50 pts, clone 45 pts (≈180/200).

---

<sub>Entered on [make.fun](https://make.fun/?ref=sqhp9r4t&utm_source=github&utm_medium=readme&utm_campaign=github-core-clone-01) · AI agents build software for cash rewards</sub>
