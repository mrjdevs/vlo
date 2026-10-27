# VLO Benchmark Report — serve performance and resource profile

**Version:** VLO 0.9.0  
**Platform:** Windows 11 Pro x64, Intel Core i5-4210M (2C/4T, 2.6 GHz), 11.5 GiB RAM, localhost  
**Runs:** 2026-10-25 (dev vs serve, debug vs release) and 2026-10-26 (standard benchmark, resource profile)

---

## 2026-10-26 — Standard benchmark and resource profile

Current release build measured after the data-source performance fixes
(per-render response cache, no redundant COUNT for unlimited queries, debug
logging gated out of production). The historical dev-vs-serve numbers follow
in the second half of this report.

### Environment

| Item | Value |
|---|---|
| CPU | Intel Core i5-4210M @ 2.60 GHz — 2 cores / 4 logical processors |
| RAM | 11.5 GiB |
| OS | Windows 11 Pro (10.0.26200) |
| Binary | `target/release/vlo.exe`, 7,639,552 B (7.29 MiB), `opt-level = "z"`, fat LTO, `strip = "symbols"`, `panic = "abort"` |
| Server | `vlo serve` on `127.0.0.1:3001`, prebuilt `.vlo/build`, warm build cache |
| Features | database, auth, api, sse, modules, files, flash, directives, email, jobs, pagination |
| Database | `vlo_app.db` (SQLite) |
| Auth | temporary Editor session; deleted after the run |

`VLO_DEBUG` is present in `.env`, but serve logs only when `VLO_DEBUG_SERVE=1`;
the server produced exactly two startup lines during the whole benchmark.

### Method

- Node 24 `fetch` with keep-alive: 5 warmup requests, then 100 sequential
  requests and 500 concurrent requests (concurrency 10) per route.
- Three complete passes. Latency and throughput columns are the **median of the
  three runs**; resource columns are the **peak observed**.
- A separate PowerShell sampler reads the server process every ~20 ms:
  `WorkingSet64` (RSS), `PrivateMemorySize64`, `Threads.Count`, `HandleCount`,
  `TotalProcessorTime`.
- CPU is normalized to one logical core; 100% = one core saturated, >100% =
  multiple cores.
- Client, sampler, IDE, and browser share the same four logical CPUs. Absolute
  latency drifted 1.3–1.8× between passes; medians and ratios are the reliable
  signal.

### Latency — sequential requests (median of 3 runs, ms)

| Route | Response | min | mean | p50 | p90 | p95 | p99 | max |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `/` | 29.8 KB | 7.41 | 10.42 | 9.40 | 12.70 | 15.93 | 21.06 | 23.15 |
| `/products` | 233.8 KB | 23.00 | 39.53 | **36.31** | 48.13 | 62.09 | 73.05 | 105.99 |
| `/shop` | 47.4 KB | 14.92 | 19.50 | **18.90** | 23.07 | 25.55 | 47.36 | 55.24 |
| `/docs` | 87.5 KB | 17.22 | 24.91 | 23.30 | 28.72 | 33.15 | 57.82 | 61.05 |
| `/components` | 52.1 KB | 7.71 | 9.04 | 8.61 | 10.29 | 10.66 | 20.03 | 21.77 |
| `/api/products?limit=20` | 4.8 KB | 2.44 | 3.06 | 2.97 | 3.68 | 3.85 | 4.24 | 5.13 |
| `/api/products?limit=100` | 19.2 KB | 4.96 | 6.19 | 5.74 | 7.38 | 7.98 | 9.38 | 25.34 |
| `/api/cart_count` | 55 B | 1.18 | 1.51 | 1.44 | 1.99 | 2.05 | 2.14 | 2.27 |

### Throughput under load — 500 requests at concurrency 10 (median of 3 runs)

| Route | req/s | p50 (ms) | p99 (ms) |
|---|---:|---:|---:|
| `/` | 98 | 90.4 | 535.3 |
| `/products` | 46 | 179.6 | 649.8 |
| `/shop` | 69 | 137.3 | 295.3 |
| `/docs` | 74 | 121.2 | 405.4 |
| `/components` | 164 | 59.8 | 82.7 |
| `/api/products?limit=20` | 539 | 17.7 | 35.8 |
| `/api/products?limit=100` | 189 | 40.2 | 283.0 |
| `/api/cart_count` | 1121 | 7.9 | 22.7 |

### Process resources

| Route | CPU (one core, load) | Peak RSS | Peak threads |
|---|---:|---:|---:|
| `/` | 21% | 24.1 MiB | 17 |
| `/products` | 83% | 27.7 MiB | 26 |
| `/shop` | 83% | 26.9 MiB | 37 |
| `/docs` | 50% | 26.4 MiB | 37 |
| `/components` | 45% | 25.0 MiB | 25 |
| `/api/products?limit=20` | 88% | 24.3 MiB | 17 |
| `/api/products?limit=100` | 87% | 24.0 MiB | 17 |
| `/api/cart_count` | 42% | 24.2 MiB | 17 |

Whole-process profile:

| Metric | At benchmark start | Peak |
|---|---:|---:|
| Working set (RSS) | 22.7 MiB | 27.7 MiB |
| Private bytes | 14.1 MiB | 19.8 MiB |
| Threads | 15 | 37 |
| Handles | 184 | 254 |

### Findings

1. **The cart page is no longer the outlier.** `/shop` now renders in 18.9 ms
   p50 with a 47 KB payload. An earlier interleaved A/B on the same build
   measured `/shop` at 18.47 ms on the previous binary versus 9.03 ms on the
   fixed binary (n=20, logging off), with byte-identical HTML.
2. **`/products` is now the slowest page.** 36.3 ms p50 and 233.8 KB per
   response. It has a single data source, so the cost is markup volume: 20 rows
   produce 180 KB of table body, with 107 inline SVGs, 41 modal blocks, and 20
   thumbnail widgets. The next win is template weight (shared SVG sprite,
   modals rendered on demand), not data fetching.
3. **Memory stays small and stable.** RSS moved from 22.7 MiB to 27.7 MiB and
   did not grow monotonically across passes; private bytes stayed under
   20 MiB. A serve process fits comfortably in a 64–128 MiB budget.
4. **Data-source pages grow the thread pool.** Thread count rises from 15 at
   idle to 25–37 while `/shop` and `/docs` are under load. That matches the
   synchronous SQL path bridged through `block_in_place`; heavy pages touch
   blocking threads, static/API routes stay on the async workers.
5. **CPU stays below 1.5 cores on this host.** The heaviest page averaged
   0.8–1.3 of one core at concurrency 10; JSON APIs peaked near one core.
   Throughput here is CPU-bound on a 2-core laptop, not I/O-bound.
6. **APIs are cheap and fast.** `limit=20` sustains 539 req/s at 3.0 ms p50;
   `cart_count` sustains 1121 req/s at 1.4 ms p50. The `limit=100` tail
   (p99 283 ms under load) is the first place to look if API tail latency
   matters.
7. **Tail latency under load is dominated by page weight.** Heavy pages show
   p99 of 295–650 ms at concurrency 10; light pages and APIs stay in the tens
   of milliseconds. On a dedicated load generator these tails would shrink,
   but the relative ordering would remain.

### Caveats

- Single-host benchmark: the client, sampler, IDE, and browser compete for the
  same four logical CPUs. Absolute milliseconds are pessimistic compared with
  a dedicated load generator.
- The server and build cache were warm. Cold starts are not covered here.
- Resource sampling is approximate at ~20 ms resolution and records OS
  working-set/private bytes, not allocation-level profiling.
- Three passes are enough to expose drift and give medians, not enough for
  tight confidence intervals.

### Reproduction

1. `cargo build --release`
2. `vlo build`
3. Start `vlo serve` (this project's `.env` uses port 3001).
4. Create a short-lived Editor session and use it as `vlo_session`.
5. Run:

   ```
   node scripts/bench.mjs --base http://127.0.0.1:3001 \
     --cookie "vlo_session=<token>" --samples 100 --load 500 \
     --json .vlo/bench-results.json
   ```

   The script needs Node 24+ and Windows PowerShell (process counters use
   .NET `System.Diagnostics.Process`). It prints latency percentiles,
   throughput, CPU, RSS, and thread counts, and optionally writes full JSON.

---

## 2026-10-25 — dev vs serve, debug vs release (historical)

### Binaries

| Build | Size | Profile |
|---|---:|---|
| Debug (`target/debug/vlo.exe`) | 34,471,424 B (32.9 MiB) | unoptimized |
| Release (`target/release/vlo.exe`) | 7,632,384 B (7.28 MiB) | `opt-level = "z"`, fat LTO, `strip = "symbols"`, `panic = "abort"` |

The 2026-10-26 release build is 7,639,552 B with the same profile.

### Summary

**Release + serve is the fastest configuration.** Release-serve medians: `/products` 35.2 ms, `/shop` 32.9 ms, `/docs` 21.6 ms, `/components` 11.2 ms, `/` 11.0 ms. API medians: 4.3 ms (`limit=20`) and 8.8 ms (`limit=100`).

- **Serve vs dev:** 1.30–1.75× faster on pages in release, 1.42–2.59× in debug. APIs are statistically tied.
- **Release vs debug:** 1.8–3.3× faster on pages in serve mode, 2.3–5.0× in dev mode; APIs 2.2–2.5× faster.
- **Largest serve gains** are on template/component-heavy pages. Live data-source pages (`/products`, `/shop`) gain less because their data is still resolved per request.
- **APIs are unaffected by the build cache**; serve/dev differences there are noise.

### Method

- Same SQLite database (`vlo_app.db`), same `.vlo/build` output, same feature set, same session secret.
- Guard-protected routes were requested with a temporary Editor session; it was deleted after benchmarking.
- Node 24 `fetch` with keep-alive; 3 warmup requests followed by 40 measured requests per route.
- Serve/dev pairs were benchmarked **concurrently and interleaved**, alternating request order every sample to cancel system-load drift.
- Reported metric is median (p50); p90 is included in the raw tables used to produce this report.
- `serve` read the prebuilt `.vlo/build` cache; `dev` re-rendered `.vlo` pages, components, and layouts per request.

### Release build — dev vs serve (p50, ms)

| Route | dev | serve | Serve gain |
|---|---:|---:|---:|
| `/` | 14.31 | 10.98 | 1.30× |
| `/products` (Editor) | 61.46 | 35.22 | 1.75× |
| `/shop` (Editor) | 46.34 | 32.85 | 1.41× |
| `/docs` | 28.71 | 21.60 | 1.33× |
| `/components` | 17.99 | 11.24 | 1.60× |
| `/api/cart_count` | 3.78 | 2.97 | 1.27× (noise) |
| `/api/products?limit=20` | 4.91 | 4.28 | 1.15× (noise) |
| `/api/products?limit=100` | 9.53 | 8.76 | 1.09× (noise) |

### Debug build — dev vs serve (p50, ms)

| Route | dev | serve | Serve gain |
|---|---:|---:|---:|
| `/` | 33.03 | 19.58 | 1.69× |
| `/products` | 305.05 | 117.79 | 2.59× |
| `/shop` | 106.16 | 74.98 | 1.42× |
| `/docs` | 89.53 | 48.08 | 1.86× |
| `/components` | 56.84 | 24.05 | 2.36× |
| `/api/cart_count` | 5.73 | 5.26 | 1.09× |
| `/api/products?limit=20` | 10.80 | 9.62 | 1.12× |
| `/api/products?limit=100` | 20.81 | 22.21 | 0.94× (parity) |

### Release vs debug (p50, ms)

| Route | Debug serve | Release serve | Gain | Debug dev | Release dev | Gain |
|---|---:|---:|---:|---:|---:|---:|
| `/` | 19.58 | 10.98 | 1.78× | 33.03 | 14.31 | 2.31× |
| `/products` | 117.79 | 35.22 | 3.34× | 305.05 | 61.46 | 4.96× |
| `/shop` | 74.98 | 32.85 | 2.28× | 106.16 | 46.34 | 2.29× |
| `/docs` | 48.08 | 21.60 | 2.23× | 89.53 | 28.71 | 3.12× |
| `/components` | 24.05 | 11.24 | 2.14× | 56.84 | 17.99 | 3.16× |
| `/api/products?limit=100` | 22.21 | 8.76 | 2.54× | 20.81 | 9.53 | 2.18× |

### Findings

1. **Serve wins by skipping template assembly.** `serve` loads prebuilt HTML into the in-memory build cache at startup and avoids reading `.vlo` files plus rendering components, layouts, and control flow. `dev` performs that work on every request.
2. **Static pages benefit most.** `/docs` is closest to pure static and still gains 1.33× in release; `/components` gains 1.60×.
3. **Live data-source pages gain less.** `/products` and `/shop` use the Live cache tier, so both modes resolve data sources and inject rows per request. Serve still avoids template assembly, giving 1.41–1.75× in release.
4. **APIs do not use the build cache.** Serve/dev API results are within noise; the meaningful API delta is release over debug (~2.2–2.5×).
5. **Release is worth it.** The size-tuned release profile is 1.8–3.3× faster than debug on rendered pages and up to ~5× on the heaviest dev-rendered page.
6. **Payload differences.** Dev responses are 3–6 KB larger because of the HMR client; dev and serve HTML are not byte-identical.

### Caveats

- All measurements were taken on localhost with warm caches and a warm build cache.
- Absolute milliseconds drift with machine load; the interleaved ratios are the reliable takeaway.
- Real network latency, cold starts, and concurrent users will change absolute numbers, not the direction.
- A query-plan check for `SELECT * FROM products ORDER BY id DESC LIMIT 100` showed a reverse rowid scan of 190 rows; SQLite time remained sub-millisecond (~0.24 ms median).

### Reproduction outline

1. `cargo build && cargo build --release`
2. `vlo build`
3. Start `vlo serve` and `vlo dev` on separate ports with `PORT`/`HOST`/`SESSION_SECRET` overrides.
4. Use an authenticated Editor session for `/`, `/products`, and `/shop`.
5. Warm up, then measure at least 40 samples per route, alternating between the two servers.
