import { useLayoutEffect, useRef } from "react";
import { useNavigate } from "@tanstack/react-router";

import type { AnyRouter, NavigateOptions, RegisteredRouter } from "@tanstack/react-router";

// Redirect sends the user somewhere else from inside a render, and is what the
// permission gates return when they turn someone away.
//
// It exists because the router's own <Navigate> cannot be written inline. That
// component re-navigates whenever its props object is not the one it navigated
// with last, compared by identity - and JSX builds a fresh props object on
// every render. The screen being left keeps rendering while the router
// transitions away from it, so each render fired another navigation, which
// re-rendered it, which navigated again: React ended the run with "Maximum
// update depth exceeded" after ~50 rounds. The address bar still arrived at the
// right place, which is why it only ever surfaced as a console error.
//
// Keying on the destination rather than on object identity makes the redirect
// fire once, and again only if the destination itself changes.
export function Redirect<
  TRouter extends AnyRouter = RegisteredRouter,
  const TFrom extends string = string,
  const TTo extends string | undefined = undefined,
  const TMaskFrom extends string = TFrom,
  const TMaskTo extends string = "",
>(props: NavigateOptions<TRouter, TFrom, TTo, TMaskFrom, TMaskTo>) {
  const navigate = useNavigate();
  const destination = JSON.stringify(props);
  // Read through a ref so the effect depends on the destination alone, not on
  // the props object it happens to be spelled with this render.
  const latest = useRef(props);
  latest.current = props;

  useLayoutEffect(() => {
    navigate(latest.current as Parameters<typeof navigate>[0]);
  }, [destination, navigate]);

  return null;
}
