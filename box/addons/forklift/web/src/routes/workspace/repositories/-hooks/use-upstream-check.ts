import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";

import { postCheckUpstreamRepositories } from "@/services/v1/repositories/api";

import type { UpstreamAuthConfig } from "@/services/v1/openapi-types";

// How long the URL has to stop changing before it is probed. Long enough that
// typing a registry address does not fire a request per keystroke, short enough
// that the answer arrives while the field still has focus.
const DEBOUNCE_MS = 600;

// useUpstreamCheck probes a typed upstream URL and reports whether it answers.
// It is a query rather than a mutation despite the endpoint being a POST: the
// result is a fact about the URL, cached under it, so going back to an address
// already tried answers from cache instead of probing again.
//
// The key is hand-written. The generator only builds keys for GET operations,
// which is right - it cannot know that this particular POST is a read.
export function useUpstreamCheck({
  url,
  auth,
  enabled,
}: {
  url: string;
  auth: UpstreamAuthConfig;
  enabled: boolean;
}) {
  const debouncedUrl = useDebouncedValue(url.trim(), DEBOUNCE_MS);
  const isProbeable = enabled && debouncedUrl !== "";

  const checkQuery = useQuery({
    queryKey: ["upstream-check", debouncedUrl, auth],
    queryFn: ({ signal }) =>
      postCheckUpstreamRepositories(
        // The cast is the document's fault, not this call's. UpstreamCheckInput
        // declares auth as a bare `type: object` and says "same shape as
        // RepoConfig.upstream_auth" only in prose, so the generated type is
        // Record<string, unknown> rather than UpstreamAuthConfig. Making the
        // document $ref it would remove this - a change for the openapi side,
        // not for a web refactor.
        { body: { url: debouncedUrl, auth: auth as Record<string, unknown> } },
        { signal },
      ),
    enabled: isProbeable,
    // A probe answers about a remote host that may have just come back up, so
    // it is never worth reusing beyond the current form session.
    staleTime: 0,
    gcTime: 60_000,
    retry: false,
    meta: { suppressGlobalErrorToast: true },
  });

  return {
    // "Checking" covers the debounce window as well as the request. Otherwise
    // the hint would blank out between the last keystroke and the probe, which
    // reads as "nothing is happening".
    isChecking:
      enabled && url.trim() !== "" && (debouncedUrl !== url.trim() || checkQuery.isFetching),
    // A probe that could not be made at all is reported as no result rather than
    // as unreachable: the difference is whether the URL is wrong or we are.
    health: checkQuery.isError ? null : (checkQuery.data ?? null),
    hasUrl: url.trim() !== "",
  };
}

function useDebouncedValue<T>(value: T, delayMs: number): T {
  const [debounced, setDebounced] = useState(value);

  useEffect(() => {
    const timer = setTimeout(() => setDebounced(value), delayMs);
    return () => clearTimeout(timer);
  }, [value, delayMs]);

  return debounced;
}
