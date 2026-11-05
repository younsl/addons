import "@tanstack/react-query";

export type ReactQueryErrorHandler = (error: unknown) => void;

export interface AppQueryMeta extends Record<string, unknown> {
  errorHandler?: ReactQueryErrorHandler;
  suppressGlobalErrorToast?: boolean;
}

declare module "@tanstack/react-query" {
  interface Register {
    mutationMeta: AppQueryMeta;
    queryMeta: AppQueryMeta;
  }
}
