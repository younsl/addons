//! Slack rich text for alarms. Every template is written once as mrkdwn
//! `text`, which Mattermost renders. Slack gets the same content as a
//! `rich_text` block, where numbered lines become a native ordered list.
//! Only the mrkdwn subset the templates use is understood: `*bold*`,
//! `_italic_`, `<url|label>` links and lines starting with `N. `.

use serde::Serialize;
use serde_json::{Value, json};

use super::Result;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Style {
    bold: bool,
    italic: bool,
}

impl Style {
    fn apply(self, mut el: Value) -> Value {
        if self.bold || self.italic {
            let mut style = serde_json::Map::new();
            if self.bold {
                style.insert("bold".to_string(), Value::Bool(true));
            }
            if self.italic {
                style.insert("italic".to_string(), Value::Bool(true));
            }
            el["style"] = Value::Object(style);
        }
        el
    }
}

/// Reports whether the marker at `i` opens or closes emphasis rather than
/// being a literal character, such as the underscore in `typing_extensions`.
fn is_marker(chars: &[char], i: usize, open: bool) -> bool {
    let prev = i.checked_sub(1).map(|p| chars[p]);
    let next = chars.get(i + 1).copied();
    if open {
        prev.is_none_or(char::is_whitespace)
            && next.is_some_and(|c| !c.is_whitespace())
            && chars[i + 1..].contains(&chars[i])
    } else {
        prev.is_some_and(|c| !c.is_whitespace())
            && next.is_none_or(|c| c.is_whitespace() || c.is_ascii_punctuation())
    }
}

/// Converts one line of mrkdwn into rich text elements.
fn inline(line: &str) -> Vec<Value> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut style = Style::default();
    let flush = |buf: &mut String, out: &mut Vec<Value>, style: Style| {
        if !buf.is_empty() {
            out.push(style.apply(json!({"type": "text", "text": std::mem::take(buf)})));
        }
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '<'
            && let Some(len) = chars[i..].iter().position(|&c| c == '>')
        {
            flush(&mut buf, &mut out, style);
            let inner: String = chars[i + 1..i + len].iter().collect();
            let (url, label) = inner.split_once('|').unwrap_or((&inner, &inner));
            out.push(style.apply(json!({"type": "link", "url": url, "text": label})));
            i += len + 1;
            continue;
        }
        let toggles = match c {
            '*' => Some(style.bold),
            '_' => Some(style.italic),
            _ => None,
        };
        if let Some(on) = toggles
            && is_marker(&chars, i, !on)
        {
            flush(&mut buf, &mut out, style);
            if c == '*' {
                style.bold = !on;
            } else {
                style.italic = !on;
            }
            i += 1;
            continue;
        }
        buf.push(c);
        i += 1;
    }
    flush(&mut buf, &mut out, style);
    out
}

/// The text of a numbered line (`12. item`), or `None` for any other line.
fn ordered_item(line: &str) -> Option<&str> {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    line[digits..].strip_prefix(". ")
}

/// Builds the Slack blocks for an mrkdwn alarm: one `rich_text` block whose
/// runs of numbered lines are ordered lists and whose other lines are text.
pub(super) fn blocks(mrkdwn: &str) -> Vec<Value> {
    let mut elements = Vec::new();
    let mut section: Vec<Value> = Vec::new();
    let mut list: Vec<Value> = Vec::new();
    let mut lines_in_section = 0;
    for line in mrkdwn.lines() {
        if let Some(item) = ordered_item(line) {
            if !section.is_empty() {
                elements.push(
                    json!({"type": "rich_text_section", "elements": std::mem::take(&mut section)}),
                );
            }
            lines_in_section = 0;
            list.push(json!({"type": "rich_text_section", "elements": inline(item)}));
            continue;
        }
        if !list.is_empty() {
            elements.push(json!({"type": "rich_text_list", "style": "ordered", "elements": std::mem::take(&mut list)}));
        }
        if lines_in_section > 0 {
            section.push(json!({"type": "text", "text": "\n"}));
        }
        lines_in_section += 1;
        section.extend(inline(line));
    }
    if !section.is_empty() {
        elements.push(json!({"type": "rich_text_section", "elements": section}));
    }
    if !list.is_empty() {
        elements.push(json!({"type": "rich_text_list", "style": "ordered", "elements": list}));
    }
    vec![json!({"type": "rich_text", "elements": elements})]
}

/// Serializes an alarm payload for a webhook, adding `blocks` built from its
/// `text`. Slack shows the blocks and keeps `text` for notifications.
/// Mattermost and other receivers ignore `blocks`.
pub(crate) fn webhook_body<T: Serialize>(payload: &T) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(payload)?;
    if let Some(text) = value.get("text").and_then(Value::as_str) {
        let built = blocks(text);
        value["blocks"] = Value::Array(built);
    }
    Ok(serde_json::to_vec(&value)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_reads_bold_italic_and_links() {
        assert_eq!(
            inline("*Score*: 57 <https://example.com/a|A> _2 more, <https://example.com|see>_"),
            vec![
                json!({"type": "text", "text": "Score", "style": {"bold": true}}),
                json!({"type": "text", "text": ": 57 "}),
                json!({"type": "link", "url": "https://example.com/a", "text": "A"}),
                json!({"type": "text", "text": " "}),
                json!({"type": "text", "text": "2 more, ", "style": {"italic": true}}),
                json!({"type": "link", "url": "https://example.com", "text": "see", "style": {"italic": true}}),
            ]
        );
    }

    #[test]
    fn inline_keeps_literal_underscores_and_stars() {
        assert_eq!(
            inline("typing_extensions@4.0 and a*b"),
            vec![json!({"type": "text", "text": "typing_extensions@4.0 and a*b"})]
        );
    }

    #[test]
    fn inline_styles_a_bold_link() {
        assert_eq!(
            inline("*<https://example.com|Forklift security insights>*"),
            vec![
                json!({"type": "link", "url": "https://example.com", "text": "Forklift security insights", "style": {"bold": true}})
            ]
        );
    }

    #[test]
    fn numbered_lines_become_an_ordered_list() {
        let built = blocks(
            "*Title*\nSummary\n\n*Findings*\n1. <https://example.com/1|One> (high)\n2. Two\n_1 more_",
        );
        assert_eq!(
            built,
            vec![json!({"type": "rich_text", "elements": [
                {"type": "rich_text_section", "elements": [
                    {"type": "text", "text": "Title", "style": {"bold": true}},
                    {"type": "text", "text": "\n"},
                    {"type": "text", "text": "Summary"},
                    {"type": "text", "text": "\n"},
                    {"type": "text", "text": "\n"},
                    {"type": "text", "text": "Findings", "style": {"bold": true}},
                ]},
                {"type": "rich_text_list", "style": "ordered", "elements": [
                    {"type": "rich_text_section", "elements": [
                        {"type": "link", "url": "https://example.com/1", "text": "One"},
                        {"type": "text", "text": " (high)"},
                    ]},
                    {"type": "rich_text_section", "elements": [{"type": "text", "text": "Two"}]},
                ]},
                {"type": "rich_text_section", "elements": [
                    {"type": "text", "text": "1 more", "style": {"italic": true}},
                ]},
            ]})]
        );
    }

    #[test]
    fn webhook_body_adds_blocks_next_to_text() {
        let body = webhook_body(&json!({"text": "*Hi*", "event": "test"})).unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["text"], "*Hi*");
        assert_eq!(v["event"], "test");
        assert_eq!(v["blocks"][0]["type"], "rich_text");
    }
}
