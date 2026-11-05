import { createFileRoute, useParams } from "@tanstack/react-router";
import { useQuery } from "@tanstack/react-query";

import { useAuth } from "@/authContext";
import { Alert } from "@/components/app-ui/alert";
import { Redirect } from "@/components/app/redirect";
import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { useTranslation } from "@/lib/i18n";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { CargoUploadForm } from "@/routes/workspace/repositories/-components/upload/cargo-upload-form";
import { GoUploadForm } from "@/routes/workspace/repositories/-components/upload/go-upload-form";
import { MavenUploadForm } from "@/routes/workspace/repositories/-components/upload/maven-upload-form";
import { NPMUploadForm } from "@/routes/workspace/repositories/-components/upload/npm-upload-form";
import { PyPIUploadForm } from "@/routes/workspace/repositories/-components/upload/pypi-upload-form";
import { RawUploadForm } from "@/routes/workspace/repositories/-components/upload/raw-upload-form";

export const Route = createFileRoute("/workspace/repositories/$id/upload")({
  component: ArtifactUploadRoute,
});

// Each ecosystem publishes differently - Maven wants coordinates, npm a
// tarball, Go a zip plus a .mod - so the route picks the form rather than one
// form branching six ways internally.
function ArtifactUploadRoute() {
  const { id } = useParams({ strict: false }) as { id?: string };
  const { me } = useAuth();
  const { t } = useTranslation();
  const repositoryQuery = useQuery({
    ...openApiQueryOptions.getRepository({ path: { id: Number(id) } }),
    enabled: Number.isFinite(Number(id)),
    meta: { suppressGlobalErrorToast: true },
  });

  const error = getErrorMessageIfAny(repositoryQuery.error);
  if (error) return <Alert>{error}</Alert>;

  const repository = repositoryQuery.data;
  if (!repository) return <div className="text-sm text-muted-foreground">{t("common.loading")}</div>;

  const toArtifacts = (
    <Redirect
      to="/workspace/repositories/$id/$tab"
      params={{ id: String(repository.id), tab: "artifacts" }}
      replace
    />
  );

  // Raw is checked before the upload capability: it has no ecosystem publisher,
  // so it is gated on plain write access instead.
  if (repository.format === "raw" && repository.type === "hosted" && repository.can_write) {
    return <RawUploadForm repository={repository} />;
  }
  if (!repository.capabilities?.upload) return toArtifacts;

  switch (repository.format) {
    case "maven": return <MavenUploadForm repository={repository} csrfToken={me.csrf_token} />;
    case "npm": return <NPMUploadForm repository={repository} csrfToken={me.csrf_token} />;
    case "pypi": return <PyPIUploadForm repository={repository} csrfToken={me.csrf_token} />;
    case "cargo": return <CargoUploadForm repository={repository} csrfToken={me.csrf_token} />;
    case "go": return <GoUploadForm repository={repository} csrfToken={me.csrf_token} />;
    // A format with no publisher (or a proxy/group, which cannot be published
    // to at all) has nothing to show here.
    default: return toArtifacts;
  }
}
