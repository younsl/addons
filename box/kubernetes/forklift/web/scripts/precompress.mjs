// Precompress build output so the Rust binary can serve gzip variants without
// compressing on the fly. Only text-like assets gain from gzip; fonts (woff2)
// and images are already compressed and are left alone. Originals are kept so
// clients without Accept-Encoding: gzip still get identity responses.
import { promises as fs } from "node:fs";
import { join } from "node:path";
import { gzipSync } from "node:zlib";

// Only assets/ is compressed: its content-hashed files are gitignored, so the
// .gz siblings never land in the repository or go stale against a tracked
// original the way root files (index.html, favicons) could.
const dist = new URL("../../src/webui/dist/assets", import.meta.url).pathname;
const compressible = /\.(js|css|svg|json|map|txt|xml|webmanifest)$/;

async function walk(dir) {
  const out = [];
  for (const entry of await fs.readdir(dir, { withFileTypes: true })) {
    const p = join(dir, entry.name);
    if (entry.isDirectory()) out.push(...(await walk(p)));
    else out.push(p);
  }
  return out;
}

let files = 0;
let saved = 0;
for (const path of await walk(dist)) {
  if (!compressible.test(path)) continue;
  const raw = await fs.readFile(path);
  const gz = gzipSync(raw, { level: 9 });
  // A grown or barely-shrunk file is not worth a second embedded copy.
  if (gz.length >= raw.length * 0.9) continue;
  await fs.writeFile(path + ".gz", gz);
  files += 1;
  saved += raw.length - gz.length;
}
console.log(`precompress: ${files} files, ${(saved / 1024).toFixed(0)} KiB saved`);
