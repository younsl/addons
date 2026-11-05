import { useEffect, useState, type Dispatch, type SetStateAction } from "react";

import type { Repository } from "@/services/v1/openapi-types";

// What a panel that edits part of the draft accepts. The full setState shape,
// not (next: Repository) => void, so a caller can use the updater form.
export type RepositoryDraftSetter = Dispatch<SetStateAction<Repository>>;

// The settings and security tabs edit a repository field by field before
// saving, so they need a mutable copy. That copy used to be the page's only
// state: the shell fetched once, held the repository, and handed setRepo down
// for the tabs to edit in place.
//
// Under React Query the fetched value can arrive again at any time, so the two
// have to be separate: the query owns what the server says, this owns what the
// user has typed.
//
// The draft re-seeds when the server's copy actually changes - a save, or an
// edit made elsewhere - and not on a background refetch that returned the same
// thing. updated_at is the key for that: it moves on every write and on no
// read. Without it, a refetch landing mid-edit would silently discard the
// unsaved changes.
export function useRepositoryDraft(repository: Repository) {
  const [draft, setDraft] = useState(repository);

  useEffect(() => {
    setDraft(repository);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [repository.id, repository.updated_at]);

  return [draft, setDraft] as const;
}
