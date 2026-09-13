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
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import type { Repository } from "@/services/v1/openapi-types";

export function NPMUploadForm({ repository, csrfToken }: { repository: Repository; csrfToken?: string }) {
  const { t } = useTranslation();
  const [file, setFile] = useState<File | null>(null);
  const [distTag, setDistTag] = useState("latest");
  const [progress, setProgress] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [result, setResult] = useState<ArtifactUploadResult | null>(null);
  const controller = useRef<AbortController | null>(null);
  const idempotencyKey = useRef<string | null>(null);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setError("");
    setResult(null);
    if (!file || !file.name.toLowerCase().endsWith(".tgz")) {
      setError(t("upload.npm-file-required"));
      return;
    }
    if (!distTag.trim()) {
      setError(t("upload.npm-tag-required"));
      return;
    }
    const form = new FormData();
    form.append("manifest", JSON.stringify({
      schema_version: 1, format: "npm", overwrite: false,
      assets: [{ part: "asset0" }], npm: { dist_tag: distTag.trim() },
    }));
    form.append("asset0", new File([file], file.name, { type: "application/octet-stream", lastModified: file.lastModified }));
    const ctl = new AbortController();
    controller.current = ctl;
    idempotencyKey.current ??= crypto.randomUUID();
    setBusy(true);
    setProgress(0);
    try {
      const uploaded = await uploadArtifact(repository.id, form, {
        idempotencyKey: idempotencyKey.current,
        csrfToken,
        signal: ctl.signal,
        onProgress: (loaded, total) => setProgress(total > 0 ? Math.min(100, Math.round((loaded / total) * 100)) : 0),
      });
      setProgress(100);
      setResult(uploaded);
      idempotencyKey.current = null;
    } catch (caught) {
      if (caught instanceof DOMException && caught.name === "AbortError") setError(t("upload.cancelled"));
      else if (caught instanceof UploadAPIError) {
        setError(caught.problem.detail);
        if (caught.problem.upload_id) idempotencyKey.current = null;
      } else setError((caught as Error).message);
    } finally {
      controller.current = null;
      setBusy(false);
    }
  };

  return (
    <div className="min-w-0 max-w-4xl">
      <PageHeader title={t("upload.title")} actions={(
        <Link className={buttonVariants({ variant: "outline" })}
          to="/workspace/repositories/$id/$tab" params={{ id: String(repository.id), tab: "artifacts" }}>
          {t("upload.back")}
        </Link>
      )} />
      <div className="mb-4 flex min-w-0 flex-wrap items-center gap-2 text-sm text-muted-foreground">
        <span>{repository.name}</span><Badge>{repository.format}</Badge><Badge variant="secondary">hosted</Badge>
      </div>
      {result ? (
        <Card size="sm"><CardContent><div className="flex items-start gap-3">
          <CheckCircle2 className="mt-0.5 size-5 text-[var(--fx-success)]" aria-hidden="true" />
          <div className="min-w-0 flex-1">
            <h2 className="m-0 text-base font-semibold">{t("upload.success")}</h2>
            <p className="mt-1 text-sm text-muted-foreground">{result.coordinate}</p>
            <dl className="mt-4 grid gap-2 text-sm sm:grid-cols-3">
              <div><dt className="text-muted-foreground">{t("upload.created")}</dt><dd className="m-0 font-medium">{result.created.length}</dd></div>
              <div><dt className="text-muted-foreground">{t("upload.derived")}</dt><dd className="m-0 font-medium">{result.derived.length}</dd></div>
              <div><dt className="text-muted-foreground">{t("upload.durability")}</dt><dd className="m-0 font-medium">{result.durability}</dd></div>
            </dl>
            <Link className={buttonVariants({ className: "mt-5" })}
              to="/workspace/repositories/$id/$tab" params={{ id: String(repository.id), tab: "artifacts" }}>
              {t("upload.view-artifacts")}
            </Link>
          </div>
        </div></CardContent></Card>
      ) : (
        <form onSubmit={submit}>
          <Card size="sm" className="mb-4"><CardContent>
            <h2 className="m-0 mb-1 text-base font-semibold">{t("upload.npm-package")}</h2>
            <p className="mb-4 mt-0 text-sm text-muted-foreground">{t("upload.npm-help")}</p>
            <FieldGroup className="grid gap-4 md:grid-cols-[minmax(0,1fr)_12rem]">
              <Field>
                <FieldLabel htmlFor="upload-npm-file">{t("upload.file")}</FieldLabel>
                <Input id="upload-npm-file" type="file" accept=".tgz,application/gzip" disabled={busy}
                  onChange={(event) => { setFile(event.target.files?.[0] ?? null); idempotencyKey.current = null; }} />
                <FieldDescription>{file ? `${file.name} · ${formatFileSize(file.size)}` : t("upload.npm-file-help")}</FieldDescription>
              </Field>
              <Field>
                <FieldLabel htmlFor="upload-npm-tag">{t("upload.npm-dist-tag")}</FieldLabel>
                <Input id="upload-npm-tag" value={distTag} disabled={busy}
                  onChange={(event) => { setDistTag(event.target.value); idempotencyKey.current = null; }} />
                <FieldDescription>{t("upload.npm-tag-help")}</FieldDescription>
              </Field>
            </FieldGroup>
          </CardContent></Card>
          {error && <Alert className="mb-4" role="alert">{error}</Alert>}
          {busy && <div className="mb-4" aria-live="polite">
            <div className="mb-1 flex justify-between text-sm"><span>{t("upload.uploading")}</span><span>{progress > 0 ? `${progress}%` : t("upload.receiving")}</span></div>
            <div className="h-2 overflow-hidden rounded-full bg-muted"><div className="h-full bg-primary transition-[width]" style={{ width: `${progress}%` }} /></div>
          </div>}
          <div className="flex flex-wrap justify-end gap-2 max-sm:flex-col-reverse">
            {busy && <Button type="button" variant="outline" onClick={() => controller.current?.abort()}>{t("common.cancel")}</Button>}
            <Button type="submit" disabled={busy}><Upload aria-hidden="true" />{busy ? t("upload.uploading") : t("upload.action")}</Button>
          </div>
        </form>
      )}
    </div>
  );
}
