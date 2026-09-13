import { useEffect, useState } from "react";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Input } from "@/components/ui/input";
import { useTranslation } from "@/lib/i18n";

// In-app confirmation modal. The app never uses native dialogs (alert/confirm/
// prompt); all confirmations render through this component.
//
// When confirmText is set, the modal becomes a type-to-confirm guard: the user
// must type that exact value (e.g. a receiver name) before the confirm button
// enables. Used for destructive actions that should not be a single click.
export function ConfirmModal({
  open,
  title,
  message,
  confirmLabel,
  danger,
  confirmText,
  onConfirm,
  onCancel,
}: {
  open: boolean;
  title: string;
  message?: string;
  confirmLabel?: string;
  danger?: boolean;
  confirmText?: string;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const { t } = useTranslation();
  const [typed, setTyped] = useState("");
  // Reset the typed value whenever the modal opens (or targets a new value) so a
  // prior entry never carries over to the next deletion.
  useEffect(() => { setTyped(""); }, [open, confirmText]);

  const needsMatch = !!confirmText;
  const matched = !needsMatch || typed === confirmText;

  return (
    <AlertDialog open={open} onOpenChange={(next) => { if (!next) onCancel(); }}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{title}</AlertDialogTitle>
          {message && <AlertDialogDescription>{message}</AlertDialogDescription>}
        </AlertDialogHeader>
        {needsMatch && (
          <div className="space-y-1.5">
            <p className="m-0 text-sm text-muted-foreground">
              {t("common.type-to-confirm")} <span className="font-mono font-semibold text-foreground">{confirmText}</span>
            </p>
            <Input value={typed} autoFocus placeholder={confirmText}
              onChange={(e) => setTyped(e.target.value)} />
          </div>
        )}
        <AlertDialogFooter>
          <AlertDialogCancel type="button" onClick={onCancel}>
            {t("common.cancel")}
          </AlertDialogCancel>
          <AlertDialogAction
            variant={danger ? "destructive" : "default"}
            type="button"
            disabled={!matched}
            onClick={() => { if (matched) onConfirm(); }}
          >
            {confirmLabel ?? t("common.confirm")}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
