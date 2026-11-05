// The client-side constraint on a name: letters, digits, underscore and hyphen,
// up to 64 characters. Mirrors what the server accepts.
//
// The hyphen is escaped, which matters more than it looks. Browsers compile a
// `pattern` attribute with the RegExp `v` flag, and there a trailing "-" in a
// character class is a syntax error rather than a literal. An invalid pattern
// is dropped silently - Chrome logs it to the console and nothing enforces the
// constraint - so `[A-Za-z0-9_-]{1,64}` had been validating nothing at all on
// four forms. A browser test caught the console error.
export const NAME_PATTERN = "[A-Za-z0-9_\\-]{1,64}";
