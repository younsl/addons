import { useRef, useState } from "react";
import type { FormEvent } from "react";
import { Link } from "@tanstack/react-router";
import { CheckCircle2, Upload } from "lucide-react";
import { UploadAPIError, cancelArtifactUpload, commitArtifactUpload, uploadArtifact } from "@/api";
import type { ArtifactUploadResult, UploadProblem } from "@/api";
import { parseMavenCoordinates } from "@/lib/archive";
import { useTranslation } from "@/lib/i18n";
import { formatFileSize } from "@/utils/format-file-size";
import { Alert } from "@/components/app-ui/alert";
import { Badge } from "@/components/app-ui/badge";
import { PageHeader } from "@/components/app-ui/page";
import { Button, buttonVariants } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { inferExtension, validClassifier } from "@/routes/workspace/repositories/-components/upload/upload-fields";
import type { Repository } from "@/services/v1/openapi-types";

export function MavenUploadForm({ repository, csrfToken }: { repository: Repository; csrfToken?: string }) {
  const { t } = useTranslation();
  const [groupId, setGroupId] = useState("");
  const [artifactId, setArtifactId] = useState("");
  const [version, setVersion] = useState("");
  const [packaging, setPackaging] = useState("jar");
  const [generatePOM, setGeneratePOM] = useState(true);
  // A managed upload publishes exactly one file; the coordinates plus the
  // optional generated POM decide the rest of the layout.
  const [file, setFile] = useState<File | null>(null);
  const [extension, setExtension] = useState("jar");
  const [classifier, setClassifier] = useState("");
  const controller = useRef<AbortController | null>(null);
  const idempotencyKey = useRef<string | null>(null);
  const [progress, setProgress] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [fieldErrors, setFieldErrors] = useState<Record<string, string[]>>({});
  const [result, setResult] = useState<ArtifactUploadResult | null>(null);
  const [conflict, setConflict] = useState<UploadProblem | null>(null);
  const [replaceAcknowledged, setReplaceAcknowledged] = useState(false);
  const [autoFilledFrom, setAutoFilledFrom] = useState("");

  const hasPOM = extension.trim().toLowerCase() === "pom" && classifier.trim() === "";
  const hasFile = Boolean(file);

  // Mirror the server's Maven layout so the user sees the path that will be
  // created before submitting: <group>/<artifact>/<version>/<artifact>-<version>[-classifier].<ext>.
  const previewPaths: string[] = [];
  if (file && groupId.trim() && artifactId.trim() && version.trim() && extension.trim()) {
    const base = `${groupId.trim().split(".").join("/")}/${artifactId.trim()}/${version.trim()}/${artifactId.trim()}-${version.trim()}`;
    const cls = classifier.trim() ? `-${classifier.trim()}` : "";
    previewPaths.push(`${base}${cls}.${extension.trim()}`);
    if (generatePOM && !hasPOM) previewPaths.push(`${base}.pom`);
  }

  const selectFile = async (next: File | null) => {
    setFile(next);
    if (next && !extension.trim()) setExtension(inferExtension(next.name));
    idempotencyKey.current = null;
    if (!next) return;
    // Pre-fill empty coordinate fields from the archive (pom.properties) so the
    // user rarely types them by hand; never clobber values they already set.
    const coords = await parseMavenCoordinates(next);
    if (!coords) return;
    let filled = false;
    if (coords.groupId && !groupId.trim()) { setGroupId(coords.groupId); filled = true; }
    if (coords.artifactId && !artifactId.trim()) { setArtifactId(coords.artifactId); filled = true; }
    if (coords.version && !version.trim()) { setVersion(coords.version); filled = true; }
    if (coords.packaging && (!packaging.trim() || packaging === "jar")) setPackaging(coords.packaging);
    if (coords.packaging && (!extension.trim() || extension === "jar")) setExtension(coords.packaging);
    if (filled) { setAutoFilledFrom(next.name); idempotencyKey.current = null; }
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setError("");
    setFieldErrors({});
    setResult(null);
    setConflict(null);
    setReplaceAcknowledged(false);
    if (!groupId.trim() || !artifactId.trim() || !version.trim() || !packaging.trim()) {
      setError(t("upload.coordinates-required"));
      return;
    }
    if (!file || !extension.trim()) {
      setError(t("upload.assets-required"));
      return;
    }
    if (!validClassifier(classifier)) {
      setError(t("upload.classifier-invalid"));
      return;
    }
    if (hasPOM && generatePOM) {
      setError(t("upload.pom-mode-conflict"));
      return;
    }

    const manifest = {
      schema_version: 1,
      format: "maven",
      overwrite: false,
      assets: [{ part: "asset0", extension: extension.trim(), classifier: classifier.trim() }],
      maven: {
        group_id: groupId.trim(), artifact_id: artifactId.trim(), version: version.trim(),
        generate_pom: generatePOM, packaging: packaging.trim(),
      },
    };
    const form = new FormData();
    form.append("manifest", JSON.stringify(manifest));
    form.append("asset0", new File([file], file.name, {
      type: "application/octet-stream",
      lastModified: file.lastModified,
    }));

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
      if (caught instanceof DOMException && caught.name === "AbortError") {
        setError(t("upload.cancelled"));
      } else if (caught instanceof UploadAPIError) {
        setError(caught.problem.detail);
        setFieldErrors(caught.problem.field_errors ?? {});
        if (caught.problem.conflict_action === "replace" && caught.problem.upload_id) {
          setConflict(caught.problem);
        } else if (caught.problem.upload_id) idempotencyKey.current = null;
      } else {
        setError((caught as Error).message);
      }
    } finally {
      controller.current = null;
      setBusy(false);
    }
  };

  const confirmReplacement = async () => {
    if (!conflict?.upload_id || !replaceAcknowledged) return;
    setBusy(true);
    setError("");
    try {
      setResult(await commitArtifactUpload(repository.id, conflict.upload_id, csrfToken));
      setConflict(null);
      idempotencyKey.current = null;
    } catch (caught) {
      setError(caught instanceof UploadAPIError ? caught.problem.detail : (caught as Error).message);
    } finally { setBusy(false); }
  };

  const cancelReplacement = async () => {
    if (!conflict?.upload_id) return;
    setBusy(true);
    try {
      await cancelArtifactUpload(repository.id, conflict.upload_id, csrfToken);
      setConflict(null);
      setReplaceAcknowledged(false);
      setError("");
      idempotencyKey.current = null;
    } catch (caught) {
      setError(caught instanceof UploadAPIError ? caught.problem.detail : (caught as Error).message);
    } finally { setBusy(false); }
  };

  return (
    <div className="min-w-0 max-w-4xl">
      <PageHeader
        title={t("upload.title")}
        actions={(
          <Link className={buttonVariants({ variant: "outline" })}
            to="/workspace/repositories/$id/$tab" params={{ id: String(repository.id), tab: "artifacts" }}>
            {t("upload.back")}
          </Link>
        )}
      />

      <div className="mb-4 flex min-w-0 flex-wrap items-center gap-2 text-sm text-muted-foreground">
        <span>{repository.name}</span><Badge>{repository.format}</Badge><Badge variant="secondary">hosted</Badge>
      </div>

      {result ? (
        <Card size="sm">
          <CardContent>
            <div className="flex items-start gap-3">
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
            </div>
          </CardContent>
        </Card>
      ) : (
        <form onSubmit={submit}>
          <Card size="sm" className="mb-4">
            <CardContent>
              <div className="mb-4 flex items-start justify-between gap-3">
                <div><h2 className="m-0 text-base font-semibold">{t("upload.assets")}</h2><p className="mt-1 text-sm text-muted-foreground">{hasFile ? t("upload.assets-help") : t("upload.choose-file-first")}</p></div>
              </div>
              <div className={file
                ? "grid min-w-0 gap-3 rounded-lg border border-border bg-muted/20 p-3 md:grid-cols-[minmax(0,1fr)_8rem_10rem] md:items-end"
                : "min-w-0 rounded-lg border border-border bg-muted/20 p-3"}>
                <Field><FieldLabel htmlFor="upload-file">{t("upload.file")}</FieldLabel><Input id="upload-file" type="file" accept=".jar,.pom,.war,.ear,.aar,.zip,.module,.tar.gz,.asc,.md5,.sha1,.sha256,.xml" disabled={busy} onChange={(e) => selectFile(e.target.files?.[0] ?? null)} /><FieldDescription>{file ? `${file.name} · ${formatFileSize(file.size)}` : t("upload.choose-file")}</FieldDescription></Field>
                {file && <>
                <Field><FieldLabel htmlFor="upload-extension">{t("upload.extension")}</FieldLabel><Input id="upload-extension" value={extension} disabled={busy} onChange={(e) => { setExtension(e.target.value); idempotencyKey.current = null; }} /></Field>
                <Field><FieldLabel htmlFor="upload-classifier">{t("upload.classifier")}</FieldLabel><Input id="upload-classifier" value={classifier} disabled={busy} placeholder={t("common.optional")} aria-invalid={!validClassifier(classifier)} onChange={(e) => { setClassifier(e.target.value); idempotencyKey.current = null; }} />{!validClassifier(classifier) && <FieldDescription className="text-destructive">{t("upload.classifier-invalid")}</FieldDescription>}</Field>
                </>}
              </div>
            </CardContent>
          </Card>

          {hasFile && (
          <Card size="sm" className="mb-4">
            <CardContent>
              <div className="mb-4 flex flex-wrap items-center gap-2">
                <h2 className="m-0 text-base font-semibold">{t("upload.maven-coordinates")}</h2>
                {autoFilledFrom && <Badge variant="secondary">{t("upload.autofilled")}</Badge>}
              </div>
              <FieldGroup className="grid gap-4 md:grid-cols-2">
                <Field><FieldLabel htmlFor="upload-group-id">{t("upload.group-id")}</FieldLabel><Input id="upload-group-id" value={groupId} onChange={(e) => { setGroupId(e.target.value); idempotencyKey.current = null; }} placeholder="com.acme" disabled={busy} /></Field>
                <Field><FieldLabel htmlFor="upload-artifact-id">{t("upload.artifact-id")}</FieldLabel><Input id="upload-artifact-id" value={artifactId} onChange={(e) => { setArtifactId(e.target.value); idempotencyKey.current = null; }} placeholder="widget" disabled={busy} /></Field>
                <Field><FieldLabel htmlFor="upload-version">{t("common.version")}</FieldLabel><Input id="upload-version" value={version} onChange={(e) => { setVersion(e.target.value); idempotencyKey.current = null; }} placeholder="1.0.0" disabled={busy} /></Field>
                <Field><FieldLabel htmlFor="upload-packaging">{t("upload.packaging")}</FieldLabel><Input id="upload-packaging" value={packaging} onChange={(e) => { setPackaging(e.target.value); idempotencyKey.current = null; }} placeholder="jar" disabled={busy} /></Field>
              </FieldGroup>
              <Field orientation="horizontal" className="mt-4">
                <Checkbox id="upload-generate-pom" checked={generatePOM} disabled={busy || hasPOM}
                  onCheckedChange={(checked) => { setGeneratePOM(Boolean(checked)); idempotencyKey.current = null; }} />
                <div><FieldLabel htmlFor="upload-generate-pom">{t("upload.generate-pom")}</FieldLabel><FieldDescription>{hasPOM ? t("upload.pom-supplied") : t("upload.generate-pom-help")}</FieldDescription></div>
              </Field>
            </CardContent>
          </Card>
          )}

          {Object.entries(fieldErrors).map(([field, messages]) => <Alert key={field} className="mb-3">{field}: {messages.join(", ")}</Alert>)}
          {error && <Alert className="mb-4" role="alert">{error}</Alert>}
          {conflict && (
            <Card size="sm" className="mb-4 border-destructive/40">
              <CardContent>
                <h2 className="m-0 text-base font-semibold">{t("upload.replace-title")}</h2>
                <p className="mt-1 text-sm text-muted-foreground">{t("upload.replace-help")}</p>
                <ul className="my-3 max-h-40 overflow-auto rounded-md bg-muted/40 p-3 pl-8 font-mono text-xs">
                  {(conflict.conflicts ?? []).map((path) => <li key={path}>{path}</li>)}
                </ul>
                <Field orientation="horizontal">
                  <Checkbox id="upload-replace-ack" checked={replaceAcknowledged} disabled={busy} onCheckedChange={(checked) => setReplaceAcknowledged(Boolean(checked))} />
                  <FieldLabel htmlFor="upload-replace-ack">{t("upload.replace-ack")}</FieldLabel>
                </Field>
                <div className="mt-4 flex justify-end gap-2">
                  <Button type="button" variant="outline" disabled={busy} onClick={cancelReplacement}>{t("common.cancel")}</Button>
                  <Button type="button" variant="destructive" disabled={busy || !replaceAcknowledged} onClick={confirmReplacement}>{t("upload.confirm-replace")}</Button>
                </div>
              </CardContent>
            </Card>
          )}
          {busy && (
            <div className="mb-4" aria-live="polite">
              <div className="mb-1 flex justify-between text-sm"><span>{t("upload.uploading")}</span><span>{progress > 0 ? `${progress}%` : t("upload.receiving")}</span></div>
              <div className="h-2 overflow-hidden rounded-full bg-muted"><div className="h-full bg-primary transition-[width]" style={{ width: `${progress}%` }} /></div>
            </div>
          )}
          {hasFile && previewPaths.length > 0 && (
            <Card size="sm" className="mb-4">
              <CardContent>
                <h2 className="m-0 mb-2 text-base font-semibold">{t("upload.path-preview")}</h2>
                <ul className="m-0 max-h-40 overflow-auto rounded-md bg-muted/40 p-3 pl-8 font-mono text-xs">
                  {previewPaths.map((path) => <li key={path}>{path}</li>)}
                </ul>
              </CardContent>
            </Card>
          )}
          {hasFile && (
          <div className="flex flex-wrap justify-end gap-2 max-sm:flex-col-reverse">
            {busy && <Button type="button" variant="outline" onClick={() => controller.current?.abort()}>{t("common.cancel")}</Button>}
            <Button type="submit" disabled={busy || Boolean(conflict) || !validClassifier(classifier)}><Upload aria-hidden="true" />{busy ? t("upload.uploading") : t("upload.action")}</Button>
          </div>
          )}
        </form>
      )}
    </div>
  );
}
