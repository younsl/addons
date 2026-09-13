import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { ACTIONS, ACTION_DESCRIPTION_KEYS, type RoleAction } from "@/lib/role-actions";
import type { RoleActions } from "@/routes/access/roles/-utils/role-permissions";

type ActionPickerProps = {
  selected: RoleActions;
  onToggle: (action: RoleAction) => void;
};

// Two renderings of the same choice. The detail page adds permissions to a role
// that already exists, inline beside the pattern field, so it needs the compact
// row. The create page has a whole card to itself and the user is meeting these
// actions for the first time, so it spells each one out.

export function RoleActionCheckboxes({ selected, onToggle }: ActionPickerProps) {
  const { t } = useTranslation();

  return (
    <>
      {ACTIONS.map((action) => (
        <label
          key={action}
          title={t(ACTION_DESCRIPTION_KEYS[action])}
          className="flex items-center gap-2 text-xs"
        >
          <Checkbox
            checked={selected.includes(action)}
            onCheckedChange={() => onToggle(action)}
          />
          <span>{action}</span>
        </label>
      ))}
    </>
  );
}

export function RoleActionCards({ selected, onToggle }: ActionPickerProps) {
  const { t } = useTranslation();

  return (
    <div className="grid gap-2 sm:grid-cols-2" role="group" aria-label={t("common.actions")}>
      {ACTIONS.map((action) => {
        const isSelected = selected.includes(action);

        return (
          <Button
            key={action}
            type="button"
            variant="ghost"
            role="checkbox"
            aria-checked={isSelected}
            className={cn(
              // One background class applies at a time -- listing two background
              // utilities lets CSS source order pick the winner, which the class
              // string does not reveal. And the unselected card is not dimmed:
              // opacity-55 put five of the seven permission descriptions at
              // 2.2-2.7:1, and choosing a permission means reading what it grants.
              "h-full w-full flex-col items-start justify-start whitespace-normal rounded-lg border px-3.5 py-2.5 text-left text-sm transition-all",
              isSelected
                ? "border-accent-ink bg-primary/10"
                : "border-border bg-muted hover:bg-[var(--fx-surface-3)]",
            )}
            onClick={() => onToggle(action)}
          >
            <div className={cn("mb-0.5 font-semibold", isSelected && "text-accent-ink")}>{action}</div>
            <div className="text-xs leading-relaxed text-muted-foreground">
              {t(ACTION_DESCRIPTION_KEYS[action])}
            </div>
          </Button>
        );
      })}
    </div>
  );
}
