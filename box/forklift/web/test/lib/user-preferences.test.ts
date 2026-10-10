import { beforeEach, describe, expect, test } from "vitest";

import {
  DEFAULT_USER_PREFERENCES,
  LEGACY_STORAGE_KEYS,
  USER_PREFERENCE_STORAGE_KEY,
  migrateLegacyStorage,
  parsePreferences,
  useUserPreferences,
} from "@/stores/user-preferences";

beforeEach(() => {
  // Order matters: setting the store writes through to storage, so the reset
  // has to come first and the clear after it.
  useUserPreferences.setState(DEFAULT_USER_PREFERENCES);
  window.localStorage.clear();
});

describe("the preference store", () => {
  test("writing a preference persists it under one key", () => {
    useUserPreferences.getState().actions.setThemeMode("dark");

    expect(useUserPreferences.getState().themeMode).toBe("dark");
    const stored = JSON.parse(window.localStorage.getItem(USER_PREFERENCE_STORAGE_KEY)!);
    expect(stored.state.themeMode).toBe("dark");
  });

  // The actions object is what components subscribe to when they only write.
  // If it were rebuilt on each change, every such component would re-render.
  test("the actions object survives a state change", () => {
    const before = useUserPreferences.getState().actions;

    useUserPreferences.getState().actions.setLanguage("ko");

    expect(useUserPreferences.getState().actions).toBe(before);
  });

  test("toggling the sidebar flips it", () => {
    useUserPreferences.getState().actions.toggleSidebar();
    expect(useUserPreferences.getState().isSidebarCollapsed).toBe(true);

    useUserPreferences.getState().actions.toggleSidebar();
    expect(useUserPreferences.getState().isSidebarCollapsed).toBe(false);
  });

  // Actions are functions; persisting them would write "{}" and read it back
  // over the real ones on the next load.
  test("the actions are not persisted", () => {
    useUserPreferences.getState().actions.setLanguage("ko");

    const stored = JSON.parse(window.localStorage.getItem(USER_PREFERENCE_STORAGE_KEY)!);
    expect(stored.state).not.toHaveProperty("actions");
  });
});

describe("reading what was stored", () => {
  test("a value outside the allowed set is dropped, not adopted", () => {
    expect(parsePreferences({ themeMode: "neon", language: "ko" })).toEqual({ language: "ko" });
  });

  test("anything that is not an object reads as empty", () => {
    expect(parsePreferences("dark")).toEqual({});
    expect(parsePreferences(null)).toEqual({});
    expect(parsePreferences(["dark"])).toEqual({});
  });
});

describe("migrating the legacy keys", () => {
  test("the four old keys become one, and the old ones are removed", () => {
    window.localStorage.setItem(LEGACY_STORAGE_KEYS.themeMode, "dark");
    window.localStorage.setItem(LEGACY_STORAGE_KEYS.language, "ko");
    window.localStorage.setItem(LEGACY_STORAGE_KEYS.contentWidthMode, "normal");
    window.localStorage.setItem(LEGACY_STORAGE_KEYS.isSidebarCollapsed, "true");

    expect(migrateLegacyStorage(window.localStorage)).toBe(true);

    const stored = JSON.parse(window.localStorage.getItem(USER_PREFERENCE_STORAGE_KEY)!);
    expect(stored.state).toEqual({
      themeMode: "dark",
      language: "ko",
      contentWidthMode: "normal",
      isSidebarCollapsed: true,
    });
    for (const key of Object.values(LEGACY_STORAGE_KEYS)) {
      expect(window.localStorage.getItem(key)).toBeNull();
    }
  });

  test("an existing new key is left alone", () => {
    window.localStorage.setItem(
      USER_PREFERENCE_STORAGE_KEY,
      JSON.stringify({ state: { language: "en" }, version: 1 }),
    );
    window.localStorage.setItem(LEGACY_STORAGE_KEYS.language, "ko");

    expect(migrateLegacyStorage(window.localStorage)).toBe(false);

    const stored = JSON.parse(window.localStorage.getItem(USER_PREFERENCE_STORAGE_KEY)!);
    expect(stored.state.language).toBe("en");
  });

  test("nothing to migrate is not a migration", () => {
    expect(migrateLegacyStorage(window.localStorage)).toBe(false);
    expect(window.localStorage.getItem(USER_PREFERENCE_STORAGE_KEY)).toBeNull();
  });

  // Safari in private mode accepts setItem and stores nothing. Deleting the
  // legacy keys on the strength of a write that did not land would lose the
  // preferences of exactly the users this migration exists for.
  test("storage that accepts a write but stores nothing keeps the old keys", () => {
    const storage = {
      getItem: (key: string) => (key === LEGACY_STORAGE_KEYS.language ? "ko" : null),
      setItem: () => {},
      removeItem: () => {
        throw new Error("must not remove the legacy keys after a failed write");
      },
    } as unknown as Storage;

    expect(migrateLegacyStorage(storage)).toBe(false);
  });

  test("a storage that throws is survived", () => {
    const storage = {
      getItem: () => {
        throw new Error("storage disabled");
      },
    } as unknown as Storage;

    expect(migrateLegacyStorage(storage)).toBe(false);
  });
});
