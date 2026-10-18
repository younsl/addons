import { useEffect, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { BadgeCheck } from "lucide-react";

import { Alert } from "@/components/app-ui/alert";
import { Select } from "@/components/app-ui/select";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Textarea } from "@/components/ui/textarea";
import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { useTranslation, type MessageKey } from "@/lib/i18n";
import { openApiQueryKeys } from "@/query/v1/openapi-query-options";
import { acknowledgeInsightCheck, unacknowledgeInsightCheck } from "@/services/v1/insights/api";
import type { InsightCheck } from "@/services/v1/openapi-types";

const DURATIONS = [7, 30, 90, 180, 365];

// canAcknowledge reports whether the Acknowledge action applies: a failing
// check without an acknowledgement in force, for someone allowed to accept it.
export function canAcknowledge(check: InsightCheck, canAck: boolean): boolean {
  const failing = check.status === "action" || check.status === "investigate";
  return canAck && failing && check.acknowledgement?.state !== "active";
}

// AckButton opens the acknowledge dialog: the reason is required, the period
// bounded, and the check returns on its own when either runs out.
export function AckButton({ check }: { check: InsightCheck }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [open, setOpen] = useState(false);
  const [note, setNote] = useState("");
  const [days, setDays] = useState("30");
  const ack = useMutation({
    mutationFn: () => acknowledgeInsightCheck({ path: { id: check.id }, body: { note: note.trim(), days: Number(days) } }),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: openApiQueryKeys.getInsights() });
      setOpen(false);
    },
  });

  // Each opening starts from an empty reason.
  useEffect(() => {
    if (!open) return;
    setNote("");
    setDays("30");
    ack.reset();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  return (
    <>
      <Button variant="outline" onClick={() => setOpen(true)}>
        <BadgeCheck className="size-4" aria-hidden="true" />
        {t("insights.ack-submit")}
      </Button>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent className="sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>{t("insights.ack-dialog-title").replace("{code}", check.code)}</DialogTitle>
            <DialogDescription>{t("insights.ack-help")}</DialogDescription>
          </DialogHeader>
          {ack.isError && <Alert>{getErrorMessageIfAny(ack.error)}</Alert>}
          <div className="flex min-w-0 flex-col gap-2">
            <label htmlFor={`ack-note-${check.id}`} className="text-sm font-medium">
              {t("insights.ack-note")} <span className="text-destructive">*</span>
              <span className="ml-1 text-xs font-normal text-muted-foreground">{t("insights.ack-note-required")}</span>
            </label>
            <Textarea id={`ack-note-${check.id}`} value={note} rows={4} maxLength={1000} required aria-required="true"
              autoFocus placeholder={t("insights.ack-note-placeholder")} onChange={(e) => setNote(e.target.value)} />
            <label htmlFor={`ack-days-${check.id}`} className="mt-2 text-sm font-medium">{t("insights.ack-period")}</label>
            <Select className="w-48" value={days} onChange={setDays}
              options={DURATIONS.map((d) => ({ value: String(d), label: t("insights.ack-days").replace("{days}", String(d)) }))} />
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setOpen(false)}>{t("common.cancel")}</Button>
            <Button disabled={!note.trim() || ack.isPending} onClick={() => ack.mutate()}>{t("insights.ack-submit")}</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}

// AckPanel shows a check's acknowledgement to everyone who can see the check:
// whether it is in force or why it lapsed, who gave it, until when and why.
// Who may acknowledge also gets the remove action.
export function AckPanel({ check, canAck }: { check: InsightCheck; canAck: boolean }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const unack = useMutation({
    mutationFn: () => unacknowledgeInsightCheck({ path: { id: check.id } }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: openApiQueryKeys.getInsights() }),
  });
  const current = check.acknowledgement;
  if (!current) return null;

  return (
    <section className="mb-5 rounded-md border border-border p-4">
      <h2 className="m-0 mb-2 text-xs font-medium uppercase tracking-wide text-muted-foreground">{t("insights.ack-title")}</h2>
      {unack.isError && <Alert className="mb-3">{getErrorMessageIfAny(unack.error)}</Alert>}
      <div className="text-sm leading-relaxed">
        <div className={current.state === "active" ? "text-[var(--fx-info)]" : "text-[var(--fx-warning)]"}>
          {t(`insights.ack-state-${current.state}` as MessageKey)}
        </div>
        <div className="mt-1 text-muted-foreground">
          {t("insights.ack-by")
            .replace("{user}", current.acknowledged_by)
            .replace("{at}", current.acknowledged_at.slice(0, 10))
            .replace("{until}", current.expires_at.slice(0, 10))
            .replace("{flagged}", String(current.flagged))}
        </div>
        <blockquote className="m-0 mt-2 border-l-2 border-border pl-3 text-foreground">{current.note}</blockquote>
        {canAck && (
          <Button variant="outline" className="mt-3" disabled={unack.isPending} onClick={() => unack.mutate()}>
            {t("insights.ack-remove")}
          </Button>
        )}
      </div>
    </section>
  );
}
