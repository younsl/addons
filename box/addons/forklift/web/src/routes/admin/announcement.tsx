import { createFileRoute } from "@tanstack/react-router";

import { useAuth } from "@/authContext";
import { AnnouncementForm } from "@/components/app-ui/announcement";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import { Redirect } from "@/components/app/redirect";
import { useTranslation } from "@/lib/i18n";

export const Route = createFileRoute("/admin/announcement")({
  component: AdminAnnouncementRoute,
});

function AdminAnnouncementRoute() {
  const { me } = useAuth();
  const { t } = useTranslation();
  // Signed out: the shell redirects to /login, so do not compete with it.
  if (!me.authenticated) return null;
  if (!me.admin) return <Redirect to="/workspace/repositories" replace />;
  return (
    <>
      <PageHeader title={t("announce.title")} />
      <PageDescription>{t("announce.hint")}</PageDescription>
      <AnnouncementForm />
    </>
  );
}
