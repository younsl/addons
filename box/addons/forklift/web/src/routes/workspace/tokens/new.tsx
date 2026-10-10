import { createFileRoute } from "@tanstack/react-router";

import { TokenNewPage } from "@/routes/workspace/tokens/-components/token-new-page";

export const Route = createFileRoute("/workspace/tokens/new")({
  component: TokenNewPage,
});
