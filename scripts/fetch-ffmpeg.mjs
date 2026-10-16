#!/usr/bin/env node
/**
 * fetch-ffmpeg.mjs — download a static FFmpeg build into `.tools/ffmpeg`.
 *
 * FFmpeg is optional: the built-in encoder covers MP4/AVI/WMV/MPEG-2 and GIF /
 * image sequences work without it. Installing it unlocks Matroska, WebM,
 * QuickTime and the software codecs, which are selected automatically once the
 * binary is found (see `screenrec doctor`).
 *
 * Usage:
 *   node scripts/fetch-ffmpeg.mjs [--url <zip>] [--dest <dir>] [--force]
 */

import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { pipeline } from 'node:stream/promises';
import { Readable } from 'node:stream';
import { execFileSync } from 'node:child_process';

const DEFAULT_URL = 'https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip';

function argOf(flag, fallback) {
  const index = process.argv.indexOf(flag);
  return index !== -1 && process.argv[index + 1] ? process.argv[index + 1] : fallback;
}

const url = argOf('--url', process.env.FFMPEG_URL ?? DEFAULT_URL);
const repoRoot = path.resolve(path.dirname(new URL(import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1')), '..');
const dest = path.resolve(argOf('--dest', path.join(repoRoot, '.tools', 'ffmpeg')));
const force = process.argv.includes('--force');
const cacheFile = path.join(dest, 'ffmpeg-download.zip');

const log = (...args) => console.log('[ffmpeg]', ...args);

if (fs.existsSync(path.join(dest, 'bin', process.platform === 'win32' ? 'ffmpeg.exe' : 'ffmpeg')) && !force) {
  log(`already installed in ${dest} (use --force to re-download)`);
  process.exit(0);
}

await fsp.mkdir(dest, { recursive: true });

if (!fs.existsSync(cacheFile) || force) {
  log(`downloading ${url}`);
  const response = await fetch(url, { redirect: 'follow' });
  if (!response.ok) {
    console.error(`[ffmpeg] download failed: HTTP ${response.status}`);
    process.exit(1);
  }
  const total = Number(response.headers.get('content-length') ?? 0);
  let received = 0;
  let lastReport = 0;

  const progress = new TransformStream({
    transform(chunk, controller) {
      received += chunk.byteLength;
      if (total > 0 && Date.now() - lastReport > 1000) {
        lastReport = Date.now();
        log(`${(received / 1048576).toFixed(1)} / ${(total / 1048576).toFixed(1)} MiB`);
      }
      controller.enqueue(chunk);
    },
  });

  await pipeline(
    Readable.fromWeb(response.body.pipeThrough(progress)),
    fs.createWriteStream(cacheFile),
  );
  log(`saved ${cacheFile} (${(received / 1048576).toFixed(1)} MiB)`);
} else {
  log(`using cached ${cacheFile}`);
}

log('extracting');
execFileSync(
  process.platform === 'win32' ? 'powershell.exe' : 'unzip',
  process.platform === 'win32'
    ? ['-NoProfile', '-Command', `Expand-Archive -LiteralPath '${cacheFile}' -DestinationPath '${dest}' -Force`]
    : ['-o', cacheFile, '-d', dest],
  { stdio: 'inherit' },
);

// Static builds unpack into a versioned folder; flatten `bin/` into place.
const entries = await fsp.readdir(dest, { withFileTypes: true });
for (const entry of entries) {
  if (!entry.isDirectory()) continue;
  const binDir = path.join(dest, entry.name, 'bin');
  if (fs.existsSync(binDir)) {
    await fsp.cp(binDir, path.join(dest, 'bin'), { recursive: true });
    log(`installed binaries from ${entry.name}/bin`);
    break;
  }
}

const exe = path.join(dest, 'bin', process.platform === 'win32' ? 'ffmpeg.exe' : 'ffmpeg');
if (!fs.existsSync(exe)) {
  console.error(`[ffmpeg] could not find ${exe} after extraction`);
  process.exit(1);
}

// Probing the binary needs to capture its output; some sandboxes forbid that
// (EPERM on the pipe). It is only a convenience, so never fail on it.
try {
  const version = execFileSync(exe, ['-hide_banner', '-version'], { encoding: 'utf8' }).split('\n')[0];
  log(version.trim());
} catch (err) {
  log(`installed ${exe} (version check skipped: ${err.code ?? err.message})`);
  log('run `screenrec doctor` to confirm the recorder can use it');
}
log(`done — ScreenRec will pick it up automatically from ${exe}`);
