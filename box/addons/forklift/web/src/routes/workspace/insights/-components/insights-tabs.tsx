import { Link } from "@tanstack/react-router";

import { useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";

// InsightsTabs switches between the two views under Insights: the security
// analysis and Forklift coverage. Both are routes, so each tab is a link that
// can be shared on its own.
export function InsightsTabs({ active }: { active: "security" | "coverage" }) {
  const { t } = useTranslation();
  const tabClass = (on: boolean) =>
    cn(
      "border-b-2 px-1 pb-2 text-sm transition-colors hover:no-underline",
      on ? "border-accent-ink text-foreground" : "border-transparent text-muted-foreground hover:text-foreground",
    );
  return (
    <nav aria-label={t("nav.insights")} className="mb-5 flex gap-4 border-b border-[var(--fx-border-subtle)]">
      <Link to="/workspace/insights" className={tabClass(active === "security")}>
        {t("insights.tab-security")}
      </Link>
      <Link to="/workspace/coverage" className={tabClass(active === "coverage")}>
        {t("insights.tab-coverage")}
      </Link>
    </nav>
  );
}
