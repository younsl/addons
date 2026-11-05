import { useLanguage, useTranslation } from "@/lib/i18n";
import { describeTokenExpiry } from "@/routes/workspace/tokens/-utils/token-expiry";

// IdleExpiry shows when the idle reaper may delete a row: the date plus how
// long is left. A past date means the next hourly pass removes it.
export function IdleExpiry({ at }: { at?: string | null }) {
  const { t } = useTranslation();
  const language = useLanguage();
  if (!at) return <span className="text-muted-foreground">-</span>;

  const expiry = describeTokenExpiry(at, language);
  return (
    <span className="whitespace-nowrap text-muted-foreground">
      {at.slice(0, 10)}{" "}
      <span className={expiry.isExpired ? "text-destructive" : undefined}>
        ({expiry.isExpired ? t("repo.auto-delete-due") : expiry.label})
      </span>
    </span>
  );
}
