import { useQuery } from "@tanstack/react-query";

import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";

// Independent of the page's auto-refresh toggle: the timeline is meant to stay
// live. The server checks once a minute, so this shows a new result within a
// few seconds of it landing.
export const HEALTH_POLL_MS = 5_000;

export function useStorageHealth() {
  const query = useQuery({
    ...openApiQueryOptions.getStorageHealth(),
    refetchInterval: HEALTH_POLL_MS,
    staleTime: 0,
    meta: { suppressGlobalErrorToast: true },
  });

  return {
    error: getErrorMessageIfAny(query.error),
    health: query.data,
    // The timeline's right edge. It advances on every successful poll, so the
    // window slides even when no new check has arrived, and freezes on the last
    // good fetch when the server stops answering.
    now: query.dataUpdatedAt || null,
  };
}
