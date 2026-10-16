#!/usr/bin/env node
/**
 * registry-proxy.mjs — offline / restricted-network bridge for crates.io.
 *
 * Why this exists
 * ---------------
 * On some Windows hosts the Schannel TLS provider is broken
 * (`SEC_E_NO_CREDENTIALS`, 0x8009030e), so Cargo's bundled libcurl cannot open
 * *any* HTTPS connection.  Plain HTTP still works, and Node.js (OpenSSL) still
 * works, so this tiny local server:
 *
 *   1. re-serves the crates.io **sparse index** over plain HTTP,
 *   2. rewrites `config.json` so Cargo downloads `.crate` files from us too,
 *   3. fetches upstream over HTTP (mirror) and caches everything on disk.
 *
 * Cargo is then pointed at it with:
 *
 *   [source.crates-io]
 *   replace-with = "http-mirror"
 *   [source.http-mirror]
 *   registry = "sparse+http://127.0.0.1:8799/index/"
 *
 * Checksums are still verified by Cargo against the index, so swapped sources
 * cannot inject different bytes silently.
 *
 * Usage:  node scripts/registry-proxy.mjs [--port 8799] [--cache <dir>]
 */

import http from 'node:http';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { Readable } from 'node:stream';

function argOf(flag, fallback) {
  const i = process.argv.indexOf(flag);
  return i !== -1 && process.argv[i + 1] ? process.argv[i + 1] : fallback;
}

const PORT = Number(argOf('--port', process.env.PROXY_PORT ?? 8799));
const CACHE_DIR = path.resolve(argOf('--cache', process.env.PROXY_CACHE ?? '.toolchain/proxy-cache'));
const INDEX_UPSTREAM = (argOf('--index', process.env.PROXY_INDEX_UPSTREAM ?? 'http://mirrors.aliyun.com/crates.io-index')).replace(/\/+$/, '');
const DL_UPSTREAM = (argOf('--dl', process.env.PROXY_DL_UPSTREAM ?? 'http://mirrors.aliyun.com/crates/api/v1/crates')).replace(/\/+$/, '');
const INDEX_TTL_MS = Number(argOf('--index-ttl', 6 * 60 * 60 * 1000));

const INDEX_CACHE = path.join(CACHE_DIR, 'index');
const CRATE_CACHE = path.join(CACHE_DIR, 'crates');

await fsp.mkdir(INDEX_CACHE, { recursive: true });
await fsp.mkdir(CRATE_CACHE, { recursive: true });

const log = (...a) => console.log(new Date().toISOString().slice(11, 19), ...a);

/** Fetch with a couple of retries; upstream mirrors occasionally hiccup. */
async function fetchWithRetry(url, attempts = 3) {
  let lastErr;
  for (let i = 0; i < attempts; i++) {
    try {
      return await fetch(url, { redirect: 'follow' });
    } catch (err) {
      lastErr = err;
      await new Promise((r) => setTimeout(r, 250 * (i + 1)));
    }
  }
  throw lastErr;
}

function sendJson(res, status, obj) {
  const body = Buffer.from(JSON.stringify(obj));
  res.writeHead(status, { 'content-type': 'application/json', 'content-length': body.length, 'cache-control': 'no-store' });
  res.end(body);
}

/**
 * Only allow safe relative paths (no traversal, no absolute paths, no queries).
 * Version numbers may contain `+` (build metadata), so that is allowed too.
 */
function safeRelative(raw) {
  const clean = decodeURIComponent(raw).replace(/^\/+/, '');
  if (!clean || clean.includes('..') || clean.includes('\\') || clean.startsWith('/')) return null;
  if (!/^[A-Za-z0-9._+\-/]+$/.test(clean)) return null;
  return clean;
}

async function handleIndexConfig(res) {
  // The one field that matters: point "dl" at ourselves, over plain HTTP.
  sendJson(res, 200, {
    dl: `http://127.0.0.1:${PORT}/dl`,
    api: `http://127.0.0.1:${PORT}/api`,
  });
}

async function handleIndexFile(rel, res) {
  const cacheFile = path.join(INDEX_CACHE, rel);
  try {
    const st = await fsp.stat(cacheFile);
    if (Date.now() - st.mtimeMs < INDEX_TTL_MS) {
      log('index HIT ', rel);
      const body = await fsp.readFile(cacheFile);
      res.writeHead(200, { 'content-type': 'text/plain', 'content-length': body.length });
      return res.end(body);
    }
  } catch {
    /* not cached */
  }

  const upstream = `${INDEX_UPSTREAM}/${rel}`;
  let upstreamRes;
  try {
    upstreamRes = await fetchWithRetry(upstream);
  } catch (err) {
    log('index ERR ', rel, err.message);
    return sendJson(res, 502, { error: `upstream index unavailable: ${err.message}` });
  }

  if (upstreamRes.status === 404) {
    log('index 404 ', rel);
    res.writeHead(404, { 'content-type': 'text/plain' });
    return res.end('not found');
  }
  if (!upstreamRes.ok) {
    log('index HTTP', upstreamRes.status, rel);
    res.writeHead(upstreamRes.status, { 'content-type': 'text/plain' });
    return res.end(`upstream status ${upstreamRes.status}`);
  }

  const body = Buffer.from(await upstreamRes.arrayBuffer());
  await fsp.mkdir(path.dirname(cacheFile), { recursive: true });
  await fsp.writeFile(cacheFile, body).catch(() => {});
  log('index GET ', rel, body.length);
  res.writeHead(200, { 'content-type': 'text/plain', 'content-length': body.length });
  res.end(body);
}

async function handleCrateDownload(rel, res) {
  // Cargo appends "/{crate}/{version}/download" to the dl base.
  const parts = rel.split('/').filter(Boolean);
  if (parts.length !== 3 || parts[2] !== 'download') {
    return sendJson(res, 400, { error: 'expected /<crate>/<version>/download' });
  }
  const [name, version] = parts;
  const cacheFile = path.join(CRATE_CACHE, `${name}-${version}.crate`);

  const serve = async (body) => {
    res.writeHead(200, { 'content-type': 'application/x-tar', 'content-length': body.length });
    res.end(body);
  };

  try {
    const body = await fsp.readFile(cacheFile);
    log('crate HIT ', `${name}-${version}`, body.length);
    return serve(body);
  } catch {
    /* not cached */
  }

  const upstream = `${DL_UPSTREAM}/${encodeURIComponent(name)}/${encodeURIComponent(version)}/download`;
  let upstreamRes;
  try {
    upstreamRes = await fetchWithRetry(upstream);
  } catch (err) {
    log('crate ERR ', `${name}-${version}`, err.message);
    return sendJson(res, 502, { error: `upstream crate unavailable: ${err.message}` });
  }
  if (!upstreamRes.ok) {
    log('crate HTTP', upstreamRes.status, `${name}-${version}`);
    res.writeHead(upstreamRes.status, { 'content-type': 'text/plain' });
    return res.end(`upstream status ${upstreamRes.status}`);
  }

  const body = Buffer.from(await upstreamRes.arrayBuffer());
  await fsp.writeFile(cacheFile, body).catch(() => {});
  log('crate GET ', `${name}-${version}`, body.length);
  serve(body);
}

const server = http.createServer((req, res) => {
  const url = new URL(req.url ?? '/', `http://127.0.0.1:${PORT}`);
  const pathname = url.pathname;
  if (req.method !== 'GET' && req.method !== 'HEAD') return sendJson(res, 405, { error: 'GET only' });

  if (pathname === '/healthz') return sendJson(res, 200, { status: 'ok', cache: CACHE_DIR });
  if (pathname === '/index/config.json' || pathname === '/config.json') return handleIndexConfig(res);
  if (pathname === '/api' || pathname.startsWith('/api/')) {
    // Published/yank endpoints are never used by a build; fail loudly.
    return sendJson(res, 501, { error: 'registry API is not proxied' });
  }

  const rel = safeRelative(pathname.replace(/^\/(index|dl)\//, ''));
  if (!rel) return sendJson(res, 400, { error: 'bad path' });

  if (pathname.startsWith('/dl/')) return void handleCrateDownload(rel, res);
  if (pathname.startsWith('/index/')) return void handleIndexFile(rel, res);
  return sendJson(res, 404, { error: 'unknown route' });
});

server.listen(PORT, '127.0.0.1', () => {
  log(`registry proxy listening on http://127.0.0.1:${PORT}/index/  (cache: ${CACHE_DIR})`);
  log(`index upstream: ${INDEX_UPSTREAM}`);
  log(`crate upstream: ${DL_UPSTREAM}`);
});

process.on('SIGINT', () => server.close(() => process.exit(0)));
process.on('SIGTERM', () => server.close(() => process.exit(0)));
