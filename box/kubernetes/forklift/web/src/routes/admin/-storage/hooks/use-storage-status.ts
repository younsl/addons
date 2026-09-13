import { useState } from "react";
import { useQuery } from "@tanstack/react-query";

import { getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";

// Auto-refresh cadence when the toggle is on, keeping drive health and usage
// current without hammering the MinIO admin API behind it.
const REFRESH_MS = 10_000;

// useStorageStatus is the one query the storage overview needs, plus the
// auto-refresh toggle over it. refetchInterval replaces the setInterval the
// screen used to run: React Query stops it while the query is unmounted, which
// the interval did not.
export function useStorageStatus() {
  const [isAutoRefreshing, setIsAutoRefreshing] = useState(true);

  const storageQuery = useQuery({
    ...openApiQueryOptions.getStorage(),
    refetchInterval: isAutoRefreshing ? REFRESH_MS : false,
    // Live status: a value from fifteen seconds ago is not worth reusing, which
    // is what the client-wide staleTime would otherwise do.
    staleTime: 0,
    meta: { suppressGlobalErrorToast: true },
  });

  return {
    error: getErrorMessageIfAny(storageQuery.error),
    isAutoRefreshing,
    isLoading: storageQuery.isPending,
    refresh: () => storageQuery.refetch(),
    setAutoRefreshing: setIsAutoRefreshing,
    // When the data last arrived, taken from the cache rather than stamped by
    // hand on each response. A failed refresh leaves this at the last good
    // fetch, which is what "last updated" should mean.
    updatedAt: storageQuery.dataUpdatedAt ? new Date(storageQuery.dataUpdatedAt) : null,
    storage: storageQuery.data,
  };
}
