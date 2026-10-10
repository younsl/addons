import { MutationCache, QueryCache, QueryClient } from "@tanstack/react-query";

import { ApiError, toErrorViewModel } from "@/lib/http/error/api-error";
import type {
  AppQueryMeta,
  ReactQueryErrorHandler,
} from "@/lib/react-query/query-meta";

export interface QueryClientErrorHandlers {
  onMutationError?: ReactQueryErrorHandler;
  onQueryError?: ReactQueryErrorHandler;
}

// handleReactQueryError decides who reports a failure, in this order:
//
//   1. a cancelled request is not a failure and is dropped. httpClient forwards
//      an external abort to its own controller, so navigating away or letting a
//      newer request supersede an older one raises AbortError - showing that to
//      the user would be reporting their own action back at them
//   2. a query may name its own handler through meta.errorHandler
//   3. a query may opt out entirely, when the screen renders the error itself
//   4. anything left goes to the global handler
function handleReactQueryError(
  error: unknown,
  meta: AppQueryMeta | undefined,
  globalErrorHandler: ReactQueryErrorHandler | undefined,
) {
  if (toErrorViewModel(error).kind === "abort") return;

  if (meta?.errorHandler) {
    meta.errorHandler(error);
    return;
  }

  if (meta?.suppressGlobalErrorToast) return;

  globalErrorHandler?.(error);
}

// A 4xx is an answer, not a hiccup: not signed in, not allowed, not there.
// Asking again gets the same one, so the only effect of a retry is to double
// the requests and delay the message the user is waiting for. A 5xx or a
// transport failure (ApiError carries status 0 for those) is worth one more
// attempt.
//
// This replaces a flat `retry: 1`. That was harmless while two queries used
// this client; it now covers 39, several of which 403 by design - the user
// list is admin-only but the approval queue links to it for anyone who can
// approve.
function retryOnlyWhatCouldSucceed(failureCount: number, error: unknown): boolean {
  if (error instanceof ApiError && error.status >= 400 && error.status < 500) return false;

  return failureCount < 1;
}

export function createQueryClient(
  errorHandlers: QueryClientErrorHandlers = {},
) {
  return new QueryClient({
    mutationCache: new MutationCache({
      onError: (error, _variables, _context, mutation) => {
        handleReactQueryError(
          error,
          mutation.meta,
          errorHandlers.onMutationError,
        );
      },
    }),
    queryCache: new QueryCache({
      onError: (error, query) => {
        handleReactQueryError(error, query.meta, errorHandlers.onQueryError);
      },
    }),
    defaultOptions: {
      queries: {
        refetchOnWindowFocus: false,
        retry: retryOnlyWhatCouldSucceed,
        staleTime: 15_000,
      },
    },
  });
}

// No global handler is connected yet. The toast component is in the tree
// (components/ui/toast) and this is the one line that would use it, but almost
// nothing reaches it today: only app-shell and login fetch through React Query,
// and the other 23 screens still render their own errors from useEffect. Wiring
// it now would report a failure twice on those screens and not at all elsewhere.
//
// Connect it once the screens move to React Query:
//
//   import { toast } from "@/components/ui/toast";
//   import { getErrorMessage } from "@/lib/http/error/api-error";
//
//   const reportToToast = (error: unknown) =>
//     toast.add({ title: getErrorMessage(error), type: "error" });
//
//   export const queryClient = createQueryClient({
//     onMutationError: reportToToast,
//     onQueryError: reportToToast,
//   });
//
// The routing below is live regardless: aborts are dropped, and a query can
// claim its error through meta.errorHandler or opt out with
// meta.suppressGlobalErrorToast.
export const queryClient = createQueryClient();
