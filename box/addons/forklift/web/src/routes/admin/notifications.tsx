import { createFileRoute, Outlet } from "@tanstack/react-router";
import { useAuth } from "@/authContext";
import { Redirect } from "@/components/app/redirect";

export const Route = createFileRoute("/admin/notifications")({
  component: AdminNotificationsLayout,
});

function AdminNotificationsLayout() {
  const { me } = useAuth();
  // Signed out: the shell redirects to /login, so do not compete with it.
  if (!me.authenticated) return null;
  if (!me.admin) return <Redirect to="/workspace/repositories" replace />;
  return <Outlet />;
}
