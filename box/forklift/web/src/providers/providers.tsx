import type { ReactNode } from "react";
import { QueryClientProvider } from "@tanstack/react-query";

import { queryClient } from "@/lib/react-query/query-client";

// Providers holds what wraps the whole app, so main.tsx stays a mount point and
// anything added later lands in one place rather than nesting further there.
//
// AuthProvider is deliberately not here: it carries the current principal,
// which is only known after /me resolves, so it belongs inside the shell
// rather than above the router.
//
// The toast surface goes here when the global error handler is connected, and
// it has to wrap the query client rather than the reverse: the handler pushes
// into the toast manager from outside React, so the surface must outlive any
// single screen.
//
//   <Toaster>
//     <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
//   </Toaster>
export function Providers({ children }: Readonly<{ children: ReactNode }>) {
  return <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>;
}
