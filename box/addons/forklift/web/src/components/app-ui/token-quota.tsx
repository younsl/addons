import { cn } from "@/lib/utils";
import { Card, CardContent } from "@/components/ui/card";
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
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useTranslation } from "@/lib/i18n";

// Per-user access-token cap. Mirrors maxTokensPerUser in src/api/auth.rs;
// the server enforces it (409). Drives the quota display and lets callers
// disable the create action once the quota is used up.
export const MAX_TOKENS_PER_USER = 3;

// Consecutive failed-password threshold that locks an account. Mirrors
// auth.MaxFailedLogins in src/auth/credentials.rs.
export const MAX_FAILED_LOGINS = 5;

// QuotasPanel lists an account's resource quotas in an AWS-console-style table:
// quota name, description, type (hard/soft limit, with a hover explanation),
// current usage and the allocated limit. Currently the only quota is access
// tokens; the table generalizes to more. Shared by the admin user detail page
// and the self-service tokens page so a user can see their own quota.
export function QuotasPanel({ tokenUsed, loginFailures, robot }: { tokenUsed: number; loginFailures?: number; robot?: boolean }) {
  const { t } = useTranslation();
  const quotas = [
    {
      name: t("quota.access-tokens"),
      description: t("quota.access-tokens-desc"),
      type: t("quota.hard-limit"),
      typeDesc: t("quota.hard-limit-desc"),
      current: tokenUsed,
      limit: MAX_TOKENS_PER_USER,
    },
    // Failed-login row only where the caller can know the count (the admin
    // user detail page); the self-service tokens page omits it. Robot accounts
    // cannot log in interactively, so the quota renders as not applicable
    // instead of a misleading 0 / 5.
    ...(loginFailures !== undefined
      ? [{
          name: t("quota.login-failures"),
          description: robot ? t("quota.login-failures-robot-desc") : t("quota.login-failures-desc"),
          type: robot ? t("quota.not-applicable") : t("quota.hard-limit"),
          typeDesc: robot ? t("quota.login-failures-robot-desc") : t("quota.hard-limit-desc"),
          current: robot ? null : loginFailures,
          limit: robot ? null : MAX_FAILED_LOGINS,
        }]
      : []),
  ];
  const { sorted, sort } = useSort(quotas, {
    name: (q) => q.name,
    description: (q) => q.description,
    type: (q) => q.type,
    current: (q) => q.current ?? -1,
    limit: (q) => q.limit ?? -1,
  });
  return (
    <Card size="sm" className="mb-4">
      <CardContent>
        <h2 className="m-0 mb-4 text-base font-semibold">{t("quota.title")}</h2>
        <TableWrap>
          <Table>
            <TableHeader>
              <TableRow>
                <SortableHead k="name" sort={sort}>{t("quota.name")}</SortableHead>
                <SortableHead k="description" sort={sort}>{t("common.description")}</SortableHead>
                <SortableHead k="type" sort={sort}>{t("common.type")}</SortableHead>
                <SortableHead k="current" sort={sort}>{t("quota.current")}</SortableHead>
                <SortableHead k="limit" sort={sort}>{t("quota.limit")}</SortableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {sorted.map((q) => {
                const atCap = q.current !== null && q.limit !== null && q.current >= q.limit;
                return (
                  <TableRow key={q.name}>
                    <TableCell>{q.name}</TableCell>
                    <TableCell className="text-muted-foreground">{q.description}</TableCell>
                    <TableCell>
                      <Tooltip>
                        <TooltipTrigger render={<span tabIndex={0} className="cursor-help text-muted-foreground underline decoration-dotted underline-offset-2" />}>
                          {q.type}
                        </TooltipTrigger>
                        <TooltipContent>{q.typeDesc}</TooltipContent>
                      </Tooltip>
                    </TableCell>
                    <TableCell className={cn("tabular-nums", atCap && "text-destructive", q.current === null && "text-muted-foreground")}>{q.current ?? t("quota.not-applicable")}</TableCell>
                    <TableCell className={cn("tabular-nums", q.limit === null && "text-muted-foreground")}>{q.limit ?? t("quota.not-applicable")}</TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        </TableWrap>
      </CardContent>
    </Card>
  );
}
