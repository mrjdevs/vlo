# VLO Benchmark Report — dev vs serve, debug vs release

**Date:** 2026-10-25  
**Version:** VLO 0.8.7  
**Platform:** Windows 10 x64, Node.js 24, localhost

## Binaries

| Build | Size | Profile |
|---|---:|---|
| Debug (`target/debug/vlo.exe`) | 34,471,424 B (32.9 MiB) | unoptimized |
| Release (`target/release/vlo.exe`) | 7,632,384 B (7.28 MiB) | `opt-level = "z"`, fat LTO, `strip = "symbols"`, `panic = "abort"` |

Release was rebuilt from the current source immediately before measuring.

## Summary

**Release + serve is the fastest configuration.** Release-serve medians: `/products` 35.2 ms, `/shop` 32.9 ms, `/docs` 21.6 ms, `/components` 11.2 ms, `/` 11.0 ms. API medians: 4.3 ms (`limit=20`) and 8.8 ms (`limit=100`).

- **Serve vs dev:** 1.30–1.75× faster on pages in release, 1.42–2.59× in debug. APIs are statistically tied.
- **Release vs debug:** 1.8–3.3× faster on pages in serve mode, 2.3–5.0× in dev mode; APIs 2.2–2.5× faster.
- **Largest serve gains** are on template/component-heavy pages. Live data-source pages (`/products`, `/shop`) gain less because their data is still resolved per request.
- **APIs are unaffected by the build cache**; serve/dev differences there are noise.

## Method

- Same SQLite database (`vlo_app.db`), same `.vlo/build` output, same feature set, same session secret.
- Guard-protected routes were requested with a temporary Editor session; it was deleted after benchmarking.
- Node 24 `fetch` with keep-alive; 3 warmup requests followed by 40 measured requests per route.
- Serve/dev pairs were benchmarked **concurrently and interleaved**, alternating request order every sample to cancel system-load drift.
- Reported metric is median (p50); p90 is included in the raw tables used to produce this report.
- `serve` read the prebuilt `.vlo/build` cache; `dev` re-rendered `.vlo` pages, components, and layouts per request.

## Release build — dev vs serve (p50, ms)

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

## Debug build — dev vs serve (p50, ms)

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

## Release vs debug (p50, ms)

| Route | Debug serve | Release serve | Gain | Debug dev | Release dev | Gain |
|---|---:|---:|---:|---:|---:|---:|
| `/` | 19.58 | 10.98 | 1.78× | 33.03 | 14.31 | 2.31× |
| `/products` | 117.79 | 35.22 | 3.34× | 305.05 | 61.46 | 4.96× |
| `/shop` | 74.98 | 32.85 | 2.28× | 106.16 | 46.34 | 2.29× |
| `/docs` | 48.08 | 21.60 | 2.23× | 89.53 | 28.71 | 3.12× |
| `/components` | 24.05 | 11.24 | 2.14× | 56.84 | 17.99 | 3.16× |
| `/api/products?limit=100` | 22.21 | 8.76 | 2.54× | 20.81 | 9.53 | 2.18× |

## Findings

1. **Serve wins by skipping template assembly.** `serve` loads prebuilt HTML into the in-memory build cache at startup and avoids reading `.vlo` files plus rendering components, layouts, and control flow. `dev` performs that work on every request.
2. **Static pages benefit most.** `/docs` is closest to pure static and still gains 1.33× in release; `/components` gains 1.60×.
3. **Live data-source pages gain less.** `/products` and `/shop` use the Live cache tier, so both modes resolve data sources and inject rows per request. Serve still avoids template assembly, giving 1.41–1.75× in release.
4. **APIs do not use the build cache.** Serve/dev API results are within noise; the meaningful API delta is release over debug (~2.2–2.5×).
5. **Release is worth it.** The size-tuned release profile is 1.8–3.3× faster than debug on rendered pages and up to ~5× on the heaviest dev-rendered page.
6. **Payload differences.** Dev responses are 3–6 KB larger because of the HMR client; dev and serve HTML are not byte-identical.

## Caveats

- All measurements were taken on localhost with warm caches and a warm build cache.
- Absolute milliseconds drift with machine load; the interleaved ratios are the reliable takeaway.
- Real network latency, cold starts, and concurrent users will change absolute numbers, not the direction.
- A query-plan check for `SELECT * FROM products ORDER BY id DESC LIMIT 100` showed a reverse rowid scan of 190 rows; SQLite time remained sub-millisecond (~0.24 ms median).

## Reproduction outline

1. `cargo build && cargo build --release`
2. `vlo build`
3. Start `vlo serve` and `vlo dev` on separate ports with `PORT`/`HOST`/`SESSION_SECRET` overrides.
4. Use an authenticated Editor session for `/`, `/products`, and `/shop`.
5. Warm up, then measure at least 40 samples per route, alternating between the two servers.
