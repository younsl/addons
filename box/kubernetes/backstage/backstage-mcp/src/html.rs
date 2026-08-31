//! Turns a TechDocs (MkDocs) HTML page into plain text an agent can read.
//!
//! Regex based on purpose: the input is our own MkDocs output rather than
//! arbitrary web pages, and an HTML parser would be the largest dependency in
//! the binary.

use std::sync::LazyLock;

use regex::Regex;

macro_rules! re {
    ($name:ident, $pattern:expr) => {
        static $name: LazyLock<Regex> =
            LazyLock::new(|| Regex::new($pattern).expect("static regex"));
    };
}

re!(ARTICLE, r"(?is)<article[^>]*>(.*?)</article>");
re!(MAIN, r"(?is)<main[^>]*>(.*?)</main>");
re!(BODY, r"(?is)<body[^>]*>(.*?)</body>");
re!(TITLE, r"(?is)<title[^>]*>(.*?)</title>");
re!(
    DROP,
    r"(?is)<(script|style|nav)[^>]*>.*?</(script|style|nav)>|<!--.*?-->"
);
re!(PRE, r"(?is)<pre[^>]*>(.*?)</pre>");
re!(HEADING, r"(?is)<h([1-6])[^>]*>(.*?)</h[1-6]>");
re!(LIST_ITEM, r"(?i)<li[^>]*>");
re!(
    BLOCK_END,
    r"(?i)</(p|div|tr|ul|ol|table|blockquote|section|dl|dt|dd)>"
);
re!(LINE_BREAK, r"(?i)<(br|hr)\s*/?>");
re!(CELL_END, r"(?i)</t[hd]>");
re!(ANCHOR, r#"(?is)<a[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#);
re!(CODE, r"(?is)<(code|kbd)[^>]*>(.*?)</(code|kbd)>");
re!(TAG, r"(?s)<[^>]+>");
re!(ENTITY, r"&(#x[0-9a-fA-F]+|#[0-9]+|[a-zA-Z]+);");
re!(SPACES, r"[ \t]+");
re!(BLANK_LINES, r"\n{3,}");

fn named_entity(name: &str) -> Option<&'static str> {
    Some(match name.to_ascii_lowercase().as_str() {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" | "ldquo" | "rdquo" => "\"",
        "apos" | "lsquo" | "rsquo" => "'",
        "nbsp" => " ",
        "hellip" => "...",
        "mdash" | "ndash" => "-",
        "copy" => "(c)",
        _ => return None,
    })
}

/// Replaces numeric and the common named HTML entities.
#[must_use]
pub fn decode_entities(text: &str) -> String {
    ENTITY
        .replace_all(text, |caps: &regex::Captures| {
            decode_entity(&caps[1]).unwrap_or_else(|| caps[0].to_string())
        })
        .into_owned()
}

fn decode_entity(entity: &str) -> Option<String> {
    let hex = entity
        .strip_prefix("#x")
        .or_else(|| entity.strip_prefix("#X"));
    let code = match (hex, entity.strip_prefix('#')) {
        (Some(hex), _) => u32::from_str_radix(hex, 16).ok(),
        (None, Some(dec)) => dec.parse::<u32>().ok(),
        (None, None) => return named_entity(entity).map(String::from),
    };
    code.and_then(char::from_u32).map(String::from)
}

/// The page title, when the document has one.
#[must_use]
pub fn title(html: &str) -> Option<String> {
    TITLE
        .captures(html)
        .map(|caps| decode_entities(caps[1].trim()))
}

/// Picks the MkDocs article body when present, otherwise the whole document.
#[must_use]
pub fn main_content(html: &str) -> &str {
    for pattern in [&*ARTICLE, &*MAIN, &*BODY] {
        if let Some(caps) = pattern.captures(html)
            && let Some(inner) = caps.get(1)
        {
            return inner.as_str();
        }
    }
    html
}

fn strip_tags(text: &str) -> String {
    TAG.replace_all(text, "").into_owned()
}

/// Converts the page to text with Markdown-style headings, list markers and
/// fenced code blocks.
#[must_use]
pub fn to_text(html: &str) -> String {
    let content = main_content(html);
    let text = DROP.replace_all(content, "");

    // Code blocks keep their whitespace, so fence them before the collapse.
    let text = PRE.replace_all(&text, |caps: &regex::Captures| {
        let raw = decode_entities(&strip_tags(&caps[1]));
        format!("\n```\n{}\n```\n", raw.trim_end_matches('\n'))
    });

    let text = HEADING.replace_all(&text, |caps: &regex::Captures| {
        let level = caps[1].parse::<usize>().unwrap_or(1);
        let title = strip_tags(&caps[2]);
        let title = title.trim().trim_end_matches(['¶', '#']).trim();
        format!("\n\n{} {title}\n\n", "#".repeat(level))
    });
    let text = LIST_ITEM.replace_all(&text, "\n- ");
    let text = BLOCK_END.replace_all(&text, "\n");
    let text = LINE_BREAK.replace_all(&text, "\n");
    let text = CELL_END.replace_all(&text, " | ");
    let text = ANCHOR.replace_all(&text, |caps: &regex::Captures| {
        let href = &caps[1];
        let label = strip_tags(&caps[2]);
        let label = label.trim();
        if label.is_empty() || label == href || href.starts_with('#') {
            label.to_string()
        } else {
            format!("{label} ({href})")
        }
    });
    let text = CODE.replace_all(&text, "`$2`");
    let text = strip_tags(&text);
    let text = decode_entities(&text);

    let mut in_fence = false;
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            if line.trim() == "```" {
                in_fence = !in_fence;
                return line.trim().to_string();
            }
            if in_fence {
                line.trim_end().to_string()
            } else {
                SPACES.replace_all(line, " ").trim_end().to_string()
            }
        })
        .collect();
    BLANK_LINES
        .replace_all(&lines.join("\n"), "\n\n")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r##"<html><head><title>Deploy &amp; Run</title><style>.x{}</style></head>
<body><nav><a href="/">Home</a></nav>
<article class="md-content__inner">
<h1 id="deploy">Deploy<a class="headerlink" href="#deploy">¶</a></h1>
<p>Run <code>make deploy</code> from the <a href="https://git.example.com/repo">repo</a>.</p>
<ul><li>first &lt;step&gt;</li><li>second</li></ul>
<pre><code class="language-bash">helm install app ./chart \
  --set image.tag=1.2.3
</code></pre>
<table><tr><th>Key</th><th>Value</th></tr><tr><td>a</td><td>1</td></tr></table>
<script>alert(1)</script>
</article>
<footer>ignored</footer></body></html>"##;

    #[test]
    fn converts_mkdocs_page() {
        let text = to_text(PAGE);
        assert!(text.starts_with("# Deploy"));
        assert!(text.contains("Run `make deploy` from the repo (https://git.example.com/repo)."));
        assert!(text.contains("- first <step>\n- second"));
        assert!(text.contains("```\nhelm install app ./chart \\\n  --set image.tag=1.2.3\n```"));
        assert!(text.contains("Key | Value |"));
        assert!(!text.contains("alert(1)"));
        assert!(!text.contains("ignored"));
        assert!(!text.contains("Home"));
    }

    #[test]
    fn title_and_entities() {
        assert_eq!(title(PAGE).as_deref(), Some("Deploy & Run"));
        assert_eq!(title("<p>no title</p>"), None);
        assert_eq!(
            decode_entities("a &#x41; &#66; &nbsp;b &unknown; &copy;"),
            "a A B  b &unknown; (c)"
        );
    }

    #[test]
    fn falls_back_to_main_body_and_raw() {
        assert_eq!(
            to_text("<html><body><main><p>hello</p></main></body></html>"),
            "hello"
        );
        assert_eq!(to_text("<html><body><p>plain</p></body></html>"), "plain");
        assert_eq!(to_text("<p>frag</p>"), "frag");
        assert_eq!(to_text("a\n\n\n\n\nb"), "a\n\nb");
    }

    #[test]
    fn anchor_edge_cases() {
        assert_eq!(to_text(r##"<a href="#x">same page</a>"##), "same page");
        assert_eq!(to_text(r#"<a href="https://x">https://x</a>"#), "https://x");
        assert_eq!(to_text(r#"<a href="https://x"><img src="i"></a>"#), "");
    }
}
