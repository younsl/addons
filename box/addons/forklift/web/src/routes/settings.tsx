import { type ReactNode } from "react";
import { createFileRoute } from "@tanstack/react-router";
import { Info } from "lucide-react";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Card, CardContent } from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import { Separator } from "@/components/ui/separator";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { cn } from "@/lib/utils";
import {
  useUserPreferences,
  useUserPreferenceActions,
  type ContentWidthMode,
  type Language,
  type ThemeMode,
} from "@/stores/user-preferences";
import { useTranslation } from "@/lib/i18n";

export const Route = createFileRoute("/settings")({
  component: SettingsRoute,
});

export function SettingsRoute() {
  const { t } = useTranslation();
  const contentWidth = useUserPreferences((state) => state.contentWidthMode);
  const theme = useUserPreferences((state) => state.themeMode);
  const language = useUserPreferences((state) => state.language);
  const { setContentWidthMode, setLanguage, setThemeMode } = useUserPreferenceActions();
  const contentWidthOptions = [
    { value: "wide", label: t("common.view-wide") },
    { value: "normal", label: t("common.view-normal") },
  ] satisfies { value: ContentWidthMode; label: string }[];
  const themeOptions = [
    { value: "system", label: t("settings.theme.system") },
    { value: "dark", label: t("settings.theme.dark") },
    { value: "light", label: t("settings.theme.light") },
  ] satisfies { value: ThemeMode; label: string }[];
  const languageOptions = [
    { value: "en", label: t("settings.language.en") },
    { value: "ko", label: t("settings.language.ko") },
  ] satisfies { value: Language; label: string }[];
  const isWide = contentWidth === "wide";
  const selectTriggerClassName = cn(
    "w-full transition-[width] duration-150",
    isWide ? "sm:w-[280px] lg:w-[320px]" : "sm:w-[220px]"
  );

  // The store is the only copy: writing it re-renders this screen, applies the
  // theme class and document language, and persists - so there is nothing left
  // for a local useState to hold.
  const onThemeChange = (next: string) => setThemeMode(next as ThemeMode);
  const onLanguageChange = (next: string) => setLanguage(next as Language);
  const onContentWidthChange = (next: string) => setContentWidthMode(next as ContentWidthMode);

  return (
    <div
      data-testid="page-settings"
      className={cn("transition-[max-width] duration-150", isWide ? "max-w-none" : "max-w-[760px]")}
    >
      <div className="mb-5">
        <h1 className="m-0 text-2xl leading-tight font-semibold tracking-normal max-sm:text-xl">
          {t("settings.title")}
        </h1>
        <p className="mb-0 mt-2 text-sm leading-relaxed text-muted-foreground">
          {t("settings.description")}
        </p>
      </div>

      <Alert className="mb-4 border-border/80 bg-muted/40">
        <Info className="size-4" aria-hidden="true" />
        <AlertTitle>{t("settings.card-title")}</AlertTitle>
        <AlertDescription>{t("settings.card-description")}</AlertDescription>
      </Alert>

      <Card className="border-border/90 bg-card/95 shadow-none">
        <CardContent className="pt-6">
          <SettingRow
            id="content-width"
            title={t("settings.content-width")}
            description={t("settings.content-width-description")}
            wide={isWide}
          >
            <Select
              items={contentWidthOptions}
              value={contentWidth}
              onValueChange={(next) => next && onContentWidthChange(next)}
            >
              <SelectTrigger id="content-width" className={selectTriggerClassName}>
                <SelectValue />
              </SelectTrigger>
              <SelectContent align="start">
                {contentWidthOptions.map((option) => (
                  <SelectItem key={option.value} value={option.value}>
                    {option.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </SettingRow>

          <Separator />

          <SettingRow
            id="appearance"
            title={t("settings.appearance")}
            description={t("settings.appearance-description")}
            wide={isWide}
          >
            <Select
              items={themeOptions}
              value={theme}
              onValueChange={(next) => next && onThemeChange(next)}
            >
              <SelectTrigger id="appearance" className={selectTriggerClassName}>
                <SelectValue />
              </SelectTrigger>
              <SelectContent align="start">
                {themeOptions.map((option) => (
                  <SelectItem key={option.value} value={option.value}>
                    {option.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </SettingRow>

          <Separator />

          <SettingRow
            id="language"
            title={t("settings.language")}
            description={t("settings.language-description")}
            wide={isWide}
          >
            <Select
              items={languageOptions}
              value={language}
              onValueChange={(next) => next && onLanguageChange(next)}
            >
              <SelectTrigger id="language" className={selectTriggerClassName}>
                <SelectValue />
              </SelectTrigger>
              <SelectContent align="start">
                {languageOptions.map((option) => (
                  <SelectItem key={option.value} value={option.value}>
                    {option.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </SettingRow>
        </CardContent>
      </Card>
    </div>
  );
}

function SettingRow({
  id,
  title,
  description,
  wide = false,
  children,
}: {
  id: string;
  title: string;
  description: string;
  wide?: boolean;
  children: ReactNode;
}) {
  return (
    <div
      className={cn(
        "grid gap-3 py-4 first:pt-0 last:pb-0 sm:items-center",
        wide ? "sm:grid-cols-[minmax(0,1fr)_320px]" : "sm:grid-cols-[1fr_240px]"
      )}
    >
      <div className="min-w-0">
        <Label htmlFor={id}>{title}</Label>
        <p className="mb-0 mt-1 text-sm leading-relaxed text-muted-foreground">{description}</p>
      </div>
      <div className="min-w-0 sm:justify-self-end">{children}</div>
    </div>
  );
}
