import { createFileRoute } from "@tanstack/react-router";

import { StoragePage } from "@/routes/admin/-storage/components/storage-page";

export const Route = createFileRoute("/admin/storage/")({
  component: StoragePage,
});
