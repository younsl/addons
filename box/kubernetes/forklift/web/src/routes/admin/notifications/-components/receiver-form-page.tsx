import { useState } from "react";
import { LockKeyhole } from "lucide-react";

import { Alert } from "@/components/app-ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { ConfirmModal } from "@/components/overlays/confirm-modal";
import { Field, FieldDescription, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { PageHeader } from "@/components/app-ui/page";
import { Switch } from "@/components/ui/switch";
import { useTranslation } from "@/lib/i18n";
import { LinkedRepositoriesPanel } from "@/routes/admin/notifications/-components/linked-repositories-panel";
import { useReceiverForm } from "@/routes/admin/notifications/-hooks/use-receiver-form";

// ReceiverFormPage is the shared create/edit form for a notification receiver,
// reached from the Receivers list. The webhook URL is write-only: it is never
// returned, so the field starts blank even on an edit - leaving it blank keeps
// the stored URL, a non-empty value replaces it.
export function ReceiverFormPage({ receiverId }: { receiverId?: number }) {
  const { t } = useTranslation();
  const [isConfirmingDelete, setIsConfirmingDelete] = useState(false);
  const receiverForm = useReceiverForm(receiverId);
  const { form, isEditing, linkedRepositories, setForm } = receiverForm;

  if (receiverForm.isLoading) {
    return <div className="text-sm text-muted-foreground">{t("common.loading")}</div>;
  }

  const isDeleteBlocked = linkedRepositories.length > 0;

  return (
    <div data-testid="page-receiver-form">
      <PageHeader title={isEditing ? t("notification.edit") : t("notification.add")} />

      <Card size="sm" className="mb-4 max-w-[44rem]">
        <CardContent>
          <FieldGroup className="gap-4">
            <div className="grid gap-4 md:grid-cols-2">
              <Field>
                <FieldLabel htmlFor="rcv-name">{t("common.name")}</FieldLabel>
                <Input
                  id="rcv-name"
                  value={form.name}
                  placeholder="slack-security"
                  autoFocus
                  onChange={(event) => setForm({ ...form, name: event.target.value })}
                />
              </Field>
              <Field>
                <FieldLabel htmlFor="rcv-desc">{t("common.description")}</FieldLabel>
                <Input
                  id="rcv-desc"
                  value={form.description}
                  placeholder={t("notification.description-placeholder")}
                  onChange={(event) => setForm({ ...form, description: event.target.value })}
                />
              </Field>
            </div>

            <Field>
              <FieldLabel htmlFor="rcv-url">
                {t("common.webhook-url")}
                {isEditing && (
                  <span className="ml-1 text-xs font-normal text-muted-foreground">
                    {t("notification.webhook-subtitle")}
                  </span>
                )}
              </FieldLabel>
              <Input
                id="rcv-url"
                value={form.webhook_url}
                placeholder={isEditing ? "•••••• (unchanged)" : "https://hooks.slack.com/services/…"}
                onChange={(event) => setForm({ ...form, webhook_url: event.target.value })}
              />
              <FieldDescription>{t("notification.webhook-hint")}</FieldDescription>
            </Field>

            <div className="flex min-w-0 items-center gap-3 max-sm:flex-wrap">
              <Button
                variant="outline"
                type="button"
                disabled={receiverForm.isTesting}
                onClick={receiverForm.sendTest}
              >
                {receiverForm.isTesting ? t("common.sending") : t("common.send-test")}
              </Button>
              {receiverForm.testMessage && (
                <span className="text-sm text-muted-foreground">{receiverForm.testMessage}</span>
              )}
              {receiverForm.testError && (
                <span className="text-sm text-destructive">{receiverForm.testError}</span>
              )}
            </div>

            <label className="mt-2.5 inline-flex items-center gap-2.5 text-sm">
              <Switch
                checked={form.enabled}
                onCheckedChange={(enabled) => setForm({ ...form, enabled })}
                aria-label={form.enabled ? t("common.enabled") : t("common.disabled")}
              />
              <span>{form.enabled ? t("common.enabled") : t("common.disabled")}</span>
            </label>
          </FieldGroup>

          {receiverForm.error && <Alert className="mt-4">{receiverForm.error}</Alert>}

          <Card size="sm" className="mt-5 border-accent-ink/70">
            <CardContent>
              <h2 className="mb-2 flex items-center gap-2 text-base font-semibold">
                <LockKeyhole className="size-4 text-accent-ink" aria-hidden="true" />
                {t("notification.webhook-write-only")}
              </h2>
              <p className="m-0 text-sm leading-relaxed text-muted-foreground">
                {t("notification.webhook-warning")}
              </p>
            </CardContent>
          </Card>

          {isEditing && <LinkedRepositoriesPanel repositories={linkedRepositories} />}

          <div className="mt-5 flex min-w-0 items-center gap-2 max-sm:flex-wrap">
            <Button
              type="button"
              disabled={!form.name.trim() || receiverForm.isSaving}
              onClick={receiverForm.save}
            >
              {isEditing ? t("common.save-changes") : t("notification.add")}
            </Button>
            <Button variant="outline" type="button" onClick={receiverForm.cancel}>
              {t("common.cancel")}
            </Button>
          </div>
        </CardContent>
      </Card>
      {isEditing && (
        <Card size="sm" className="mb-4 max-w-[44rem] ring-destructive" data-testid="panel-danger-zone">
          <CardContent>
            <h2 className="m-0 mb-3 text-base font-semibold text-destructive">{t("common.danger-zone")}</h2>
            <p className="mt-0 text-sm leading-relaxed text-muted-foreground">{t("notification.delete-blocked")}</p>
            <Button
              variant="destructive"
              type="button"
              disabled={isDeleteBlocked}
              title={isDeleteBlocked ? t("notification.delete-blocked") : undefined}
              onClick={() => setIsConfirmingDelete(true)}
            >
              {t("common.delete")}
            </Button>
          </CardContent>
        </Card>
      )}
      {isEditing && (
        <ConfirmModal
          open={isConfirmingDelete}
          title={`Delete receiver "${form.name}"?`}
          message={t("notification.delete-confirm")}
          confirmLabel={t("common.delete")}
          danger
          // Typing the name back: deleting a receiver silences alarms for
          // whatever still points at it, and that is not visible from here.
          confirmText={form.name}
          onConfirm={() => { setIsConfirmingDelete(false); receiverForm.remove(); }}
          onCancel={() => setIsConfirmingDelete(false)}
        />
      )}
    </div>
  );
}
