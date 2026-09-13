import { useNavigate } from "@tanstack/react-router";

import { Alert } from "@/components/app-ui/alert";
import { Button } from "@/components/ui/button";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { useTranslation } from "@/lib/i18n";
import { UsersTable } from "@/routes/access/users/-components/users-table";
import { useUsersList } from "@/routes/access/users/-hooks/use-users-list";

import type { Me } from "@/services/v1/openapi-types";

// Admin user directory (read-only). All edits - role mapping, password reset,
// enable/disable, delete - happen on each user's modify page; creation and its
// initial role assignment happen on /access/users/new.
export function UsersPage({ me }: { me: Me }) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const usersQuery = useUsersList();
  const users = usersQuery.data ?? [];
  const error = getErrorMessageIfAny(usersQuery.error);

  return (
    <div data-testid="page-users">
      <PageHeader
        title={
          <span className="flex items-baseline gap-2">
            {t("common.users")}
            <span className="text-base font-normal text-muted-foreground">{users.length}</span>
          </span>
        }
        actions={me.admin && (
          <Button onClick={() => navigate({ to: "/access/users/new" })}>
            {t("user.create")}
          </Button>
        )}
      />
      <PageDescription>{t("user.list-description")}</PageDescription>
      {error && <Alert className="mb-4">{error}</Alert>}

      <UsersTable me={me} users={users} />
    </div>
  );
}
