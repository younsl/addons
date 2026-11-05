import { Link } from "@tanstack/react-router";

import type { ReactNode } from "react";

// The queue names repositories and users as strings, but the pages they lead to
// are keyed by id. Whether an id is known depends on who is looking: listing
// repositories and users is admin-only, and both detail pages are admin-only
// anyway, so a non-admin approver correctly gets plain text rather than a link
// into a 403.
//
// label overrides the rendered text, so search-match highlighting survives.

export function repoLink(
  name: string,
  idsByName: Record<string, number>,
  label?: ReactNode,
): ReactNode {
  const id = idsByName[name];

  return id ? (
    <Link to="/workspace/repositories/$id/$tab" params={{ id: String(id), tab: "approvals" }}>
      {label ?? name}
    </Link>
  ) : (
    label ?? name
  );
}

export function userLink(
  username: string,
  idsByUsername: Record<string, number>,
  label?: ReactNode,
): ReactNode {
  const id = idsByUsername[username];

  return id ? (
    <Link to="/access/users/$id" params={{ id: String(id) }}>
      {label ?? username}
    </Link>
  ) : (
    label ?? username
  );
}
