import { createContext, useContext } from "react";
import type { Me } from "@/services/v1/openapi-types";

interface AuthContextValue {
  me: Me;
}

// The default is a signed-out principal rather than null, and useAuth does not
// throw when it sees it.
//
// The provider is mounted in exactly one place - the application shell - so the
// "did you forget the provider?" error it used to raise was never guarding
// against a real mistake. What it did instead was crash the app on sign-out:
// the shell stops providing the principal the moment the session ends, while
// the router still has the workspace layout mounted for another render, and
// that layout calls useAuth. The screen a user was leaving anyway took down the
// tree on the way out.
//
// Screens read this defensively already (`Boolean(me?.admin)`), so an
// unauthenticated principal renders a harmless empty frame for the one render
// before the redirect lands.
const AuthContext = createContext<AuthContextValue>({ me: { authenticated: false } });

export const AuthProvider = AuthContext.Provider;

export function useAuth() {
  return useContext(AuthContext);
}
