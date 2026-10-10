import { useEffect, useState, type ReactNode } from "react";
import { createFileRoute, Link, useParams, useSearch } from "@tanstack/react-router";
import { api, OCIArtifactDetail, Repository } from "@/api";
import { Alert } from "@/components/app-ui/alert";
import { ArtifactLabels } from "@/components/app-ui/artifact-labels";
import { Badge } from "@/components/app-ui/badge";
import { CodeView } from "@/components/app-ui/code-view";
import { CopyOnHover } from "@/components/app-ui/copy-button";
import { MarkdownDoc } from "@/components/app-ui/markdown-doc";
import { PageHeader } from "@/components/app-ui/page";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  TableWrap,
} from "@/components/app-ui/table";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { useDateTime, useTranslation } from "@/lib/i18n";
import { formatFileSize } from "@/utils/format-file-size";

// The Harbor-style artifact drill-down as its own page (not a modal): an
// Overview section and kind-specific Additions. The OCI name may contain
// slashes, so it travels as a search parameter rather than a path segment.
export const Route = createFileRoute("/workspace/repositories/$id/oci-artifact")({
  validateSearch: (search: Record<string, unknown>) => ({
    name: String(search.name ?? ""),
    ref: String(search.ref ?? ""),
  }),
  component: OCIArtifactPage,
});

function OCIArtifactPage() {
  const { t } = useTranslation();
  const fmtDate = useDateTime();
  const { id } = useParams({ strict: false }) as { id?: string };
  const { name, ref } = useSearch({ from: "/workspace/repositories/$id/oci-artifact" });
  const repoId = Number(id);
  const [repo, setRepo] = useState<Repository | null>(null);
  const [detail, setDetail] = useState<OCIArtifactDetail | null>(null);
  const [error, setError] = useState("");
  // Kept apart from error: a failed label edit must not replace the whole page
  // with a banner, the artifact it describes is still there to look at.
  const [labelError, setLabelError] = useState("");

  useEffect(() => {
    if (!repoId || !name || !ref) return;
    Promise.all([api.getRepository(repoId), api.getOCIDetail(repoId, name, ref)])
      .then(([r, d]) => { setRepo(r); setDetail(d); })
      .catch((e) => setError((e as Error).message));
  }, [repoId, name, ref]);

  if (error) return <Alert className="my-2.5">{error}</Alert>;
  if (!repo || !detail) return <div className="text-sm text-muted-foreground">{t("common.loading")}</div>;

  const host = window.location.host;
  const reference = `${host}/${repo.name}/${name}${detail.info.tag ? `:${detail.info.tag}` : `@${detail.info.digest}`}`;

  const kv = (label: string, value: ReactNode) => (
    <div className="flex min-w-0 gap-3 text-sm">
      <span className="w-36 shrink-0 text-muted-foreground">{label}</span>
      <span className="min-w-0 break-all">{value}</span>
    </div>
  );

  return (
    <>
      <PageHeader
        title={
          <div className="flex min-w-0 flex-wrap items-center gap-2">
            <CopyOnHover value={`${repo.name}/${name}`}>
              <span className="min-w-0 truncate font-mono">{name}</span>
            </CopyOnHover>
            {detail.info.tag && <Badge variant="outline">{detail.info.tag}</Badge>}
            <Badge>{detail.info.kind}</Badge>
          </div>
        }
        actions={
          <Link to="/workspace/repositories/$id/$tab" params={{ id: String(repoId), tab: "artifacts" }}>
            <Button variant="outline">{t("oci.back-to-artifacts")}</Button>
          </Link>
        }
      />

      {labelError && <Alert className="mb-4">{labelError}</Alert>}

      <Card size="sm" className="mb-4">
        <CardContent>
        <h2 className="m-0 mb-3 text-base font-semibold">{t("oci.overview")}</h2>
        <div className="flex flex-col gap-2">
          {kv(t("oci.digest"), <CopyOnHover value={detail.info.digest} className="font-mono text-xs">{detail.info.digest}</CopyOnHover>)}
          {kv(t("oci.pull"), <CopyOnHover value={reference} className="font-mono text-xs">{reference}</CopyOnHover>)}
          {kv("Media type", <span className="font-mono text-xs">{detail.info.media_type}</span>)}
          {kv(t("common.size"), formatFileSize(detail.info.size))}
          {kv(t("oci.platforms"), detail.info.platforms.length > 0 ? detail.info.platforms.join(", ") : "-")}
          {kv(t("oci.push-time"), fmtDate(detail.info.pushed_at))}
          {kv(t("oci.pushed-by"), detail.info.pushed_by || "-")}
          {kv(t("oci.pull-time"), detail.info.pulled_at ? fmtDate(detail.info.pulled_at) : "-")}
          {kv(t("oci.pulled-by"), detail.info.pulled_by || "-")}
          {kv(t("common.labels"), (
            <ArtifactLabels
              repoId={repoId}
              path={detail.path}
              labels={detail.labels ?? []}
              canLabel={detail.can_label ?? false}
              onError={setLabelError}
            />
          ))}
        </div>
        </CardContent>
      </Card>

      <Card size="sm" className="mb-4">
        <CardContent>
        <h2 className="m-0 mb-3 text-base font-semibold">{t("oci.additions")}</h2>
        {detail.chart ? (
          <Tabs defaultValue={detail.chart.readme_md ? "readme" : "values"}>
            <TabsList variant="line" className="h-9 w-full justify-start gap-5 border-b border-border p-0">
              <TabsTrigger value="readme" className="flex-none px-1 pb-2">README</TabsTrigger>
              <TabsTrigger value="values" className="flex-none px-1 pb-2">values.yaml</TabsTrigger>
              <TabsTrigger value="chartmeta" className="flex-none px-1 pb-2">Chart.yaml</TabsTrigger>
              <TabsTrigger value="manifest" className="flex-none px-1 pb-2">Manifest</TabsTrigger>
            </TabsList>
            <TabsContent value="readme">
              {detail.chart.readme_md
                ? <div className="max-h-[42rem] overflow-auto rounded-md border border-border px-6 py-5"><MarkdownDoc source={detail.chart.readme_md} /></div>
                : <p className="text-sm text-muted-foreground">-</p>}
            </TabsContent>
            <TabsContent value="values">
              {detail.chart.values_yaml ? <CodeView code={detail.chart.values_yaml} language="yaml" /> : <p className="text-sm text-muted-foreground">-</p>}
            </TabsContent>
            <TabsContent value="chartmeta">
              {detail.config_json ? <CodeView code={JSON.stringify(detail.config_json, null, 2)} language="json" /> : <p className="text-sm text-muted-foreground">-</p>}
            </TabsContent>
            <TabsContent value="manifest">
              <CodeView code={JSON.stringify(detail.manifest_json, null, 2)} language="json" />
            </TabsContent>
          </Tabs>
        ) : detail.image ? (
          <Tabs defaultValue="summary">
            <TabsList variant="line" className="h-9 w-full justify-start gap-5 border-b border-border p-0">
              <TabsTrigger value="summary" className="flex-none px-1 pb-2">{t("oci.image-config")}</TabsTrigger>
              <TabsTrigger value="config" className="flex-none px-1 pb-2">Config JSON</TabsTrigger>
              <TabsTrigger value="manifest" className="flex-none px-1 pb-2">Manifest</TabsTrigger>
            </TabsList>
            <TabsContent value="summary">
              <div className="flex flex-col gap-2 py-2">
                {kv("OS / Arch", detail.image.os ? `${detail.image.os}/${detail.image.architecture}` : "-")}
                {kv("Created", detail.image.created && !detail.image.created.startsWith("0001") ? fmtDate(detail.image.created) : "-")}
                {kv("Layers", String(detail.image.layers))}
                {kv("Entrypoint", detail.image.entrypoint?.join(" ") || "-")}
                {kv("Cmd", detail.image.cmd?.join(" ") || "-")}
                {kv("Env", detail.image.env && detail.image.env.length > 0 ? <span className="font-mono text-xs">{detail.image.env.join(" ")}</span> : "-")}
              </div>
            </TabsContent>
            <TabsContent value="config">
              {detail.config_json ? <CodeView code={JSON.stringify(detail.config_json, null, 2)} language="json" /> : <p className="text-sm text-muted-foreground">-</p>}
            </TabsContent>
            <TabsContent value="manifest">
              <CodeView code={JSON.stringify(detail.manifest_json, null, 2)} language="json" />
            </TabsContent>
          </Tabs>
        ) : detail.children && detail.children.length > 0 ? (
          <>
            <TableWrap>
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>{t("oci.digest")}</TableHead>
                    <TableHead>{t("oci.platforms")}</TableHead>
                    <TableHead>{t("common.size")}</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {detail.children.map((c) => (
                    <TableRow key={c.digest}>
                      <TableCell className="font-mono text-xs"><CopyOnHover value={c.digest}>{c.digest.slice(7, 19)}</CopyOnHover></TableCell>
                      <TableCell>{c.platform || "-"}</TableCell>
                      <TableCell>{formatFileSize(c.size)}</TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </TableWrap>
            <h3 className="mb-1 mt-4 text-sm font-semibold">Manifest</h3>
            <CodeView code={JSON.stringify(detail.manifest_json, null, 2)} language="json" />
          </>
        ) : (
          <p className="mt-0 mb-0 text-sm text-muted-foreground">{t("oci.no-additions")}</p>
        )}
        </CardContent>
      </Card>
    </>
  );
}
