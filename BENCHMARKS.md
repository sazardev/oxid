# Benchmarks

Numbers behind the [landing page](https://sazardev.github.io/oxid/#benchmarks). Everything
here is reproducible from this repository; the method is written out so the
figures can be argued with rather than taken on faith.

## The rig

| | |
|---|---|
| CPU | AMD Ryzen 5 3600 — 6 cores / 12 threads, 4.2 GHz boost |
| Memory | 16 GB |
| Storage | NVMe SSD, btrfs |
| OS | Linux 7.2, Docker Engine 29.7.2 |
| Toolchain | Rust 1.98 — `debug` profile for the HTTP figures, the released static musl binary for the deploy one |

The machine was **not** idle: it ran its owner's usual containers throughout. That
adds noise, so it applies equally to both halves of every comparison — and it is
closer to a real self-hosted node than a quiet lab box would be.

Storage matters more than it looks. An earlier run of this suite lived under
`/tmp`, which is `tmpfs` on this machine — the database was in RAM and the
numbers flattered every write. The results below are on the NVMe.

## The database

A node keeps one row per deploy as history, so the tables the hot paths read
grow with use. The fixture is a team a few months in, not a fresh install:

- 6,000 environments (300 of them `running`)
- 30,000 audit events
- 40 projects

## Method

- **Two real binaries**, built from the commit before the work and the commit
  after — not one build behind a flag.
- **Each daemon measured alone.** Running both at once made them compete for
  CPU and produced a difference that vanished when they were separated; that
  false result is why this is spelled out.
- **Median of five runs after a warmup**, per data point — three for the deploy figure, which costs about a minute per run.
- Load generated over keep-alive HTTP connections, so the figures measure the
  daemon rather than connection setup or a process spawn per request.

## Results

### Deploy throughput

Fifteen branches pushed simultaneously at one node — the shape of a team
starting its morning, and the number the whole exercise was about. Real
signed GitHub webhooks against a real repository, the full Docker + Traefik
stack, measured from the first webhook to the last environment leaving
`building`.

| | Before | After | |
|---|---|---|---|
| 15 simultaneous pushes, default settings | 27.3 s | 7.1 s | 3.8× |
| …with `OXID_DEPLOY_CONCURRENCY=16` | — | 4.2 s | 6.5× |

Same host, same warm Docker layer cache, runs alternated: before 23.3 / 28.3
/ 27.3 s; after 7.1 / 7.1 / 8.1 s; at concurrency 16, twelve runs with a
median of 4.2 s.

Three separate things were in the way, each hiding the next:

- **Every deploy on the node held one mutex.** Sibling branches share no
  checkout, no container name and no environment row, so they had no reason
  to queue behind one another. Locking is now keyed by what is actually
  shared.
- **A burst waited out a scheduler tick doing nothing.** Webhooks arrive
  faster than a drain can read the queue, so most of them landed after its
  snapshot and were told a drain was already running — true, but that drain
  had already read past them. Sixteen of the first measurement's
  twenty-eight seconds were that idle wait.
- **Every deploy did its own `git fetch`, one at a time.** This one was
  invisible until the other two were gone, and it is worth showing how it
  was found. With the concurrency cap raised from 4 to 16 the burst barely
  improved — 8.1 s to 7.2 s — which rules out the cap. The daemon's own log
  said why: all fourteen deploys started in *the same millisecond* and then
  finished spaced 425 ms apart, like clockwork. A `git fetch` against that
  repository from this machine measures 402–428 ms. They were queueing for
  the network, under the lock that protects the shared checkout. A fetch
  brings down every branch at once, so the first one had already retrieved
  what the other fourteen were about to ask for. They now share it.

Read this as a floor, not a ceiling — and note which way. The layer cache
was warm, so each build here is a second or two: that *understates* the gain
on cold builds, where whole builds overlap rather than the bookkeeping
around them. It also means the remaining seconds are now mostly real work
(container start, readiness), which is where they belong.

### Heartbeat

The proxy calls this on every HTTP request to every environment. It resolves a
hostname to its environment and records the visit.

| Concurrent callers | Before | After | p50 latency | |
|---|---|---|---|---|
| 1 | 242 req/s | 848 req/s | 3.9ms → 0.9ms | 3.5× |
| 8 | 288 req/s | 3,382 req/s | 26.8ms → 2.0ms | 11.7× |
| 32 | 236 req/s | 4,763 req/s | 125.5ms → 5.7ms | 20.2× |
| 64 | 262 req/s | 4,592 req/s | 233.7ms → 11.7ms | 17.5× |

Throughput used to be flat from 1 to 64 callers while latency climbed in
lockstep — the shape of one serialized resource rather than a busy one.

### Authenticated read (`GET /api/v1/audit?limit=50`)

| Concurrent callers | Before | After | p50 latency | |
|---|---|---|---|---|
| 1 | 283 req/s | 270 req/s | 3.2ms → 3.4ms | unchanged |
| 8 | 356 req/s | 1,044 req/s | 22.0ms → 7.1ms | 2.9× |
| 32 | 366 req/s | 1,267 req/s | 85.9ms → 24.2ms | 3.5× |

A single caller has nothing to overlap with, so it gains nothing. That row is
kept rather than dropped.

### Where it came from

Separated by re-running the *new* build with `OXID_DB_MAX_CONNECTIONS=1`, at 32
concurrent callers:

| | Heartbeat | Read |
|---|---|---|
| Before | 236 req/s | 366 req/s |
| New build, one connection (index + write coalescing) | 1,651 req/s | 387 req/s |
| New build, pool of 8 | 4,763 req/s | 1,267 req/s |

The index and the write change ship together and were measured together; this
experiment cannot split them. For reads they do nothing at all — the entire
gain there is the pool, which is what one would expect, since the audit query
never touches the column that was indexed.

### Pool size

Single runs rather than medians, so read them as a shape and not as precise
figures. Read throughput at 32 concurrent callers: 311 req/s at one connection,
688 at two, 1,087 at four, 1,185 at eight, 1,356 at sixteen. Eight is the
default because the returns flatten and each connection carries its own page
cache; `OXID_DB_MAX_CONNECTIONS` moves it.

## Durability under concurrent writes

64 concurrent writers across 16 rows completed 4,800 writes with no
`SQLITE_BUSY` and no errors in the daemon log. Writes still serialize — that is
SQLite, not a setting — but WAL keeps them from blocking readers, and
`busy_timeout` absorbs the contention rather than failing a request over it.

## Fleet: a node that is slow, and a node that is gone

Measured against a **real second `dockerd --tlsverify`** — its own process,
data-root and container namespace — with the control plane reaching it over
mTLS. Latency injected with `tc netem`, partition with `iptables -j DROP`
(a drop, not a reject: a partitioned machine sends no RST, which is what
makes it the hard case).

### Latency

| Added RTT to the node | Deploy (build + run + ready) |
|---|---|
| none | 3.1 s |
| 200 ms | 9.5 s |
| 400 ms | 17.7 s |

All three deploys succeeded, all landed on the remote node, and none of them
made a healthy node look dead. Latency costs throughput and nothing else —
the deploy is a conversation of many small round trips, so it scales roughly
with RTT.

`OXID_NODE_STATUS_TIMEOUT_SECS` (default 5 s) is the headroom before a slow
node starts being *treated* as a dead one. 400 ms RTT already spends a
noticeable share of it, so a worse link should raise it.

### Partition

This is where the interesting number was, and it was a defect rather than a
property. Blackholing a registered node's port:

| | before | after |
|---|---|---|
| Deploy aimed at a **healthy** node | 121 s | **7 s** |
| Health probe noticing the dead node | 126 s | **7 s** |
| Environments on the dead node | untouched | untouched |

Correctness held the whole time in both columns — nothing was moved, marked
destroyed or rebuilt — which is exactly why no test had caught it. The cost
was liveness: the fleet was walked one node at a time with no deadline, so a
machine that answers with silence held up every deploy for as long as the
kernel was willing to wait. The fleet is now asked concurrently and each
answer is bounded; a dead node costs one deadline, not one timeout per node
and, at startup, not one timeout per *environment* on it.
