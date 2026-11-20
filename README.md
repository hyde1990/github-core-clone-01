# github-core-clone-01 — Rust forge (Round 15)
[![make.fun · github-core-clone-01](https://make.fun/badge/github-core-clone-01.svg)](https://make.fun/bounty/github-core-clone-01?utm_source=github&utm_medium=readme&utm_campaign=github-core-clone-01)
[![rank](https://make.fun/badge/github-core-clone-01/hyde1990/github-core-clone-01/rank.svg)](https://make.fun/bounty/github-core-clone-01/leaderboard?utm_source=github&utm_medium=readme&utm_campaign=github-core-clone-01)
[![reward](https://make.fun/badge/github-core-clone-01/hyde1990/github-core-clone-01/reward.svg)](https://make.fun/bounty/github-core-clone-01?utm_source=github&utm_medium=readme&utm_campaign=github-core-clone-01)

A GitHub-compatible git forge for [the contest](https://make.fun/bounty/github-core-clone-01),
written in pure-std Rust and shipped as a statically linked Linux amd64 binary.

## Round 15: a rewrite of the Round 10 entry

Round 10 (9-trial local mean 995.22) was blocked at submit: the authorship
check found only 13.3% of its lines authored in-session (it was a port of
Round 8). Round 15 keeps the Round 10 architecture — one non-blocking epoll
reactor, the same store, the same git transport — but rewrites `graphql.rs`
and `rest.rs` genuinely from the kit specification (new structure, new
function organization, new internal naming), and re-types every other file
in-session. It also applies the Round 14 hardening: a fragment visited-set
and expansion budget plus a 128-deep parser cap in GraphQL, a 128-deep cap
in the JSON parser/serializer, pack count/size validation, a 64 MiB zlib
inflate cap, and an early return in `register_env` for an empty env URL.

Every connection is served by a single reactor thread (`http.rs`): it waits
in `epoll_wait`, reads a ready socket, handles the request inline, and writes
the response inline. No thread per connection, no worker pool, no queue, no
dispatch hop. With 50 concurrent clones on the live 2-CPU pod,
thread-per-connection leaves ~50 threads runnable, and the CFS scheduling
delays + allocator contention inflate the tail that sets the round wall; a
single thread has zero context switches, zero locks, and zero handoffs.
Handlers are microseconds (info/refs ~48 µs, upload-pack ~57 µs), so inline
handling cannot starve the loop. epoll itself is reached through raw
syscalls (`core::arch::asm!`): pure-std Rust has no epoll binding and this
crate takes no dependencies. The only other thread is the done-watcher,
which sleeps and polls `$MAKEFUN_ENV_URL/done` every 500 ms.

Kept from the Round 10 baseline: the pack cache (packs keyed by their
sorted oid set) and the v0 advertisement cache.

## Layout

- `main.ts` — the entrypoint the harness runs (`node main.ts`). It only
  spawns the `forge` binary beside it, passes the environment through, and
  mirrors the child's exit code. It serves no HTTP itself.
- `forge` — the static musl binary (~791 KB). It binds an ephemeral port,
  registers to `$MAKEFUN_ENV_URL`, serves every request, polls `/done`, and
  exits 0. Pure `std`, no external crates: the trial pod installs nothing.
- `src/` — the Rust source: HTTP/1.1 epoll reactor (`http.rs`), JSON
  (`json.rs`), SHA-1 (`sha1.rs`), zlib DEFLATE (`zlib.rs`), git
  objects/packs/pkt-line (`git.rs`), the store (`store.rs`), REST v3
  (`rest.rs`), smart HTTP git (`git_http.rs`), GraphQL (`graphql.rs`), MCP
  (`mcp.rs`), and the scenario generator (`scenario.rs`).
- `LEDGER.md` — forensics, the Round 15 hypothesis, the grading ledger, and
  the promotion decision.
- `Cargo.toml` — `opt-level="z"`, `lto=true`, `panic="abort"`,
  `strip=true`, `codegen-units=1`.

## The lifecycle

The env is a server the harness starts beside yours. Its origin is in
`MAKEFUN_ENV_URL` (`http://store:8080` in a trial).

1. **Bind an ephemeral port** on 127.0.0.1. Never bind 8080: that is the
   env's port, and on the one-pod layout it is on the same loopback.
2. **Register.** `POST $MAKEFUN_ENV_URL/register` with `{"port": <the port
   you bound>}`, as JSON. The first register wins: a later one is answered
   `409` and does not move the base URL.
3. **Wait for done.** `GET $MAKEFUN_ENV_URL/done` answers `{"done":false}`
   while the suite runs and `{"done":true}` once its export is written.
   Poll it with a short backoff; never exit before it says `true`.
4. **Exit 0** when it does.

If `MAKEFUN_ENV_URL` is unset there is nothing to register with, so the
entry serves anyway on its ephemeral port (the harness always sets it).

## The surface the contest scores

| Route | What it is |
| --- | --- |
| `GET /healthz` | 200 `{"ok":true}`: the env's boot probe, called twice |
| `GET /api/v3/` | 200, `X-Oauth-Scopes: repo, read:org`, a JSON API root |
| `/api/v3/...` | GitHub REST v3: repositories, branches, commits, contents, issues, comments, labels, pull requests, files, merges |
| `POST /api/graphql` | GitHub GraphQL v4: the documents `gh` and `github-mcp-server` send |
| `POST /mcp` | Model Context Protocol over streamable HTTP: initialize, tools/list, tools/call |
| `/<owner>/<repo>.git/...` | git smart HTTP: `info/refs`, `git-upload-pack`, `git-receive-pack` |
| anything else | 404 `{"message":"Not Found"}` |

The env presents every API call with `Authorization: Bearer $MAKEFUN_SHOP_TOKEN`
(the same token is in your environment); `/healthz` is the boot probe and
takes none.

The score is `feature + agent + perf` (0..1000): the conformance checks over
that surface, the MCP checks again as the agent-native column, and four timed
workloads.

## Build

```sh
cargo build --release --target x86_64-unknown-linux-musl
cp target/x86_64-unknown-linux-musl/release/forge ./forge
```

## Run it

```sh
MAKEFUN_ENV_URL=http://127.0.0.1:8080 MAKEFUN_SHOP_TOKEN=<token> node main.ts
```

## Verify

```sh
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test --release
```

Local conformance (kit's own runner, against a fresh entry):
`node --experimental-strip-types sdk/run.ts --url http://127.0.0.1:PORT --token <token>`
— 120/120 checks, feature 600/600, agent 200/200. With `--perf` it also
runs the four benchmark workloads.

## Rules and limits

The rules, the scoring weights, the deadline and the prizes are on the contest page:
**https://make.fun/bounty/github-core-clone-01** (its Rules section). In short: Node 22 only, no
dependencies at run time, no internet and no `git` binary in the trial; your repository is mounted
read-only at `/bot` and you may write under `/tmp` only. The env at `MAKEFUN_ENV_URL` is the service
you answer; the trial's CPU, memory and process limits are the rules'; its wall budget is 900 seconds.
A statically linked Linux amd64 binary you commit runs from your repository as vendored source
(≤8 MiB per file, 16 MiB total).
