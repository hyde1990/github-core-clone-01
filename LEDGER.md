# Round 10 — Benchmark Optimization Ledger

## Goal (frozen)

Beat the Round 6 local baseline (9-trial mean **994.78**) by a clear margin.
Local ~993 translates to live ~935; the paid threshold is live 958, so a
Round 10 that only matches R6 locally is NOT enough. Promotion gate:
9-trial mean **≥995.00** AND feature 600 + agent 200 in ALL 9 trials AND
kit check clean except `transcripts_present`.

## Phase A — Forensics (recorded vs inferred)

### Recorded (from files + live measurement, 2026-11-20)

- Live entry @witty-condor-29 "Ardent Nucleus" (submission
  `01a1f157-dd33-75ec-8577-7144ca1a6732`, checked 09:05 WIB): score **925**,
  rank **#5007 / 10,229**. Feature 600 + agent 200 maxed; perf only 125,
  live p50 565,885 µs. Paid threshold (rank 20) = **958** (live p50
  358,300 µs); top-60 final cutoff = **954**.
- R6 baseline (9-trial mean **994.78**, geomean p50 ~170,197 µs): push
  50.00 @216.6 ms, clone 45.67 @85.6 ms, read 50.00 @540.2 ms, write
  49.11 @86.4 ms. HEAD 66cb84b. R6 stays the submit candidate.
- R8 semaphore(4): 9-trial mean **993.00** (min 980, max 998), perf
  193.00, geomean p50 177,431 µs; clone 44.56 @92.1 ms. NOT promoted.
  HEAD fbce843.
- R9 boot warmup: 9-trial mean **990.67**. NOT promoted. HEAD b71fdcf.
- R7 worker pool + reactor + dispatch hop: mean 972.00 — FAILED; the
  dispatch hop was the cost, not single-threading.
- Server CPU ~0.15 ms/clone (~0.2% of round wall); pack-cache stampede
  refuted (154 hits / 1 miss); `plan_fetch` 30–46 µs; handler ~48–57 µs;
  handler_max 30–40× handler_avg under ~50 runnable threads; clone wall
  dominated by client-side JS + scheduling noise; run-to-run drift (996.0
  vs 988.67, same binary) exceeds the promotion margin.
- Leaderboard top (09:05 WIB): rank 1 mmo-alpha-agent 970, rank 2
  fighters-gunpla 968 (Rust static forge), rank 3 vinzsenzo 966 (Go). All
  top 10 are static Go/Rust cores. ≥958: 24 entries, ≥950: 169, ≥940:
  1,099.
- Quota (09:05 WIB): entries made 1, day cap 3, taken 0, left 3, resets
  2026-11-21T00:00:00Z, hard limit 20 (left 19), `can_submit: true`.

### Inferred (labelled, not recorded)

- With 50 concurrent clones on the live 2-CPU pod, thread-per-connection
  leaves ~50 threads runnable; CFS scheduling delays + allocator
  contention inflate the tail that sets the round wall. A single thread
  has zero context switches, zero locks, zero handoffs — so the tail
  should fall even though per-request handler time is unchanged.
- Live precedent: rank 9 "Zero-Spin Epoll" 960, rank 15 "single-threaded
  epoll loop" 959 — both single-threaded reactors near the paid threshold.

## Hypothesis (single, frozen for this build)

**H10: single-threaded NON-BLOCKING epoll reactor in pure-std Rust.**
No thread-per-connection, no worker pool, no queue, no dispatch hop. The
ONE reactor thread reads a ready socket, handles the request inline, writes
the response inline. Distinct from R7 (which failed on its dispatch hop).
Target: clone p50 92.1 ms (R8) → ≤75 ms; write/read tails down; push/read
stay saturated. Keep R6/R8-proven caches (pack cache, v0 advertisement
cache). Pure-std Rust, no external crates. Static musl binary ≤8 MiB,
tree ≤16 MiB. `main.ts` is a thin launcher ONLY.

## Variants log

| Variant | Change vs R6 | Result |
| --- | --- | --- |
| R10 (H10) | epoll reactor, single thread, no pool/hop | grading pending |

## Phase B — Rebuild (done 2026-11-20)

- Fresh build dir `~/workspace/makefun/github-core-clone-01-r10`; kit-v27
  init FIRST (anchor `makefun anchor
  76859c77fe14641001dd5c73b66a4fd37e3c2c8b round 34112424`, root commit
  `76859c7`). All 13 Rust modules retyped in-session from the kit spec +
  R8 PORTING.md as design reference; no file copied from r2–r9, -opt, or
  the live repo.
- `http.rs` rewritten: epoll via raw syscalls (`epoll_create1` 291,
  `epoll_ctl` 233, `epoll_wait` 232, `core::arch::asm!`), level-triggered,
  non-blocking sockets, inline handling, partial-write buffering with
  EPOLLOUT arming, 100-continue interim, pipelining, 64 KiB header /
  64 MiB body caps, 4096-connection cap. `Request`/`Response` API kept
  identical to R8.
- `mcp.rs`: same tool surface as R8, plus suffix matching for a server
  prefix (`github__create_issue`, `server/create_issue`) per the kit
  spec (R8 only stripped `/` prefixes).
- Static musl binary: 791,336 bytes, static-pie, stripped. `cargo test
  --release` 24/24, `cargo clippy --all-targets -- -D warnings` clean,
  `cargo fmt --check` clean. Reactor smoke: `/healthz` → `{"ok":true}`,
  `/api/v3/user` → viewer `adaiversen` id 1000.

## Phase C — Verify (in progress)

- Kit smoke (kit-v27 `sdk/run.ts`, fresh entry): pending.
- Kit check: pending (expect only `transcripts_present` failing).
- Grade-local 9 trials (3 runs × `--instances 3`): pending.

### Grading table (fill after grading)

| Trial | Total | Feature | Agent | Perf | push | clone | read | write | geomean p50 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| (pending) | | | | | | | | | |

R6 baseline for comparison: mean 994.78 — push 50.00 @216.6 ms, clone
45.67 @85.6 ms, read 50.00 @540.2 ms, write 49.11 @86.4 ms.

## Decision

PENDING grading. Promote only if the gate is met (9-trial mean ≥995.00,
feature 600 + agent 200 in all 9 trials, kit check clean except
`transcripts_present`). Otherwise NOT promoted; R6 stays the submit
candidate. NO submit, NO push in this round.

## Hard constraints

- Node 22 only at runtime; no dependencies, no internet, no `git` binary
  in the trial; repo mounted read-only at `/bot`, writable under `/tmp`
  only; 900 s wall budget; static binary ≤8 MiB/file, 16 MiB tree.
- Entries close 2026-11-21 06:30 UTC (13:30 WIB 2026-11-21).
