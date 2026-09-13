import React from "react";
import ReactDOM from "react-dom/client";
import { createRouter, RouterProvider } from "@tanstack/react-router";
import { Providers } from "@/providers/providers";
import { routeTree } from "@/generated/route-tree.gen";
import { bindUserPreferenceEffects } from "@/stores/user-preferences";
import "./styles.css";

// Applies the stored theme and language to the document, and keeps them
// applied. Deliberately before render: doing it in an effect would paint the
// default theme first and then correct it.
bindUserPreferenceEffects();

const router = createRouter({ routeTree });

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <Providers>
      <RouterProvider router={router} />
    </Providers>
  </React.StrictMode>,
);
