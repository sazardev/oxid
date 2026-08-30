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
| 15 simultaneous pushes | 27.3 s | 8.1 s | 3.4× |

Three runs each, alternating, on the same host with the same warm Docker
layer cache: before 23.3 / 28.3 / 27.3 s, after 9.1 / 8.1 / 8.1 s. All
fifteen branches came up in every run.

Two separate things were in the way, and the second was the larger:

- **Every deploy on the node held one mutex.** Sibling branches share no
  checkout, no container name and no environment row, so they had no reason
  to queue behind one another. Locking is now keyed by what is actually
  shared.
- **A burst waited out a scheduler tick doing nothing.** Webhooks arrive
  faster than a drain can read the queue, so most of them landed after its
  snapshot and were told a drain was already running — true, but that drain
  had already read past them. Sixteen of the first measurement's
  twenty-eight seconds were that idle wait.

Read this as a floor, not a ceiling: the layer cache was warm, so each build
here is a second or two and the figure is dominated by lock contention. On
cold builds — the first push to a new branch — the gap is larger, because
what overlaps is then whole builds rather than the bookkeeping around them.

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
