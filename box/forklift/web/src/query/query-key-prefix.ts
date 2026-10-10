// The generated query keys are ["openapi", method, routePath, params]. Dropping
// the params leaves a prefix that matches every variant of one operation, which
// is what a write usually needs to invalidate: an approval decision changes the
// row for every filter combination the queue might be showing it under, not
// just the one the reviewer happens to be looking at.
//
// This lives here rather than being inlined as `.slice(0, 3)` at each call site
// so the dependency on the generated key shape is stated once. If the generator
// changes that shape, this is the single place to follow it.
export function operationKeyPrefix(key: readonly unknown[]): readonly unknown[] {
  return key.slice(0, 3);
}
