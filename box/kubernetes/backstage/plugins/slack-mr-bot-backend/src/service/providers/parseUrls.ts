/** Upper bound on merge requests in one message, to keep the modal readable. */
export const MAX_REQUESTS = 5;

/**
 * Splits slash command text or a pasted list into URLs.
 *
 * Slack wraps links in angle brackets and may append a `|label`, and URLs
 * pasted without a separator arrive as one token, so each whitespace-separated
 * token is split again at every embedded `http(s)://`.
 */
export function parseUrls(text: string): string[] {
  const urls: string[] = [];
  for (const token of text.split(/\s+/)) {
    const cleaned = token.replace(/^<|>$/g, '').split('|')[0];
    if (!cleaned) continue;
    for (const part of cleaned.split(/(?=https?:\/\/)/)) {
      if (part) urls.push(part);
    }
  }
  // The same merge request pasted twice would post two identical lines.
  return [...new Set(urls)];
}
