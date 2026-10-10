import { useRef, useState } from "react";
import type { FormEvent } from "react";
import { Link } from "@tanstack/react-router";
import { CheckCircle2, Upload } from "lucide-react";
import { UploadAPIError, uploadArtifact } from "@/api";
import type { ArtifactUploadResult } from "@/api";
import { parseGoCoordinates } from "@/lib/archive";
import { useTranslation } from "@/lib/i18n";
import { formatFileSize } from "@/utils/format-file-size";
import { Alert } from "@/components/app-ui/alert";
import { Badge } from "@/components/app-ui/badge";
import { PageHeader } from "@/components/app-ui/page";
import { Button, buttonVariants } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import type { Repository } from "@/services/v1/openapi-types";

export function GoUploadForm({ repository, csrfToken }: { repository: Repository; csrfToken?: string }) {
  const { t } = useTranslation();
  const [modulePath, setModulePath] = useState(""); const [version, setVersion] = useState("");
  const [zipFile, setZipFile] = useState<File | null>(null);
  const [progress, setProgress] = useState(0); const [busy, setBusy] = useState(false);
  const [error, setError] = useState(""); const [result, setResult] = useState<ArtifactUploadResult | null>(null);
  const controller = useRef<AbortController | null>(null); const idempotencyKey = useRef<string | null>(null);
  const [autoFilledFrom, setAutoFilledFrom] = useState("");
  const changed = () => { idempotencyKey.current = null; };
  const selectZip = async (next: File | null) => {
    setZipFile(next); changed();
    if (!next) return;
    // Go module zips prefix every entry with "<module>@<version>/"; lift those
    // into the empty coordinate fields so the user need not retype them.
    const coords = await parseGoCoordinates(next);
    if (!coords) return;
    let filled = false;
    if (coords.module && !modulePath.trim()) { setModulePath(coords.module); filled = true; }
    if (coords.version && !version.trim()) { setVersion(coords.version); filled = true; }
    if (filled) { setAutoFilledFrom(next.name); changed(); }
  };
  const submit = async (event: FormEvent) => {
    event.preventDefault(); setError(""); setResult(null);
    if (!modulePath.trim() || !version.trim() || !zipFile?.name.toLowerCase().endsWith(".zip")) { setError(t("upload.go-required")); return; }
    const form = new FormData();
    form.append("manifest", JSON.stringify({ schema_version: 1, format: "go", overwrite: false,
      assets: [{ part: "asset0", role: "zip" }],
      go: { module: modulePath.trim(), version: version.trim() } }));
    form.append("asset0", new File([zipFile], zipFile.name, { type: "application/octet-stream", lastModified: zipFile.lastModified }));
    const ctl = new AbortController(); controller.current = ctl; idempotencyKey.current ??= crypto.randomUUID(); setBusy(true); setProgress(0);
    try {
      const uploaded = await uploadArtifact(repository.id, form, { idempotencyKey: idempotencyKey.current, csrfToken, signal: ctl.signal,
        onProgress: (loaded, total) => setProgress(total > 0 ? Math.min(100, Math.round((loaded / total) * 100)) : 0) });
      setProgress(100); setResult(uploaded); idempotencyKey.current = null;
    } catch (caught) {
      if (caught instanceof DOMException && caught.name === "AbortError") setError(t("upload.cancelled"));
      else if (caught instanceof UploadAPIError) { setError(caught.problem.detail); if (caught.problem.upload_id) idempotencyKey.current = null; }
      else setError((caught as Error).message);
    } finally { controller.current = null; setBusy(false); }
  };
  // Server publishes the GOPROXY triplet under "<module>/@v/<version>.{zip,mod,info}".
  const previewPaths = modulePath.trim() && version.trim()
    ? [".zip", ".mod", ".info"].map((ext) => `${modulePath.trim()}/@v/${version.trim()}${ext}`)
    : [];
  return <div className="min-w-0 max-w-4xl"><PageHeader title={t("upload.title")} actions={<Link className={buttonVariants({ variant: "outline" })}
    to="/workspace/repositories/$id/$tab" params={{ id: String(repository.id), tab: "artifacts" }}>{t("upload.back")}</Link>} />
    <div className="mb-4 flex flex-wrap items-center gap-2 text-sm text-muted-foreground"><span>{repository.name}</span><Badge>{repository.format}</Badge><Badge variant="secondary">hosted</Badge></div>
    {result ? <Card size="sm"><CardContent><div className="flex gap-3"><CheckCircle2 className="mt-0.5 size-5 text-[var(--fx-success)]" /><div>
      <h2 className="m-0 text-base font-semibold">{t("upload.success")}</h2><p className="text-sm text-muted-foreground">{result.coordinate}</p>
      <Alert className="my-3">{t("upload.go-private-help")}</Alert><Link className={buttonVariants()} to="/workspace/repositories/$id/$tab" params={{ id: String(repository.id), tab: "artifacts" }}>{t("upload.view-artifacts")}</Link>
    </div></div></CardContent></Card> : <form onSubmit={submit}>
      <Card size="sm" className="mb-4"><CardContent><div className="mb-1 flex flex-wrap items-center gap-2"><h2 className="m-0 text-base font-semibold">{t("upload.go-module")}</h2>{autoFilledFrom && <Badge variant="secondary">{t("upload.autofilled")}</Badge>}</div><p className="mb-4 mt-0 text-sm text-muted-foreground">{t("upload.go-help")}</p>
        <FieldGroup className="grid gap-4 md:grid-cols-2">
          <Field><FieldLabel htmlFor="upload-go-zip">{t("upload.go-zip")}</FieldLabel><Input id="upload-go-zip" type="file" accept=".zip" disabled={busy} onChange={(e) => selectZip(e.target.files?.[0] ?? null)} /><FieldDescription>{zipFile ? `${zipFile.name} · ${formatFileSize(zipFile.size)}` : t("upload.go-zip-help")}</FieldDescription></Field>
          {zipFile && <>
          <Field><FieldLabel htmlFor="upload-go-module">{t("upload.go-module-path")}</FieldLabel><Input id="upload-go-module" value={modulePath} placeholder="example.com/acme/widget" disabled={busy} onChange={(e) => { setModulePath(e.target.value); changed(); }} /></Field>
          <Field><FieldLabel htmlFor="upload-go-version">{t("common.version")}</FieldLabel><Input id="upload-go-version" value={version} placeholder="v1.2.3" disabled={busy} onChange={(e) => { setVersion(e.target.value); changed(); }} /></Field>
          </>}
        </FieldGroup></CardContent></Card>
      {error && <Alert className="mb-4" role="alert">{error}</Alert>}{busy && <div className="mb-4" aria-live="polite"><div className="mb-1 flex justify-between text-sm"><span>{t("upload.uploading")}</span><span>{progress ? `${progress}%` : t("upload.receiving")}</span></div><div className="h-2 rounded-full bg-muted"><div className="h-full bg-primary" style={{ width: `${progress}%` }} /></div></div>}
      {zipFile && previewPaths.length > 0 && <Card size="sm" className="mb-4"><CardContent><h2 className="m-0 mb-2 text-base font-semibold">{t("upload.path-preview")}</h2><ul className="m-0 rounded-md bg-muted/40 p-3 pl-8 font-mono text-xs">{previewPaths.map((path) => <li key={path}>{path}</li>)}</ul></CardContent></Card>}
      {zipFile && <div className="flex justify-end gap-2">{busy && <Button type="button" variant="outline" onClick={() => controller.current?.abort()}>{t("common.cancel")}</Button>}<Button type="submit" disabled={busy}><Upload />{busy ? t("upload.uploading") : t("upload.action")}</Button></div>}
    </form>}
  </div>;
}
