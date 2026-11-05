import { useRef, useState } from "react";
import type { FormEvent } from "react";
import { Link } from "@tanstack/react-router";
import { CheckCircle2, Upload } from "lucide-react";
import { UploadAPIError, uploadArtifact } from "@/api";
import type { ArtifactUploadResult } from "@/api";
import { useTranslation } from "@/lib/i18n";
import { formatFileSize } from "@/utils/format-file-size";
import { Alert } from "@/components/app-ui/alert";
import { Badge } from "@/components/app-ui/badge";
import { PageHeader } from "@/components/app-ui/page";
import { Button, buttonVariants } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Field, FieldDescription, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import type { Repository } from "@/services/v1/openapi-types";

export function CargoUploadForm({ repository, csrfToken }: { repository: Repository; csrfToken?: string }) {
  const { t } = useTranslation();
  const [file, setFile] = useState<File | null>(null);
  const [progress, setProgress] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [result, setResult] = useState<ArtifactUploadResult | null>(null);
  const controller = useRef<AbortController | null>(null);
  const idempotencyKey = useRef<string | null>(null);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setError(""); setResult(null);
    if (!file || !file.name.toLowerCase().endsWith(".crate")) {
      setError(t("upload.cargo-file-required")); return;
    }
    const form = new FormData();
    form.append("manifest", JSON.stringify({ schema_version: 1, format: "cargo", overwrite: false,
      assets: [{ part: "asset0" }], cargo: { yanked: false } }));
    form.append("asset0", new File([file], file.name, { type: "application/octet-stream", lastModified: file.lastModified }));
    const ctl = new AbortController(); controller.current = ctl;
    idempotencyKey.current ??= crypto.randomUUID(); setBusy(true); setProgress(0);
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

  return <div className="min-w-0 max-w-4xl">
    <PageHeader title={t("upload.title")} actions={<Link className={buttonVariants({ variant: "outline" })}
      to="/workspace/repositories/$id/$tab" params={{ id: String(repository.id), tab: "artifacts" }}>{t("upload.back")}</Link>} />
    <div className="mb-4 flex flex-wrap items-center gap-2 text-sm text-muted-foreground"><span>{repository.name}</span><Badge>{repository.format}</Badge><Badge variant="secondary">hosted</Badge></div>
    {result ? <Card size="sm"><CardContent><div className="flex items-start gap-3"><CheckCircle2 className="mt-0.5 size-5 text-[var(--fx-success)]" />
      <div><h2 className="m-0 text-base font-semibold">{t("upload.success")}</h2><p className="text-sm text-muted-foreground">{result.coordinate}</p>
        <Link className={buttonVariants({ className: "mt-3" })} to="/workspace/repositories/$id/$tab" params={{ id: String(repository.id), tab: "artifacts" }}>{t("upload.view-artifacts")}</Link></div>
    </div></CardContent></Card> : <form onSubmit={submit}>
      <Card size="sm" className="mb-4"><CardContent><h2 className="m-0 mb-1 text-base font-semibold">{t("upload.cargo-crate")}</h2>
        <p className="mb-4 mt-0 text-sm text-muted-foreground">{t("upload.cargo-help")}</p>
        <Field><FieldLabel htmlFor="upload-cargo-file">{t("upload.file")}</FieldLabel>
          <Input id="upload-cargo-file" type="file" accept=".crate" disabled={busy} onChange={(event) => { setFile(event.target.files?.[0] ?? null); idempotencyKey.current = null; }} />
          <FieldDescription>{file ? `${file.name} · ${formatFileSize(file.size)}` : t("upload.cargo-file-help")}</FieldDescription>
        </Field></CardContent></Card>
      {error && <Alert className="mb-4" role="alert">{error}</Alert>}
      {busy && <div className="mb-4" aria-live="polite"><div className="mb-1 flex justify-between text-sm"><span>{t("upload.uploading")}</span><span>{progress > 0 ? `${progress}%` : t("upload.receiving")}</span></div>
        <div className="h-2 overflow-hidden rounded-full bg-muted"><div className="h-full bg-primary" style={{ width: `${progress}%` }} /></div></div>}
      <div className="flex justify-end gap-2">{busy && <Button type="button" variant="outline" onClick={() => controller.current?.abort()}>{t("common.cancel")}</Button>}
        <Button type="submit" disabled={busy}><Upload />{busy ? t("upload.uploading") : t("upload.action")}</Button></div>
    </form>}
  </div>;
}
