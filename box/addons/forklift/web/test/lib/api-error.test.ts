import { describe, expect, it } from "vitest";

import {
  ApiError,
  getErrorMessage,
  getErrorMessageIfAny,
  toErrorViewModel,
} from "@/lib/http/error/api-error";

// These pin the classifier against what httpClient throws today, before it is
// changed to raise ApiError. A cancelled request must not read as a failure:
// that is the distinction the screens currently make by hand, inconsistently.
describe("toErrorViewModel", () => {
  it("reads the timeout DOMException httpClient raises as a timeout", () => {
    const viewModel = toErrorViewModel(
      new DOMException("request timed out", "TimeoutError"),
    );

    expect(viewModel.kind).toBe("timeout");
    expect(viewModel.retryable).toBe(true);
  });

  it("reads an abort as a cancellation rather than a failure", () => {
    const viewModel = toErrorViewModel(new DOMException("aborted", "AbortError"));

    expect(viewModel.kind).toBe("abort");
    expect(viewModel.retryable).toBe(false);
  });

  it("keeps the message of the plain Error httpClient throws for a 4xx", () => {
    const error = new Error("repository name already exists");

    expect(toErrorViewModel(error).kind).toBe("unknown");
    expect(getErrorMessage(error)).toBe("repository name already exists");
  });

  it("exposes status and field errors once ApiError is thrown", () => {
    const viewModel = toErrorViewModel(
      new ApiError(400, "invalid", { fieldErrors: { name: ["required"] } }),
    );

    expect(viewModel.kind).toBe("api");
    expect(viewModel.status).toBe(400);
    expect(viewModel.fieldErrors).toEqual({ name: ["required"] });
  });

  it("treats a transport failure (status 0) as retryable network trouble", () => {
    const viewModel = toErrorViewModel(new ApiError(0, "Network request failed"));

    expect(viewModel.kind).toBe("network");
    expect(viewModel.retryable).toBe(true);
  });
});

// Screens that fold several queries into one alert slot chain the sources with
// `||`. getErrorMessage cannot be used there: it always returns a sentence, so
// the chain would report a failure on a screen that loaded perfectly.
describe("getErrorMessageIfAny", () => {
  it("says nothing when there is no error", () => {
    expect(getErrorMessage(null)).not.toBe("");
    expect(getErrorMessageIfAny(null)).toBe("");
    expect(getErrorMessageIfAny(undefined)).toBe("");
  });

  it("reports the message when there is one", () => {
    expect(getErrorMessageIfAny(new ApiError(500, "roles unavailable")))
      .toBe("roles unavailable");
  });

  it("still falls back for an error carrying no message", () => {
    expect(getErrorMessageIfAny(new Error(""))).not.toBe("");
  });
});
