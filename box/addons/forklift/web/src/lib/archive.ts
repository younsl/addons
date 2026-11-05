// Minimal client-side ZIP reader used to pre-fill upload coordinates from a
// picked archive (Maven jar, Go module zip). It reads the central directory and
// inflates single small entries with the browser's DecompressionStream, so no
// third-party dependency is pulled in. Everything here is best-effort: any
// malformed input resolves to null and the form falls back to manual entry.

const EOCD_SIGNATURE = 0x06054b50;
const CENTRAL_SIGNATURE = 0x02014b50;
const LOCAL_SIGNATURE = 0x04034b50;
const MAX_ARCHIVE_BYTES = 64 * 1024 * 1024; // don't read giant files into memory

type CentralEntry = {
  name: string;
  method: number;
  compressedSize: number;
  localOffset: number;
};

function readCentralDirectory(view: DataView): CentralEntry[] | null {
  const size = view.byteLength;
  // The End Of Central Directory record sits within the last 22 + 65535 bytes.
  const scanStart = Math.max(0, size - (22 + 0xffff));
  let eocd = -1;
  for (let i = size - 22; i >= scanStart; i--) {
    if (view.getUint32(i, true) === EOCD_SIGNATURE) {
      eocd = i;
      break;
    }
  }
  if (eocd < 0) return null;

  const total = view.getUint16(eocd + 10, true);
  let offset = view.getUint32(eocd + 16, true);
  const entries: CentralEntry[] = [];
  for (let i = 0; i < total; i++) {
    if (offset + 46 > size || view.getUint32(offset, true) !== CENTRAL_SIGNATURE) return null;
    const method = view.getUint16(offset + 10, true);
    const compressedSize = view.getUint32(offset + 20, true);
    const nameLen = view.getUint16(offset + 28, true);
    const extraLen = view.getUint16(offset + 30, true);
    const commentLen = view.getUint16(offset + 32, true);
    const localOffset = view.getUint32(offset + 42, true);
    const nameBytes = new Uint8Array(view.buffer, offset + 46, nameLen);
    entries.push({ name: new TextDecoder().decode(nameBytes), method, compressedSize, localOffset });
    offset += 46 + nameLen + extraLen + commentLen;
  }
  return entries;
}

async function inflate(bytes: Uint8Array, method: number): Promise<string | null> {
  if (method === 0) return new TextDecoder().decode(bytes);
  if (method !== 8) return null; // only stored + deflate are supported
  if (typeof DecompressionStream === "undefined") return null;
  try {
    const stream = new Blob([new Uint8Array(bytes)]).stream().pipeThrough(new DecompressionStream("deflate-raw"));
    return new TextDecoder().decode(await new Response(stream).arrayBuffer());
  } catch {
    return null;
  }
}

async function loadArchive(file: File): Promise<{ view: DataView; entries: CentralEntry[] } | null> {
  if (file.size === 0 || file.size > MAX_ARCHIVE_BYTES) return null;
  try {
    const view = new DataView(await file.arrayBuffer());
    const entries = readCentralDirectory(view);
    return entries ? { view, entries } : null;
  } catch {
    return null;
  }
}

async function readEntryText(view: DataView, entry: CentralEntry): Promise<string | null> {
  const base = entry.localOffset;
  if (base + 30 > view.byteLength || view.getUint32(base, true) !== LOCAL_SIGNATURE) return null;
  const nameLen = view.getUint16(base + 26, true);
  const extraLen = view.getUint16(base + 28, true);
  const dataStart = base + 30 + nameLen + extraLen;
  if (dataStart + entry.compressedSize > view.byteLength) return null;
  const bytes = new Uint8Array(view.buffer, dataStart, entry.compressedSize);
  return inflate(bytes, entry.method);
}

export type MavenCoordinates = {
  groupId?: string;
  artifactId?: string;
  version?: string;
  packaging?: string;
};

// parseMavenCoordinates reads META-INF/maven/<g>/<a>/pom.properties from a jar
// and, failing that, guesses artifactId/version from the filename. Packaging is
// taken from the file extension.
export async function parseMavenCoordinates(file: File): Promise<MavenCoordinates | null> {
  const packaging = extension(file.name);
  const archive = await loadArchive(file);
  if (archive) {
    const entry = archive.entries.find((e) => /(^|\/)META-INF\/maven\/[^/]+\/[^/]+\/pom\.properties$/.test(e.name));
    if (entry) {
      const text = await readEntryText(archive.view, entry);
      const props = text ? parseProperties(text) : null;
      if (props && (props.groupId || props.artifactId || props.version)) {
        return {
          groupId: props.groupId,
          artifactId: props.artifactId,
          version: props.version,
          packaging: packaging || undefined,
        };
      }
    }
  }
  const fromName = coordinatesFromFilename(file.name);
  if (fromName) return { ...fromName, packaging: packaging || undefined };
  // Without any recoverable coordinate, don't guess packaging from a stray
  // extension; leave the form untouched.
  return null;
}

// parseGoCoordinates reads a Go module zip whose entries are all prefixed with
// "<module>@<version>/" and returns that module path and version.
export async function parseGoCoordinates(file: File): Promise<{ module: string; version: string } | null> {
  const archive = await loadArchive(file);
  if (!archive) return null;
  for (const entry of archive.entries) {
    const match = entry.name.match(/^(.+)@(v[^/]+)\//);
    if (match) return { module: match[1], version: match[2] };
  }
  return null;
}

function parseProperties(text: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const line of text.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#") || trimmed.startsWith("!")) continue;
    const eq = trimmed.indexOf("=");
    if (eq > 0) out[trimmed.slice(0, eq).trim()] = trimmed.slice(eq + 1).trim();
  }
  return out;
}

function extension(name: string): string {
  const base = name.toLowerCase();
  const dot = base.lastIndexOf(".");
  return dot >= 0 ? base.slice(dot + 1) : "";
}

// coordinatesFromFilename splits a Maven-style "artifactId-version.ext" name.
// groupId cannot be recovered from the filename, so it is left blank.
function coordinatesFromFilename(name: string): { artifactId: string; version: string } | null {
  const dot = name.lastIndexOf(".");
  const stem = dot >= 0 ? name.slice(0, dot) : name;
  const match = stem.match(/^(.+)-(\d[\w.]*(?:-[A-Za-z0-9.]+)?)$/);
  if (!match) return null;
  return { artifactId: match[1], version: match[2] };
}
