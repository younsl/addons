import { gzipSync } from "node:zlib";

export type PackageFormat = "maven" | "npm" | "cargo" | "go" | "pypi" | "raw";

// Small, valid archives exercise the real server-side parsers without checking
// generated binary fixtures into the repository.
function crc32(bytes: Buffer): number {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ (crc & 1 ? 0xedb88320 : 0);
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function zip(files: Record<string, string>): Buffer {
  const local: Buffer[] = [], central: Buffer[] = [];
  let offset = 0;
  for (const [path, text] of Object.entries(files)) {
    const name = Buffer.from(path), data = Buffer.from(text), crc = crc32(data);
    const header = Buffer.alloc(30);
    header.writeUInt32LE(0x04034b50); header.writeUInt16LE(20, 4);
    header.writeUInt32LE(crc, 14); header.writeUInt32LE(data.length, 18);
    header.writeUInt32LE(data.length, 22); header.writeUInt16LE(name.length, 26);
    local.push(header, name, data);
    const directory = Buffer.alloc(46);
    directory.writeUInt32LE(0x02014b50); directory.writeUInt16LE(20, 4);
    directory.writeUInt16LE(20, 6); directory.writeUInt32LE(crc, 16);
    directory.writeUInt32LE(data.length, 20); directory.writeUInt32LE(data.length, 24);
    directory.writeUInt16LE(name.length, 28); directory.writeUInt32LE(offset, 42);
    central.push(directory, name); offset += header.length + name.length + data.length;
  }
  const directory = Buffer.concat(central), end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50); end.writeUInt16LE(central.length / 2, 8);
  end.writeUInt16LE(central.length / 2, 10); end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...local, directory, end]);
}

function tarGzip(files: Record<string, string>): Buffer {
  const parts: Buffer[] = [];
  for (const [path, text] of Object.entries(files)) {
    const data = Buffer.from(text), header = Buffer.alloc(512);
    header.write(path); header.write("0000644\0", 100); header.write("0000000\0", 108);
    header.write("0000000\0", 116); header.write(data.length.toString(8).padStart(11, "0") + "\0", 124);
    header.write("00000000000\0", 136); header.fill(32, 148, 156); header.write("0", 156);
    header.write("ustar\0", 257); header.write("00", 263);
    const sum = header.reduce((a, b) => a + b, 0);
    header.write(sum.toString(8).padStart(6, "0") + "\0 ", 148);
    parts.push(header, data, Buffer.alloc((512 - data.length % 512) % 512));
  }
  return gzipSync(Buffer.concat([...parts, Buffer.alloc(1024)]));
}

export function packageFixture(format: PackageFormat, version: string) {
  const name = "releaseprobe", module = `example.com/${name}`;
  let filename: string, buffer: Buffer, path: string;
  switch (format) {
    case "maven":
      filename = `${name}-${version}.jar`;
      buffer = zip({ "payload.txt": `maven ${version}\n` });
      path = `com/example/${name}/${version}/${filename}`;
      break;
    case "npm":
      filename = `${name}-${version}.tgz`;
      buffer = tarGzip({ "package/package.json": JSON.stringify({ name, version }), "package/index.js": "module.exports = 1;\n" });
      path = `${name}/-/${filename}`;
      break;
    case "cargo":
      filename = `${name}-${version}.crate`;
      buffer = tarGzip({ [`${name}-${version}/Cargo.toml`]: `[package]\nname = "${name}"\nversion = "${version}"\nedition = "2024"\nlicense = "MIT"\n`, [`${name}-${version}/src/lib.rs`]: "pub fn value() -> u8 { 1 }\n" });
      path = `api/v1/crates/${name}/${version}/download`;
      break;
    case "go":
      filename = `${name}-v${version}.zip`;
      buffer = zip({ [`${module}@v${version}/go.mod`]: `module ${module}\n\ngo 1.22\n`, [`${module}@v${version}/lib.go`]: "package releaseprobe\n\nconst Value = 1\n" });
      path = `${module}/@v/v${version}.zip`;
      break;
    case "pypi": {
      filename = `${name}-${version}-py3-none-any.whl`;
      const info = `${name}-${version}.dist-info`;
      buffer = zip({ [`${name}/__init__.py`]: "VALUE = 1\n", [`${info}/METADATA`]: `Metadata-Version: 2.1\nName: ${name}\nVersion: ${version}\n\n`, [`${info}/WHEEL`]: "Wheel-Version: 1.0\nGenerator: forklift-e2e\nRoot-Is-Purelib: true\nTag: py3-none-any\n", [`${info}/RECORD`]: "" });
      path = `packages/${name}/${filename}`;
      break;
    }
    case "raw":
      filename = `${name}-${version}.bin`; buffer = Buffer.from(`raw ${version}\n`); path = filename;
  }
  return { name: filename, mimeType: "application/octet-stream", buffer, path, version, module };
}
