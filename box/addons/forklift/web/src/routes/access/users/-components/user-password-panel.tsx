import { useState } from "react";
import { Eye, EyeOff } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { getErrorMessage } from "@/lib/http/error/api-error";
import { useTranslation } from "@/lib/i18n";
import { useUpdateUserMutation } from "@/routes/access/users/-hooks/use-user-mutations";

import type { User } from "@/services/v1/openapi-types";

export function UserPasswordPanel({
  user,
  onError,
}: {
  user: User;
  onError: (message: string) => void;
}) {
  const { t } = useTranslation();
  const [password, setPassword] = useState("");
  const [isVisible, setIsVisible] = useState(false);
  const [isSaved, setIsSaved] = useState(false);
  const updateUserMutation = useUpdateUserMutation();

  const reset = () => {
    onError("");
    setIsSaved(false);
    updateUserMutation.mutate(
      { userId: user.id, body: { password } },
      {
        onSuccess: () => {
          // Cleared on success only: after a rejection the typed password is
          // still the one the admin meant, and retyping it invites a typo.
          setPassword("");
          setIsSaved(true);
        },
        onError: (caught) => onError(getErrorMessage(caught)),
      },
    );
  };

  return (
    <Card size="sm" className="mb-4" data-testid="panel-password">
      <CardContent>
        <h2 className="m-0 mb-4 text-base font-semibold">{t("common.password")}</h2>
        <Field>
          <FieldLabel>{t("common.new-password")}</FieldLabel>
          <div className="relative">
            <Input
              className="pr-16"
              type={isVisible ? "text" : "password"}
              value={password}
              onChange={(event) => { setPassword(event.target.value); setIsSaved(false); }}
            />
            <Button
              type="button"
              variant="ghost"
              size="icon"
              className="absolute right-0 top-0 h-full rounded-l-none text-muted-foreground"
              onClick={() => setIsVisible((current) => !current)}
              aria-label={isVisible ? t("common.hide-password") : t("common.show-password")}
            >
              {isVisible
                ? <EyeOff className="size-4" aria-hidden="true" />
                : <Eye className="size-4" aria-hidden="true" />}
            </Button>
          </div>
        </Field>
        <div className="mt-4 flex min-w-0 items-center gap-2 max-sm:flex-col max-sm:flex-wrap max-sm:items-stretch">
          <Button
            type="button"
            disabled={!password || updateUserMutation.isPending}
            onClick={reset}
          >
            {t("user.reset-password")}
          </Button>
          {isSaved && (
            <span className="text-sm text-muted-foreground">{t("user.password-updated")}</span>
          )}
        </div>
      </CardContent>
    </Card>
  );
}
