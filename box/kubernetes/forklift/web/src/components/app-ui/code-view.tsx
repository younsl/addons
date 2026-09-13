import { PrismLight } from "react-syntax-highlighter";
import json from "react-syntax-highlighter/dist/esm/languages/prism/json";
import yaml from "react-syntax-highlighter/dist/esm/languages/prism/yaml";
import { oneDark, oneLight } from "react-syntax-highlighter/dist/esm/styles/prism";

PrismLight.registerLanguage("json", json);
PrismLight.registerLanguage("yaml", yaml);

// CodeView renders a bounded, scrollable, syntax-highlighted block for the
// languages the OCI artifact page shows (manifest/config JSON, chart YAML).
// The style follows the console theme; only the two grammars actually used are
// registered so the highlighter stays out of the main bundle's hot path.
export function CodeView({ code, language }: { code: string; language: "json" | "yaml" }) {
  const dark = typeof document !== "undefined" && document.documentElement.classList.contains("dark");
  return (
    <PrismLight
      language={language}
      style={dark ? oneDark : oneLight}
      customStyle={{
        margin: 0,
        maxHeight: "28rem",
        borderRadius: "var(--radius)",
        fontSize: "12px",
        lineHeight: 1.6,
      }}
      codeTagProps={{ style: { fontFamily: "var(--font-mono, ui-monospace, monospace)" } }}
    >
      {code}
    </PrismLight>
  );
}
