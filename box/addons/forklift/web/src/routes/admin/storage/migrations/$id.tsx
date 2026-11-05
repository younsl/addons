import { createFileRoute, useParams } from "@tanstack/react-router";

import { MigrationDetailPage } from "@/routes/admin/-storage/components/migration-detail";

export const Route = createFileRoute("/admin/storage/migrations/$id")({
  component: AdminStorageMigrationRoute,
});

function AdminStorageMigrationRoute() {
  const { id } = useParams({ from: "/admin/storage/migrations/$id" });
  return <MigrationDetailPage id={id} />;
}
