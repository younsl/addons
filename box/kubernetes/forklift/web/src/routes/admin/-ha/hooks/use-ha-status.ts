import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { getErrorMessage, getErrorMessageIfAny } from "@/lib/http/error/api-error";
import { useTranslation } from "@/lib/i18n";
import { openApiQueryKeys, openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { postStepDownHa } from "@/services/v1/ha/api";

// How often the HA status is re-fetched. The header shows a live countdown to
// the next one, so a failover (leader change) is visibly on its way rather than
// appearing without explanation.
export const HA_REFRESH_MS = 5_000;

// Storage capacity is polled on its own, much slower cadence: a volume fills up
// over days, and for a MinIO backend each read is an Admin API round trip that
// does not belong on the 5s leader-election beat.
const STORAGE_REFRESH_MS = 30_000;

// How often the countdown label is recomputed. Four times a second reads as
// smooth without being a render loop.
const COUNTDOWN_TICK_MS = 250;

export function useHaStatus() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [notice, setNotice] = useState("");
  const [actionError, setActionError] = useState("");

  const haQuery = useQuery({
    ...openApiQueryOptions.getHa(),
    refetchInterval: HA_REFRESH_MS,
    // Live status: the client-wide staleTime would let a manual refresh return
    // a cached leader on the one page whose job is to show the current one.
    staleTime: 0,
    meta: { suppressGlobalErrorToast: true },
  });

  // A failure leaves this undefined, which simply drops the usage bar from the
  // topology: the diagram is about leadership, and a missing capacity reading
  // must not take the page down with it. Hence no error surfaced from here.
  const storageQuery = useQuery({
    ...openApiQueryOptions.getStorage(),
    refetchInterval: STORAGE_REFRESH_MS,
    staleTime: 0,
    meta: { suppressGlobalErrorToast: true },
  });

  const stepDownMutation = useMutation({
    meta: { suppressGlobalErrorToast: true },
    mutationFn: () => postStepDownHa(),
    onSuccess: async () => {
      setNotice(t("ha.stepping-down-notice"));
      // The leader is changing underneath: refetch at once rather than waiting
      // out the poll, so the roles in the table swap as soon as they can.
      await queryClient.invalidateQueries({ queryKey: openApiQueryKeys.getHa() });
    },
    onError: (caught) => setActionError(getErrorMessage(caught)),
  });

  const secondsLeft = useCountdownTo(haQuery.dataUpdatedAt + HA_REFRESH_MS);

  return {
    error: actionError || getErrorMessageIfAny(haQuery.error),
    isLoading: haQuery.isPending,
    isSteppingDown: stepDownMutation.isPending,
    notice,
    secondsLeft,
    status: haQuery.data,
    usageSource: storageQuery.data,
    refresh: () => {
      setActionError("");
      return haQuery.refetch();
    },
    stepDown: () => {
      setActionError("");
      setNotice("");
      stepDownMutation.mutate();
    },
  };
}

// The countdown is display-only: React Query owns when the next fetch happens,
// and this only says how long that is. Deriving it from dataUpdatedAt rather
// than tracking a deadline by hand means the two cannot drift apart.
function useCountdownTo(deadline: number): number {
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), COUNTDOWN_TICK_MS);
    return () => window.clearInterval(timer);
  }, []);

  return Math.max(0, Math.ceil((deadline - now) / 1000));
}
