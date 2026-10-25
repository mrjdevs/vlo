#!/usr/bin/env node
// Usage: node scripts/bench.mjs [--base URL] [--pid PID] [--cookie NAME=VALUE]
//   [--samples N] [--warmup N] [--concurrency N] [--load N] [--routes a,b,c]
//   [--json PATH]

import { execFileSync, execSync, spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { performance } from 'node:perf_hooks';

const args = {};
for (let i = 2; i < process.argv.length; i++) {
  const a = process.argv[i];
  if (!a.startsWith('--')) continue;
  const eq = a.indexOf('=');
  if (eq > -1) args[a.slice(2, eq)] = a.slice(eq + 1);
  else args[a.slice(2)] = process.argv[++i] ?? true;
}

const base = (args.base ?? 'http://127.0.0.1:3000').replace(/\/$/, '');
const warmup = Number(args.warmup ?? 5);
const samples = Number(args.samples ?? 100);
const concurrency = Number(args.concurrency ?? 10);
const loadCount = Number(args.load ?? 500);
const cookie = args.cookie ?? '';
const timeout = Number(args.timeout ?? 30000);
const headers = cookie ? { cookie } : {};
const routes = (
  args.routes ??
  '/,/products,/shop,/docs,/components,/api/products?limit=20,/api/products?limit=100,/api/cart_count'
).split(',');

function findPid(port) {
  try {
    const out = execSync('netstat -ano', { encoding: 'utf8' });
    for (const line of out.split(/\r?\n/)) {
      const m = line.match(new RegExp(`[.:]${port}\\s+\\S+\\s+LISTENING\\s+(\\d+)`));
      if (m) return Number(m[1]);
    }
  } catch {}
  return null;
}

const port = Number(new URL(base).port || 80);
const pid = Number(args.pid ?? findPid(port));
if (!pid) {
  console.error(`No listener on port ${port}; pass --pid to benchmark a running server.`);
  process.exit(1);
}
const meta = readServerMeta(pid);

function readServerMeta(targetPid) {
  const script = `
$OutputEncoding = [Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = $OutputEncoding
$ErrorActionPreference = 'Stop'
$p = Get-Process -Id ${targetPid}
$cs = Get-CimInstance Win32_ComputerSystem
$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
$os = Get-CimInstance Win32_OperatingSystem
[pscustomobject]@{
  pid = $p.Id
  name = $p.ProcessName
  path = $p.Path
  startTime = $p.StartTime.ToUniversalTime().ToString('o')
  workingSet = $p.WorkingSet64
  privateBytes = $p.PrivateMemorySize64
  threads = $p.Threads.Count
  handles = $p.HandleCount
  cpuName = $cpu.Name.Trim()
  physicalCores = $cpu.NumberOfCores
  logicalCores = $cpu.NumberOfLogicalProcessors
  totalRam = $cs.TotalPhysicalMemory
  osCaption = $os.Caption
  osVersion = $os.Version
} | ConvertTo-Json -Compress`;
  const out = execFileSync(
    'powershell',
    ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-Command', script],
    { encoding: 'utf8' }
  );
  const data = JSON.parse(out.trim());
  try {
    data.binaryBytes = statSync(data.path).size;
  } catch {}
  return data;
}

const scratch = mkdtempSync(join(tmpdir(), 'vlo-bench-'));
const samplerPath = join(scratch, 'sampler.ps1');
const samplePath = join(scratch, 'samples.txt');
const stopPath = join(scratch, 'stop');
writeFileSync(
  samplerPath,
  `param([int]$TargetPid,[string]$Out,[string]$StopFile)
$sw = [System.IO.StreamWriter]::new($Out, $false, [System.Text.UTF8Encoding]::new($false))
$sw.AutoFlush = $true
$proc = [System.Diagnostics.Process]::GetProcessById($TargetPid)
while (-not (Test-Path -LiteralPath $StopFile)) {
  try {
    $proc.Refresh()
    if ($proc.HasExited) { break }
    $ticks = [DateTime]::UtcNow.Ticks
    $cpu = $proc.TotalProcessorTime.TotalSeconds.ToString('F3', [Globalization.CultureInfo]::InvariantCulture)
    $sw.WriteLine("$ticks|$($proc.WorkingSet64)|$($proc.PrivateMemorySize64)|$($proc.Threads.Count)|$($proc.HandleCount)|$cpu")
  } catch { break }
  Start-Sleep -Milliseconds 20
}
$sw.Dispose()
`,
  'utf8'
);
const sampler = spawn(
  'powershell',
  ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', samplerPath, '-TargetPid', String(pid), '-Out', samplePath, '-StopFile', stopPath],
  { windowsHide: true, stdio: 'ignore' }
);
await new Promise((resolve) => setTimeout(resolve, 1000));

function readSamples() {
  try {
    return readFileSync(samplePath, 'utf8')
      .split(/\r?\n/)
      .filter(Boolean)
      .map((line) => {
        const [ticks, ws, priv, threads, handles, cpu] = line.split('|');
        return {
          t: Number(ticks) / 10000 - 62135596800000,
          ws: Number(ws),
          priv: Number(priv),
          threads: Number(threads),
          handles: Number(handles),
          cpu: Number(cpu),
        };
      });
  } catch {
    return [];
  }
}

async function request(url) {
  const t0 = performance.now();
  const res = await fetch(url, {
    headers,
    redirect: 'manual',
    signal: AbortSignal.timeout(timeout),
  });
  const buf = await res.arrayBuffer();
  return { dt: performance.now() - t0, status: res.status, bytes: buf.byteLength };
}

function stats(values) {
  if (!values.length) return null;
  const s = [...values].sort((a, b) => a - b);
  const q = (p) => s[Math.min(s.length - 1, Math.max(0, Math.ceil((p / 100) * s.length) - 1))];
  const mean = s.reduce((a, b) => a + b, 0) / s.length;
  const sd = Math.sqrt(s.reduce((a, b) => a + (b - mean) ** 2, 0) / s.length);
  return {
    n: s.length,
    min: s[0],
    mean,
    sd,
    p50: q(50),
    p90: q(90),
    p95: q(95),
    p99: q(99),
    max: s[s.length - 1],
  };
}

function resourcesBetween(all, from, to) {
  const slice = all.filter((s) => s.t >= from && s.t <= to);
  if (!slice.length) return null;
  const cpuCores =
    slice.length > 1 ? (slice.at(-1).cpu - slice[0].cpu) / ((slice.at(-1).t - slice[0].t) / 1000) : 0;
  return {
    samples: slice.length,
    rssPeak: Math.max(...slice.map((s) => s.ws)),
    rssMean: slice.reduce((a, s) => a + s.ws, 0) / slice.length,
    privatePeak: Math.max(...slice.map((s) => s.priv)),
    threadsPeak: Math.max(...slice.map((s) => s.threads)),
    handlesPeak: Math.max(...slice.map((s) => s.handles)),
    cpuCoresAvg: cpuCores,
    cpuOneCorePct: cpuCores * 100,
  };
}

async function measureSequential(url) {
  const lat = [];
  const bytes = [];
  const statuses = {};
  const errors = [];
  const start = Date.now();
  for (let i = 0; i < samples; i++) {
    try {
      const r = await request(url);
      statuses[r.status] = (statuses[r.status] ?? 0) + 1;
      if (r.status >= 200 && r.status < 300) {
        lat.push(r.dt);
        bytes.push(r.bytes);
      } else {
        errors.push(r.status);
      }
    } catch (e) {
      errors.push(e.name);
    }
  }
  const end = Date.now();
  const wall = (end - start) / 1000;
  return {
    ...stats(lat),
    rps: lat.length / wall,
    meanBytes: bytes.length ? bytes.reduce((a, b) => a + b, 0) / bytes.length : 0,
    statuses,
    errors: errors.length,
    window: [start, end],
  };
}

async function measureLoad(url) {
  const lat = [];
  const bytes = [];
  const statuses = {};
  const errors = [];
  let next = 0;
  const start = Date.now();
  await Promise.all(
    Array.from({ length: concurrency }, async () => {
      while (true) {
        const i = next++;
        if (i >= loadCount) break;
        try {
          const r = await request(url);
          statuses[r.status] = (statuses[r.status] ?? 0) + 1;
          if (r.status >= 200 && r.status < 300) {
            lat.push(r.dt);
            bytes.push(r.bytes);
          } else {
            errors.push(r.status);
          }
        } catch (e) {
          errors.push(e.name);
        }
      }
    })
  );
  const end = Date.now();
  const wall = (end - start) / 1000;
  return {
    ...stats(lat),
    rps: (lat.length + errors.length) / wall,
    meanBytes: bytes.length ? bytes.reduce((a, b) => a + b, 0) / bytes.length : 0,
    statuses,
    errors: errors.length,
    concurrency,
    window: [start, end],
  };
}

const results = [];
for (const route of routes) {
  const url = `${base}${route}`;
  for (let i = 0; i < warmup; i++) {
    try {
      await request(url);
    } catch {}
  }
  const seq = await measureSequential(url);
  const load = loadCount > 0 ? await measureLoad(url) : null;
  const allSamples = readSamples();
  const entry = {
    route,
    url,
    seq: { ...seq, resources: resourcesBetween(allSamples, seq.window[0], seq.window[1]) },
    load: load && { ...load, resources: resourcesBetween(allSamples, load.window[0], load.window[1]) },
  };
  delete entry.seq.window;
  if (entry.load) delete entry.load.window;
  results.push(entry);

  const fmt = (v, d = 2) => (Number.isFinite(v) ? v.toFixed(d) : '-');
  const status = Object.entries(seq.statuses).map(([k, v]) => `${k}x${v}`).join(',');
  let line = `${route.padEnd(26)} seq p50=${fmt(seq.p50)} p90=${fmt(seq.p90)} p95=${fmt(seq.p95)} p99=${fmt(seq.p99)} max=${fmt(seq.max)} ms  ${status}`;
  if (load) line += `  load[${concurrency}] ${fmt(load.rps, 0)} req/s p50=${fmt(load.p50)} p99=${fmt(load.p99)}`;
  const res = entry.load?.resources ?? entry.seq.resources;
  if (res) line += `  cpu=${fmt(res.cpuOneCorePct, 0)}% rss=${fmt(res.rssPeak / 1048576, 1)}MiB thr=${res.threadsPeak}`;
  console.log(line);
}

writeFileSync(stopPath, '');
await new Promise((resolve) => {
  const timer = setTimeout(() => {
    sampler.kill();
    resolve();
  }, 3000);
  sampler.on('exit', () => {
    clearTimeout(timer);
    resolve();
  });
});

const allSamples = readSamples();
const overall = {
  rssPeak: allSamples.length ? Math.max(...allSamples.map((s) => s.ws)) : meta.workingSet,
  privatePeak: allSamples.length ? Math.max(...allSamples.map((s) => s.priv)) : meta.privateBytes,
  threadsPeak: allSamples.length ? Math.max(...allSamples.map((s) => s.threads)) : meta.threads,
  handlesPeak: allSamples.length ? Math.max(...allSamples.map((s) => s.handles)) : meta.handles,
  sampleCount: allSamples.length,
};

const report = {
  generatedAt: new Date().toISOString(),
  base,
  server: meta,
  config: { warmup, samples, concurrency, loadCount, cookie: Boolean(cookie), routes },
  overall,
  routes: results,
};

if (args.json) {
  writeFileSync(args.json, JSON.stringify(report, null, 2));
  console.log(`\nJSON written to ${args.json}`);
}

console.log(
  `\nserver pid=${meta.pid} ${meta.name} ${(meta.binaryBytes / 1048576).toFixed(2)} MiB | ` +
    `${meta.cpuName} (${meta.physicalCores}C/${meta.logicalCores}T) | ` +
    `${(meta.totalRam / 1073741824).toFixed(1)} GiB RAM | ${meta.osCaption}`
);
console.log(
  `peak rss=${(overall.rssPeak / 1048576).toFixed(1)} MiB private=${(overall.privatePeak / 1048576).toFixed(1)} MiB ` +
    `threads=${overall.threadsPeak} handles=${overall.handlesPeak}`
);

rmSync(scratch, { recursive: true, force: true });
