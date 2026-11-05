// The transport the generated services run on. It is the request logic that
// used to live inline in api.ts, lifted out so the generated client and the
// hand-written one share a single implementation rather than drifting apart.

import {
  ApiError,
  createApiErrorFromResponse,
  parseApiErrorPayload,
} from "@/lib/http/error/api-error";

export type HttpMethod = "GET" | "POST" | "PUT" | "PATCH" | "DELETE";

export type RequestOptions = {
  signal?: AbortSignal;
  timeoutMs?: number;
  headers?: HeadersInit;
};

export type HttpClientOptions = {
  baseUrl?: string;
  credentials?: RequestCredentials;
  headers?: HeadersInit;
};

// Default per-request deadline. The backend bounds its own handlers (e.g. the
// 8s upstream-probe timeout), so a request that outlives this never returned -
// abort it client-side so callers settle into a terminal state instead of
// spinning forever (e.g. a stuck "checking…" badge). Generous enough not to
// trip legitimately slow management calls.
export const DEFAULT_REQUEST_TIMEOUT_MS = 15_000;

function createRequestHeaders(
  body: unknown,
  defaultHeaders?: HeadersInit,
  requestHeaders?: HeadersInit,
) {
  const headers = new Headers(defaultHeaders);
  new Headers(requestHeaders).forEach((value, key) => headers.set(key, value));

  if (body !== undefined && !headers.has("Content-Type")) {
    headers.set("Content-Type", "application/json");
  }

  return headers;
}

async function request<TResponse, TBody = unknown>(
  method: HttpMethod,
  path: string,
  body?: TBody,
  options: RequestOptions = {},
  clientOptions: HttpClientOptions = {},
): Promise<TResponse> {
  const controller = new AbortController();
  const timer = setTimeout(
    () =>
      controller.abort(new DOMException("request timed out", "TimeoutError")),
    options.timeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS,
  );
  // Forward an external abort (e.g. a React effect cleanup) to our controller so
  // navigating away or a superseding request cancels the in-flight fetch.
  const external = options.signal;
  const onExternalAbort = () => controller.abort(external?.reason);
  if (external) {
    if (external.aborted) controller.abort(external.reason);
    else external.addEventListener("abort", onExternalAbort, { once: true });
  }

  // The deadline and external abort must cover the WHOLE request, including the
  // body read: fetch() resolves once response headers arrive, so a stalled body
  // stream (slow upstream, buffering proxy/LB) would otherwise hang res.text()
  // forever - leaving the caller's "checking…" badge spinning. Keep the timer
  // and listener live until the body is fully consumed by clearing them in a
  // finally that wraps fetch + text parsing.
  try {
    const res = await fetch(`${clientOptions.baseUrl ?? ""}${path}`, {
      method,
      credentials: clientOptions.credentials ?? "include",
      headers: createRequestHeaders(
        body,
        clientOptions.headers,
        options.headers,
      ),
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: controller.signal,
    });
    if (res.status === 204) return undefined as TResponse;
    const text = await res.text();
    // Error bodies are not always JSON (e.g. middleware returns plaintext
    // "forbidden" / "unauthorized"), so parse defensively and fall back to the
    // raw text instead of throwing a misleading "Unexpected token" parse error.
    const data = parseApiErrorPayload(text);
    if (!res.ok) {
      throw createApiErrorFromResponse(res.status, res.statusText, text, data);
    }
    return data as TResponse;
  } catch (error) {
    // An abort is the caller's own doing - navigating away, or a newer request
    // superseding this one - so it must arrive as the DOMException the caller
    // can recognise, not wrapped as a request failure.
    if (controller.signal.aborted) {
      throw (
        controller.signal.reason ??
        new DOMException("request aborted", "AbortError")
      );
    }
    if (error instanceof ApiError) throw error;
    // Everything left is the request never reaching a response: DNS, refused
    // connection, CORS. Status 0 marks that apart from any answer the server
    // gave, and reads as retryable.
    if (error instanceof Error) {
      throw new ApiError(0, error.message, {
        code: "NETWORK_ERROR",
        payload: error,
      });
    }
    throw new ApiError(0, "Network request failed", {
      code: "NETWORK_ERROR",
      payload: error,
    });
  } finally {
    clearTimeout(timer);
    external?.removeEventListener("abort", onExternalAbort);
  }
}

export function createHttpClient(clientOptions: HttpClientOptions = {}) {
  return {
    request: <TResponse, TBody = unknown>(
      method: HttpMethod,
      path: string,
      body?: TBody,
      options?: RequestOptions,
    ) => request<TResponse, TBody>(method, path, body, options, clientOptions),
    get: <TResponse>(path: string, options?: RequestOptions) =>
      request<TResponse>("GET", path, undefined, options, clientOptions),
    post: <TResponse, TBody = unknown>(
      path: string,
      body?: TBody,
      options?: RequestOptions,
    ) => request<TResponse, TBody>("POST", path, body, options, clientOptions),
    put: <TResponse, TBody = unknown>(
      path: string,
      body?: TBody,
      options?: RequestOptions,
    ) => request<TResponse, TBody>("PUT", path, body, options, clientOptions),
    patch: <TResponse, TBody = unknown>(
      path: string,
      body?: TBody,
      options?: RequestOptions,
    ) => request<TResponse, TBody>("PATCH", path, body, options, clientOptions),
    delete: <TResponse, TBody = unknown>(
      path: string,
      body?: TBody,
      options?: RequestOptions,
    ) =>
      request<TResponse, TBody>("DELETE", path, body, options, clientOptions),
  };
}

export type HttpClient = ReturnType<typeof createHttpClient>;

export const httpClient = createHttpClient();
