import { useUserPreferences, type Language } from "@/stores/user-preferences";
import en from "@/locales/en.json";
import ko from "@/locales/ko.json";

const messages = { en, ko } satisfies Record<Language, typeof en>;

export type MessageKey = keyof typeof messages.en;

export function useLanguage() {
  return useUserPreferences((state) => state.language);
}

export function useTranslation() {
  const language = useLanguage();
  const t = (key: MessageKey) => messages[language][key] ?? messages.en[key];
  return { language, t };
}

// useDateTime formats timestamps in the language selected in Settings, not the
// browser locale, so an English UI never shows Korean-formatted dates. Returns
// "" for missing values so callers can chain their own fallback.
export function useDateTime() {
  const language = useLanguage();
  return (iso: string | null | undefined) =>
    iso ? new Date(iso).toLocaleString(language === "ko" ? "ko-KR" : "en-US") : "";
}
