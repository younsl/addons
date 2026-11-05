import { Link } from "@tanstack/react-router";
import { ArrowDown, ArrowUp, X } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Table, TableBody, TableCell, TableRow } from "@/components/ui/table";
import { useTranslation } from "@/lib/i18n";

// MemberList renders an ordered member list with reorder and remove controls.
// Shared by the create form and the settings tab.
//
// The order matters: a group resolves a package by asking its members in turn,
// so moving a member up changes which copy wins.
export function MemberList({
  members,
  onChange,
  repoIndex,
  repoTypes,
}: {
  members: string[];
  onChange: (members: string[]) => void;
  // When a member name maps to a repository id, the name links to its page.
  // Absent for a member that no longer exists.
  repoIndex?: Record<string, number>;
  repoTypes?: Record<string, string>;
}) {
  const { t } = useTranslation();

  const move = (index: number, direction: -1 | 1) => {
    const target = index + direction;
    if (target < 0 || target >= members.length) return;

    const next = [...members];
    [next[index], next[target]] = [next[target], next[index]];
    onChange(next);
  };

  if (members.length === 0) {
    return <p className="text-muted-foreground">{t("repo.no-members")}</p>;
  }

  return (
    <Table>
      <TableBody>
        {members.map((name, index) => {
          const id = repoIndex?.[name];
          const type = repoTypes?.[name];

          return (
            <TableRow key={name}>
              <TableCell className="w-6 text-muted-foreground">{index + 1}</TableCell>
              <TableCell className="font-mono text-xs">
                {id !== undefined ? (
                  <Link to="/workspace/repositories/$id" params={{ id: String(id) }}>{name}</Link>
                ) : (
                  name
                )}
              </TableCell>
              <TableCell>
                {type ? (
                  <Badge variant="outline">{type}</Badge>
                ) : (
                  <span className="text-muted-foreground">-</span>
                )}
              </TableCell>
              <TableCell className="whitespace-nowrap text-right">
                <div className="flex justify-end gap-1">
                  <Button
                    variant="outline"
                    size="icon-sm"
                    type="button"
                    disabled={index === 0}
                    title={t("common.move-up")}
                    onClick={() => move(index, -1)}
                  >
                    <ArrowUp className="size-3.5" aria-hidden="true" />
                    <span className="sr-only">{t("common.move-up")}</span>
                  </Button>
                  <Button
                    variant="outline"
                    size="icon-sm"
                    type="button"
                    disabled={index === members.length - 1}
                    title={t("common.move-down")}
                    onClick={() => move(index, 1)}
                  >
                    <ArrowDown className="size-3.5" aria-hidden="true" />
                    <span className="sr-only">{t("common.move-down")}</span>
                  </Button>
                  <Button
                    variant="destructive"
                    size="icon-sm"
                    type="button"
                    title={t("repo.remove-member")}
                    onClick={() => onChange(members.filter((member) => member !== name))}
                  >
                    <X className="size-3.5" aria-hidden="true" />
                    <span className="sr-only">{t("repo.remove-member")}</span>
                  </Button>
                </div>
              </TableCell>
            </TableRow>
          );
        })}
      </TableBody>
    </Table>
  );
}
