import { createFileRoute } from "@tanstack/react-router";

import { TokensPage } from "@/routes/workspace/tokens/-components/tokens-page";

export const Route = createFileRoute("/workspace/tokens/")({
  component: TokensPage,
});
