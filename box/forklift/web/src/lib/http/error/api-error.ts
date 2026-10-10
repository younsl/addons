export type ApiFieldErrors = Record<string, string[]>;

type ApiErrorOptions = {
  code?: string;
  fieldErrors?: ApiFieldErrors;
  payload?: unknown;
};

export type ErrorViewModel = {
  code?: string;
  fieldErrors?: ApiFieldErrors;
  kind: "api" | "network" | "timeout" | "abort" | "unknown";
  message: string;
  retryable: boolean;
  status?: number;
};

export type ErrorMessageLabels = {
  cancelled: string;
  network: string;
  timeout: string;
  unexpected: string;
};

export class ApiError extends Error {
  readonly status: number;
  readonly code?: string;
  readonly fieldErrors?: ApiFieldErrors;
  readonly payload?: unknown;

  constructor(status: number, message: string, options: ApiErrorOptions = {}) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = options.code;
    this.fieldErrors = options.fieldErrors;
    this.payload = options.payload;
  }
}

const DEFAULT_ERROR_MESSAGE = "An unexpected error occurred.";

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function readString(record: Record<string, unknown>, key: string) {
  const value = record[key];
  return typeof value === "string" && value ? value : undefined;
}

function readFieldErrors(value: unknown): ApiFieldErrors | undefined {
  if (!isRecord(value)) return undefined;

  const entries = Object.entries(value).flatMap(([field, messages]) => {
    if (!Array.isArray(messages)) return [];
    const stringMessages = messages.filter(
      (message): message is string => typeof message === "string",
    );
    return stringMessages.length > 0 ? [[field, stringMessages] as const] : [];
  });

  return entries.length > 0 ? Object.fromEntries(entries) : undefined;
}

function readErrorDetails(payload: unknown) {
  if (!isRecord(payload)) return {};

  return {
    message:
      readString(payload, "error") ??
      readString(payload, "detail") ??
      readString(payload, "message"),
    code: readString(payload, "code"),
    fieldErrors: readFieldErrors(payload.field_errors),
  };
}

export function parseApiErrorPayload(text: string): unknown {
  if (!text) return undefined;

  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

export function createApiErrorFromResponse(
  status: number,
  statusText: string,
  text: string,
  payload = parseApiErrorPayload(text),
) {
  const details = readErrorDetails(payload);
  const message = details.message || text.trim() || statusText;

  return new ApiError(status, message, {
    code: details.code,
    fieldErrors: details.fieldErrors,
    payload,
  });
}

export function toErrorViewModel(
  error: unknown,
  fallbackMessage = DEFAULT_ERROR_MESSAGE,
): ErrorViewModel {
  if (error instanceof ApiError) {
    return {
      code: error.code,
      fieldErrors: error.fieldErrors,
      kind: error.status === 0 ? "network" : "api",
      message: error.message || fallbackMessage,
      retryable: error.status === 0 || error.status >= 500,
      status: error.status,
    };
  }

  if (error instanceof DOMException && error.name === "AbortError") {
    return {
      kind: "abort",
      message: error.message || "Request cancelled.",
      retryable: false,
    };
  }

  if (error instanceof DOMException && error.name === "TimeoutError") {
    return {
      kind: "timeout",
      message: error.message || "Request timed out.",
      retryable: true,
    };
  }

  if (error instanceof Error && error.message.trim()) {
    return {
      kind: "unknown",
      message: error.message,
      retryable: false,
    };
  }

  return {
    kind: "unknown",
    message: fallbackMessage,
    retryable: false,
  };
}

export function getErrorMessage(
  error: unknown,
  fallbackMessage = DEFAULT_ERROR_MESSAGE,
): string {
  return toErrorViewModel(error, fallbackMessage).message;
}

// getErrorMessageIfAny is getErrorMessage that stays quiet when there is no
// error. getErrorMessage always returns a sentence - it falls back to a generic
// one for a null or messageless input - so chaining sources with `||` would
// report a failure on a perfectly healthy screen. Screens that combine several
// queries into one alert slot need the empty string instead.
export function getErrorMessageIfAny(
  error: unknown,
  fallbackMessage = DEFAULT_ERROR_MESSAGE,
): string {
  return error ? getErrorMessage(error, fallbackMessage) : "";
}

export function getDisplayErrorMessage(
  error: unknown,
  labels: ErrorMessageLabels,
): string {
  const viewModel = toErrorViewModel(error, labels.unexpected);

  if (viewModel.kind === "abort") return labels.cancelled;
  if (viewModel.kind === "network") return labels.network;
  if (viewModel.kind === "timeout") return labels.timeout;

  return viewModel.message;
}
