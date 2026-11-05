// @vitest-environment node
// The parser uses DecompressionStream / Blob.stream(), which jsdom does not
// implement; Node's environment provides both (as it does in real browsers).
import { describe, expect, it } from "vitest";

import { parseGoCoordinates, parseMavenCoordinates } from "@/lib/archive";

type Entry = { name: string; data: Uint8Array; method: 0 | 8 };

async function deflateRaw(bytes: Uint8Array): Promise<Uint8Array> {
  const stream = new Blob([bytes]).stream().pipeThrough(new CompressionStream("deflate-raw"));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

// buildZip assembles a minimal but standards-correct ZIP so the reader is
// exercised against real central-directory + local-header layout. CRCs are
// left zero because the reader does not check them.
async function buildZip(entries: Entry[]): Promise<File> {
  const enc = new TextEncoder();
  const locals: Uint8Array[] = [];
  const centrals: Uint8Array[] = [];
  let offset = 0;

  for (const entry of entries) {
    const name = enc.encode(entry.name);
    const uncompressed = entry.data;
    const payload = entry.method === 8 ? await deflateRaw(uncompressed) : uncompressed;

    const local = new Uint8Array(30 + name.length + payload.length);
    const lv = new DataView(local.buffer);
    lv.setUint32(0, 0x04034b50, true);
    lv.setUint16(8, entry.method, true);
    lv.setUint32(18, payload.length, true);
    lv.setUint32(22, uncompressed.length, true);
    lv.setUint16(26, name.length, true);
    local.set(name, 30);
    local.set(payload, 30 + name.length);
    locals.push(local);

    const central = new Uint8Array(46 + name.length);
    const cv = new DataView(central.buffer);
    cv.setUint32(0, 0x02014b50, true);
    cv.setUint16(10, entry.method, true);
    cv.setUint32(20, payload.length, true);
    cv.setUint32(24, uncompressed.length, true);
    cv.setUint16(28, name.length, true);
    cv.setUint32(42, offset, true);
    central.set(name, 46);
    centrals.push(central);

    offset += local.length;
  }

  const centralSize = centrals.reduce((sum, c) => sum + c.length, 0);
  const eocd = new Uint8Array(22);
  const ev = new DataView(eocd.buffer);
  ev.setUint32(0, 0x06054b50, true);
  ev.setUint16(8, entries.length, true);
  ev.setUint16(10, entries.length, true);
  ev.setUint32(12, centralSize, true);
  ev.setUint32(16, offset, true);

  return new File([...locals, ...centrals, eocd], "archive.zip");
}

describe("parseMavenCoordinates", () => {
  it("reads coordinates from pom.properties (deflated)", async () => {
    const props = "#generated\ngroupId=com.acme\nartifactId=widget\nversion=1.2.3\n";
    const jar = await buildZip([
      { name: "com/acme/widget/Thing.class", data: new Uint8Array([0xca, 0xfe]), method: 0 },
      { name: "META-INF/maven/com.acme/widget/pom.properties", data: new TextEncoder().encode(props), method: 8 },
    ]);
    const coords = await parseMavenCoordinates(new File([jar], "widget-1.2.3.jar"));
    expect(coords).toEqual({ groupId: "com.acme", artifactId: "widget", version: "1.2.3", packaging: "jar" });
  });

  it("falls back to the filename when pom.properties is absent", async () => {
    const jar = await buildZip([{ name: "a/B.class", data: new Uint8Array([1]), method: 0 }]);
    const coords = await parseMavenCoordinates(new File([jar], "my-lib-2.0.0.jar"));
    expect(coords?.artifactId).toBe("my-lib");
    expect(coords?.version).toBe("2.0.0");
    expect(coords?.packaging).toBe("jar");
  });

  it("returns null for a non-archive", async () => {
    const coords = await parseMavenCoordinates(new File([new Uint8Array([1, 2, 3])], "notes.txt"));
    expect(coords).toBeNull();
  });
});

describe("parseGoCoordinates", () => {
  it("derives module and version from the entry prefix", async () => {
    const root = "example.com/acme/widget@v1.2.3/";
    const zip = await buildZip([
      { name: root + "go.mod", data: new TextEncoder().encode("module example.com/acme/widget\n"), method: 0 },
      { name: root + "main.go", data: new TextEncoder().encode("package widget\n"), method: 8 },
    ]);
    const coords = await parseGoCoordinates(new File([zip], "module.zip"));
    expect(coords).toEqual({ module: "example.com/acme/widget", version: "v1.2.3" });
  });
});
