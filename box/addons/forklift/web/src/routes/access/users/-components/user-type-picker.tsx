import { Bot, Globe, KeyRound, User as UserIcon } from "lucide-react";

import { Button } from "@/components/ui/button";
import { useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";

export type AccountType = "user" | "robot";

// Access paths shown as flat chips inside each type card, so the difference
// between the two is visible rather than only described.
const WEB_CONSOLE = { Icon: Globe, labelKey: "user.access-web-console" } as const;
const ACCESS_TOKEN = { Icon: KeyRound, labelKey: "user.access-token" } as const;

// A robot is a token-only service account (no interactive login); a user signs
// in normally.
const USER_TYPES = [
  {
    value: "user",
    titleKey: "user.type-user",
    descKey: "user.type-user-desc",
    Icon: UserIcon,
    access: [WEB_CONSOLE, ACCESS_TOKEN],
  },
  {
    value: "robot",
    titleKey: "user.type-robot",
    descKey: "user.type-robot-desc",
    Icon: Bot,
    access: [ACCESS_TOKEN],
  },
] as const;

export function UserTypePicker({
  value,
  onChange,
}: {
  value: AccountType;
  onChange: (value: AccountType) => void;
}) {
  const { t } = useTranslation();

  return (
    <div className="grid gap-2 sm:grid-cols-2" role="radiogroup" aria-label={t("common.type")}>
      {USER_TYPES.map((userType) => {
        const isSelected = value === userType.value;

        return (
          <Button
            key={userType.value}
            type="button"
            variant="ghost"
            role="radio"
            aria-checked={isSelected}
            className={cn(
              // One background class applies at a time, and the unselected card is
              // not dimmed -- opacity-55 put its description and access chips at
              // 2.2:1, and these are options that have to be read to be chosen.
              "h-full w-full flex-col items-start justify-start whitespace-normal rounded-lg border px-3.5 py-3 text-left text-sm transition-all",
              isSelected
                ? "border-accent-ink bg-primary/10"
                : "border-border bg-muted hover:bg-[var(--fx-surface-3)]",
            )}
            onClick={() => onChange(userType.value)}
          >
            <div className={cn("mb-1 flex items-center gap-2 font-semibold", isSelected && "text-accent-ink")}>
              <userType.Icon className="size-4 shrink-0" aria-hidden="true" />
              {t(userType.titleKey)}
            </div>
            <div className="text-xs leading-relaxed text-muted-foreground">
              {t(userType.descKey)}
            </div>
            <div className="mt-2.5 flex flex-wrap gap-1.5">
              {userType.access.map((access) => (
                <span
                  key={access.labelKey}
                  // One step above the card, which is now bg-muted; a translucent
                  // muted chip on a muted card is invisible.
                  className="inline-flex items-center gap-1 rounded bg-[var(--fx-surface-3)] px-1.5 py-0.5 text-[11px] font-medium text-muted-foreground"
                >
                  <access.Icon className="size-3 shrink-0" aria-hidden="true" />
                  {t(access.labelKey)}
                </span>
              ))}
            </div>
          </Button>
        );
      })}
    </div>
  );
}
