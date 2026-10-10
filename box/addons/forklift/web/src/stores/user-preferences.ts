import { create } from "zustand";
import { persist, createJSONStorage } from "zustand/middleware";

export type ThemeMode = "system" | "dark" | "light";
export type Language = "en" | "ko";
export type ContentWidthMode = "normal" | "wide";

export interface UserPreferenceState {
  themeMode: ThemeMode;
  language: Language;
  contentWidthMode: ContentWidthMode;
  isSidebarCollapsed: boolean;
}

export interface UserPreferenceActions {
  setThemeMode: (themeMode: ThemeMode) => void;
  setLanguage: (language: Language) => void;
  setContentWidthMode: (contentWidthMode: ContentWidthMode) => void;
  toggleSidebar: () => void;
}

// Actions live in a nested object that is never replaced, so a component that
// only writes - the sidebar's collapse button - subscribes to something stable
// and does not re-render when a preference changes.
export interface UserPreferenceStore extends UserPreferenceState {
  actions: UserPreferenceActions;
}

export const USER_PREFERENCE_STORAGE_KEY = "forklift.user-preferences.v1";
export const USER_PREFERENCE_STORAGE_VERSION = 1;

// The keys this replaces. Each preference had its own entry, read through a
// bespoke helper and announced with a CustomEvent nothing else could see.
export const LEGACY_STORAGE_KEYS = {
  themeMode: "forklift.theme",
  language: "forklift.language",
  contentWidthMode: "forklift.content-width",
  isSidebarCollapsed: "forklift.sidebar-collapsed",
} as const;

export const DEFAULT_USER_PREFERENCES: UserPreferenceState = {
  themeMode: "system",
  language: "en",
  contentWidthMode: "wide",
  isSidebarCollapsed: false,
};

const themeModes = ["system", "dark", "light"] satisfies ThemeMode[];
const languages = ["en", "ko"] satisfies Language[];
const contentWidthModes = ["normal", "wide"] satisfies ContentWidthMode[];

// Runs before the store is created, so a returning user's existing preferences
// are already under the new key by the time persist reads it.
migrateLegacyStorage();

export const useUserPreferences = create<UserPreferenceStore>()(
  persist(
    (set) => ({
      ...DEFAULT_USER_PREFERENCES,
      actions: {
        setThemeMode: (themeMode) => set({ themeMode }),
        setLanguage: (language) => set({ language }),
        setContentWidthMode: (contentWidthMode) => set({ contentWidthMode }),
        toggleSidebar: () =>
          set((state) => ({ isSidebarCollapsed: !state.isSidebarCollapsed })),
      },
    }),
    {
      name: USER_PREFERENCE_STORAGE_KEY,
      version: USER_PREFERENCE_STORAGE_VERSION,
      storage: createJSONStorage(() => localStorage),
      // Actions are not state to be restored; without this they would be
      // written to storage as an empty object and read back over the real ones.
      partialize: (state) => ({
        themeMode: state.themeMode,
        language: state.language,
        contentWidthMode: state.contentWidthMode,
        isSidebarCollapsed: state.isSidebarCollapsed,
      }),
      // Anything stored can be edited by hand or left over from an older
      // build, so every field is checked rather than trusted.
      merge: (persisted, current) => ({
        ...current,
        ...parsePreferences(persisted),
      }),
    },
  ),
);

export function useUserPreferenceActions(): UserPreferenceActions {
  return useUserPreferences((state) => state.actions);
}

// Applies the preferences that live outside React - the theme class and the
// document language - and keeps them applied. Called once, from main.tsx.
//
// This is a subscription rather than an effect in a component because the
// document element is not owned by any screen: an effect would re-run on
// every mount and would stop working the moment the owning screen unmounted.
export function bindUserPreferenceEffects(): () => void {
  applyThemeMode(useUserPreferences.getState().themeMode);
  applyLanguage(useUserPreferences.getState().language);

  const unsubscribe = useUserPreferences.subscribe((state, previous) => {
    if (state.themeMode !== previous.themeMode) applyThemeMode(state.themeMode);
    if (state.language !== previous.language) applyLanguage(state.language);
  });

  // "system" resolves against the OS setting, which can change while the tab
  // is open.
  const media = systemThemeMedia();
  const onSystemThemeChange = () => {
    if (useUserPreferences.getState().themeMode === "system") applyThemeMode("system");
  };
  media?.addEventListener("change", onSystemThemeChange);

  // Another tab writing the same key. persist does not listen for this on its
  // own, so without it two open tabs drift apart until one is reloaded.
  const onStorage = (event: StorageEvent) => {
    if (event.key !== USER_PREFERENCE_STORAGE_KEY && event.key !== null) return;
    void useUserPreferences.persist.rehydrate();
  };
  window.addEventListener("storage", onStorage);

  return () => {
    unsubscribe();
    media?.removeEventListener("change", onSystemThemeChange);
    window.removeEventListener("storage", onStorage);
  };
}

export function resolveThemeMode(themeMode: ThemeMode): Exclude<ThemeMode, "system"> {
  if (themeMode !== "system") return themeMode;

  return systemThemeMedia()?.matches ? "light" : "dark";
}

// Moves the four legacy keys into one, and only deletes them once the write
// has been read back and compared. Storage can fail silently - Safari in
// private mode accepts setItem and stores nothing - and deleting first would
// lose the preferences of exactly the users this exists for.
//
// Returns whether anything was migrated, which is what the tests assert on.
export function migrateLegacyStorage(storage: Storage | null = safeLocalStorage()): boolean {
  if (!storage) return false;

  try {
    if (storage.getItem(USER_PREFERENCE_STORAGE_KEY) !== null) return false;

    const legacy = readLegacyPreferences(storage);
    if (Object.keys(legacy).length === 0) return false;

    storage.setItem(
      USER_PREFERENCE_STORAGE_KEY,
      JSON.stringify({ state: legacy, version: USER_PREFERENCE_STORAGE_VERSION }),
    );

    const written = storage.getItem(USER_PREFERENCE_STORAGE_KEY);
    if (!written) return false;

    const parsed = JSON.parse(written) as unknown;
    if (!isRecord(parsed) || parsed.version !== USER_PREFERENCE_STORAGE_VERSION) return false;
    if (!isRecord(parsed.state)) return false;

    const readBack = parsePreferences(parsed.state);
    for (const [key, value] of Object.entries(legacy)) {
      if (readBack[key as keyof UserPreferenceState] !== value) return false;
    }

    for (const legacyKey of Object.values(LEGACY_STORAGE_KEYS)) storage.removeItem(legacyKey);

    return true;
  } catch {
    return false;
  }
}

export function parsePreferences(value: unknown): Partial<UserPreferenceState> {
  if (!isRecord(value)) return {};

  return {
    ...(isThemeMode(value.themeMode) ? { themeMode: value.themeMode } : {}),
    ...(isLanguage(value.language) ? { language: value.language } : {}),
    ...(isContentWidthMode(value.contentWidthMode)
      ? { contentWidthMode: value.contentWidthMode }
      : {}),
    ...(typeof value.isSidebarCollapsed === "boolean"
      ? { isSidebarCollapsed: value.isSidebarCollapsed }
      : {}),
  };
}

function readLegacyPreferences(storage: Storage): Partial<UserPreferenceState> {
  const themeMode = storage.getItem(LEGACY_STORAGE_KEYS.themeMode);
  const language = storage.getItem(LEGACY_STORAGE_KEYS.language);
  const contentWidthMode = storage.getItem(LEGACY_STORAGE_KEYS.contentWidthMode);
  const isSidebarCollapsed = storage.getItem(LEGACY_STORAGE_KEYS.isSidebarCollapsed);

  return {
    ...(isThemeMode(themeMode) ? { themeMode } : {}),
    ...(isLanguage(language) ? { language } : {}),
    ...(isContentWidthMode(contentWidthMode) ? { contentWidthMode } : {}),
    ...(isSidebarCollapsed === "true" ? { isSidebarCollapsed: true } : {}),
    ...(isSidebarCollapsed === "false" ? { isSidebarCollapsed: false } : {}),
  };
}

function applyThemeMode(themeMode: ThemeMode) {
  const resolved = resolveThemeMode(themeMode);
  document.documentElement.classList.toggle("dark", resolved === "dark");
  document.documentElement.classList.toggle("light", resolved === "light");
}

function applyLanguage(language: Language) {
  document.documentElement.lang = language;
}

function systemThemeMedia(): MediaQueryList | null {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") return null;

  return window.matchMedia("(prefers-color-scheme: light)");
}

function safeLocalStorage(): Storage | null {
  try {
    return typeof window === "undefined" ? null : window.localStorage;
  } catch {
    return null;
  }
}

function isThemeMode(value: unknown): value is ThemeMode {
  return typeof value === "string" && themeModes.includes(value as ThemeMode);
}

function isLanguage(value: unknown): value is Language {
  return typeof value === "string" && languages.includes(value as Language);
}

function isContentWidthMode(value: unknown): value is ContentWidthMode {
  return typeof value === "string" && contentWidthModes.includes(value as ContentWidthMode);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
