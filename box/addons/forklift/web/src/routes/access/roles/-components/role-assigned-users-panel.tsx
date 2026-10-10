import { Link } from "@tanstack/react-router";

import { Badge } from "@/components/app-ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { StateBadge } from "@/components/app-ui/status-badge";
import {
  SortableHead,
  Table,
  TableBody,
  TableCell,
  TableHeader,
  TableRow,
  TableWrap,
  useSort,
} from "@/components/app-ui/table";
import { useDateTime, useTranslation } from "@/lib/i18n";

import type { User } from "@/services/v1/openapi-types";

// RoleAssignedUsersPanel lists the users that currently hold this role.
// Assignment itself is managed on each user's detail page, so this is read-only
// with links.
export function RoleAssignedUsersPanel({ members }: { members: User[] }) {
  const { t } = useTranslation();
  const fmtDate = useDateTime();
  const { sorted, sort } = useSort(members, {
    username: (user) => user.username,
    source: (user) => user.source,
    email: (user) => user.email,
    roles: (user) => user.roles.map((role) => role.name).join(", "),
    status: (user) => (user.disabled ? "disabled" : "active"),
    lastLogin: (user) => user.last_login_at,
  });

  return (
    <Card size="sm" className="mb-4" data-testid="panel-assigned-users">
      <CardContent>
        <h2 className="m-0 mb-4 text-base font-semibold">
          {t("role.assigned-users")}{" "}
          <Badge className="ml-1.5 tabular-nums">{members.length}</Badge>
        </h2>
        {members.length === 0 ? (
          <p className="m-0 text-sm text-muted-foreground">{t("role.no-users")}</p>
        ) : (
          // Same column structure and order as the Users page; the username
          // links to that user's detail page.
          <TableWrap>
            <Table>
              <TableHeader>
                <TableRow>
                  <SortableHead k="username" sort={sort}>{t("common.username")}</SortableHead>
                  <SortableHead k="source" sort={sort}>{t("common.source")}</SortableHead>
                  <SortableHead k="email" sort={sort}>{t("common.email")}</SortableHead>
                  <SortableHead k="roles" sort={sort}>{t("common.roles")}</SortableHead>
                  <SortableHead k="status" sort={sort}>{t("common.status")}</SortableHead>
                  <SortableHead k="lastLogin" sort={sort}>{t("common.last-login")}</SortableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {sorted.map((user) => (
                  <TableRow key={user.id}>
                    <TableCell className="whitespace-nowrap">
                      <Link to="/access/users/$id" params={{ id: String(user.id) }}>
                        {user.username}
                      </Link>
                    </TableCell>
                    <TableCell><Badge>{user.source}</Badge></TableCell>
                    <TableCell className="text-muted-foreground">{user.email || "-"}</TableCell>
                    <TableCell>
                      <div className="flex min-w-0 flex-wrap items-center gap-1.5">
                        {user.roles.map((role) => (
                          <Badge
                            key={role.id}
                            render={<Link to="/access/roles/$id" params={{ id: String(role.id) }} />}
                          >
                            {role.name}
                          </Badge>
                        ))}
                        {user.roles.length === 0 && (
                          <span className="text-muted-foreground">{t("common.none")}</span>
                        )}
                      </div>
                    </TableCell>
                    <TableCell>
                      {user.disabled ? (
                        <StateBadge state="disabled">{t("common.status.disabled")}</StateBadge>
                      ) : (
                        <StateBadge state="active">{t("common.status.active")}</StateBadge>
                      )}
                    </TableCell>
                    <TableCell
                      className="whitespace-nowrap text-muted-foreground"
                      title={user.last_login_at ?? undefined}
                    >
                      {user.last_login_at ? fmtDate(user.last_login_at) : t("common.never")}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </TableWrap>
        )}
      </CardContent>
    </Card>
  );
}
