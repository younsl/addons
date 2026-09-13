import { useState } from "react";
import { useNavigate } from "@tanstack/react-router";

import { getErrorMessage } from "@/lib/http/error/api-error";
import { useRepositoriesList } from "@/routes/workspace/repositories/-hooks/use-repositories-list";
import { useCreateRepositoryMutation } from "@/routes/workspace/repositories/-hooks/use-repository-mutations";
import { useUpstreamCheck } from "@/routes/workspace/repositories/-hooks/use-upstream-check";

import type { RepositoryCreate, UpstreamAuthConfig } from "@/services/v1/openapi-types";

export type RepositoryType = "hosted" | "proxy" | "group";
export type RepositoryFormat = RepositoryCreate["format"];

export function useRepositoryCreateForm() {
  const navigate = useNavigate();
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [isPublic, setIsPublic] = useState(false);
  const [format, setFormat] = useState<RepositoryFormat>("maven");
  const [type, setType] = useState<RepositoryType>("proxy");
  const [upstreamUrl, setUpstreamUrl] = useState("");
  // Optional upstream credentials for private registries; {} means anonymous.
  const [upstreamAuth, setUpstreamAuth] = useState<UpstreamAuthConfig>({});
  const [isAgeEnabled, setIsAgeEnabled] = useState(false);
  const [minAge, setMinAge] = useState("3d");
  const [members, setMembers] = useState<string[]>([]);
  const [error, setError] = useState("");

  const { repositories } = useRepositoriesList();
  const createMutation = useCreateRepositoryMutation();
  const upstreamCheck = useUpstreamCheck({
    url: upstreamUrl,
    auth: upstreamAuth,
    enabled: type === "proxy",
  });

  // Candidate members: same format, not a group itself, not already selected.
  // A group of groups is not something the resolver supports.
  const candidates = repositories.filter(
    (repository) =>
      repository.format === format &&
      repository.type !== "group" &&
      !members.includes(repository.name),
  );

  // Mirrors the form's required fields so Create stays disabled until complete.
  // Reachability is deliberately not part of this: an upstream that does not
  // answer yet is still a valid address to configure.
  const isComplete =
    name.trim() !== "" &&
    (type !== "proxy" || upstreamUrl.trim() !== "") &&
    (type !== "proxy" || !isAgeEnabled || minAge.trim() !== "") &&
    (type !== "group" || members.length > 0);

  return {
    candidates,
    description,
    error,
    format,
    isAgeEnabled,
    isComplete,
    isCreating: createMutation.isPending,
    isPublic,
    members,
    minAge,
    name,
    repositories,
    type,
    upstreamAuth,
    upstreamCheck,
    upstreamUrl,
    setDescription,
    setIsAgeEnabled,
    setIsPublic,
    setMembers,
    setMinAge,
    setName,
    setUpstreamAuth,
    setUpstreamUrl,
    // Changing the format invalidates the chosen members: a group may only hold
    // repositories of its own format.
    setFormat: (next: RepositoryFormat) => { setFormat(next); setMembers([]); },
    setType,
    cancel: () => navigate({ to: "/workspace/repositories" }),
    submit: () => {
      setError("");
      createMutation.mutate(
        {
          name,
          format,
          type,
          upstream_url: type === "proxy" ? upstreamUrl : "",
          // Omitted rather than sent empty: the API distinguishes an absent
          // description from a blank one.
          ...(description.trim() ? { description: description.trim() } : {}),
          config: {
            cache: { enabled: true, metadata_ttl: "15m", negative_ttl: "5m", eviction: "lru" },
            // Private is the default, so only the public choice is sent.
            ...(isPublic ? { public: true } : {}),
            age_policy: isAgeEnabled
              ? { enabled: true, min_age: minAge, action: "block" }
              : { enabled: false },
            policy_pipeline: {
              schema_version: 2,
              order: ["vulnerability", "license", "age"],
            },
            // Each is omitted rather than sent empty: a hosted repository has
            // no members, and anonymous upstream access has no auth block.
            ...(type === "group" ? { group: { members } } : {}),
            ...(type === "proxy" && upstreamAuth.type ? { upstream_auth: upstreamAuth } : {}),
          },
        } as RepositoryCreate,
        {
          onSuccess: () => navigate({ to: "/workspace/repositories" }),
          onError: (caught) => setError(getErrorMessage(caught)),
        },
      );
    },
  };
}
