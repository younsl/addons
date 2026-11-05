// remarkLineBreaks makes a single newline in Markdown text a line break, as in
// chat tools, so a notice reads as it was typed without two trailing spaces.
// Code spans and blocks keep their own newlines: only text nodes are split.

type MdNode = { type: string; value?: string; children?: MdNode[] };

function split(node: MdNode): void {
  if (!node.children) return;
  node.children = node.children.flatMap((child) => {
    if (child.type === "text" && child.value?.includes("\n")) {
      return child.value.split(/\r?\n/).flatMap((part, i) =>
        i === 0 ? [{ type: "text", value: part }] : [{ type: "break" }, { type: "text", value: part }],
      );
    }
    split(child);
    return [child];
  });
}

export function remarkLineBreaks() {
  return (tree: MdNode) => split(tree);
}
