import { createFileRoute, Outlet } from "@tanstack/react-router";
import { AnnouncementBanner } from "@/components/app-ui/announcement";
import { useAuth } from "@/authContext";

export const Route = createFileRoute("/workspace")({
  component: WorkspaceLayout,
});

// Every page under /workspace shows the same site-wide announcement banner
// above its content, so a notice is seen no matter which menu the user lands
// on.
function WorkspaceLayout() {
  const { me } = useAuth();
  return (
    <>
      <AnnouncementBanner isAdmin={Boolean(me?.admin)} />
      <Outlet />
    </>
  );
}
