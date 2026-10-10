import { useState } from "react";
import type { FormEvent } from "react";
import { Link } from "@tanstack/react-router";
import { CheckCircle2, Upload } from "lucide-react";
import { api } from "@/api";
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

export function RawUploadForm({ repository }: { repository: Repository }) {
  const { t } = useTranslation();
  const [file, setFile] = useState<File | null>(null);
  const [targetPath, setTargetPath] = useState("");
  const [progress, setProgress] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [complete, setComplete] = useState(false);

  const selectFile = (next: File | null) => {
    setFile(next);
    setTargetPath(next?.name ?? "");
    setError("");
    setComplete(false);
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!file || !targetPath.trim()) return;
    setBusy(true);
    setError("");
    setComplete(false);
    setProgress(0);
    try {
      const plan = await api.validateArtifactUpload(repository.id, {
        path: targetPath.trim(),
        size: file.size,
        content_type: file.type || "application/octet-stream",
      });
      if (plan.exists) {
        setError(t("repo.upload-exists"));
        return;
      }
      await api.uploadArtifact(repository.id, plan.path, file, setProgress);
      setProgress(100);
      setComplete(true);
      setFile(null);
      setTargetPath("");
    } catch (caught) {
      setError((caught as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return <div className="min-w-0 max-w-4xl">
    <PageHeader title={t("repo.upload-title")} actions={<Link className={buttonVariants({ variant: "outline" })}
      to="/workspace/repositories/$id/$tab" params={{ id: String(repository.id), tab: "artifacts" }}>{t("common.back")}</Link>} />
    <div className="mb-4 flex flex-wrap items-center gap-2 text-sm text-muted-foreground"><span>{repository.name}</span><Badge>{repository.format}</Badge><Badge variant="secondary">hosted</Badge></div>
    {complete ? <Card size="sm"><CardContent><div className="flex items-start gap-3"><CheckCircle2 className="mt-0.5 size-5 text-[var(--fx-success)]" />
      <div><h2 className="m-0 text-base font-semibold">{t("repo.upload-complete")}</h2>
        <Link className={buttonVariants({ className: "mt-3" })} to="/workspace/repositories/$id/$tab" params={{ id: String(repository.id), tab: "artifacts" }}>{t("common.artifacts")}</Link></div>
    </div></CardContent></Card> : <form onSubmit={submit}>
      <Card size="sm" className="mb-4"><CardContent>
        <p className="mb-4 mt-0 text-sm text-muted-foreground">{t("repo.upload-description")}</p>
        <FieldGroup className="grid gap-4">
          <Field><FieldLabel htmlFor="upload-raw-file">{t("repo.upload-file")}</FieldLabel>
            <Input id="upload-raw-file" type="file" disabled={busy} onChange={(event) => selectFile(event.target.files?.[0] ?? null)} />
            {file && <FieldDescription>{file.name} · {formatFileSize(file.size)}</FieldDescription>}
          </Field>
          {file && <Field><FieldLabel htmlFor="upload-raw-path">{t("repo.upload-target-path")}</FieldLabel>
            <Input id="upload-raw-path" value={targetPath} placeholder="path/to/artifact.bin" disabled={busy} onChange={(event) => setTargetPath(event.target.value)} />
          </Field>}
        </FieldGroup>
      </CardContent></Card>
      {error && <Alert className="mb-4" role="alert">{error}</Alert>}
      {busy && <div className="mb-4" aria-live="polite"><div className="mb-1 flex justify-between text-sm"><span>{t("repo.upload-uploading")}</span><span>{progress}%</span></div>
        <div className="h-2 overflow-hidden rounded-full bg-muted"><div className="h-full bg-primary" style={{ width: `${progress}%` }} /></div></div>}
      <div className="flex justify-end"><Button type="submit" disabled={busy || !file || !targetPath.trim()}><Upload />{busy ? t("repo.upload-uploading") : t("repo.upload-action")}</Button></div>
    </form>}
  </div>;
}
