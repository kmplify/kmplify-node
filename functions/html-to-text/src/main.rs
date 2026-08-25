//! `html-to-text`: markup in, readable text out.
//!
//! The one thing an agent cannot do for itself cheaply. It can fetch a page —
//! it has a network and this sandbox does not — but what comes back is 900 KB
//! of navigation, scripts and styling wrapped around the four paragraphs that
//! matter, and feeding that to a model wastes context on markup.
//!
//! Deliberately not a browser: no DOM, no CSS, no JavaScript. It drops what
//! is never prose, keeps what is, and decides block boundaries from the tag
//! names — which is enough for reading and honest about being nothing more.
//!
//! stdin: HTML. stdout: text. Exit 0 always; malformed markup is normal input
//! on the web, not an error.

use std::io::{Read, Write};

/// Tags whose CONTENT is never prose. Dropped entirely, not just unwrapped.
const DROPPED: [&str; 6] = ["script", "style", "noscript", "template", "svg", "head"];

/// Tags that end a line of text. Everything else is inline: `<em>` inside a
/// sentence must not break it.
const BLOCK: [&str; 24] = [
    "p", "div", "br", "hr", "section", "article", "header", "footer", "main", "aside", "nav",
    "h1", "h2", "h3", "h4", "h5", "h6", "li", "ul", "ol", "tr", "table", "blockquote", "pre",
];

fn main() {
    let mut html = String::new();
    if std::io::stdin().read_to_string(&mut html).is_err() {
        // Not UTF-8: read it lossily rather than refusing. A page in some
        // other encoding still has readable text in it.
        html = String::from_utf8_lossy(&read_bytes()).into_owned();
    }
    let text = to_text(&html);
    let mut out = std::io::stdout();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

fn read_bytes() -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut buf);
    buf
}

pub fn to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 4);
    let bytes: Vec<char> = html.chars().collect();
    let mut i = 0;
    // Depth inside a dropped element, so nested tags inside <script> cannot
    // end the drop early.
    let mut skipping: Option<String> = None;

    while i < bytes.len() {
        if bytes[i] == '<' {
            let Some(close) = find(&bytes, i + 1, '>') else {
                // An unterminated tag at the end of a truncated page: the
                // rest is markup, not text.
                break;
            };
            let raw: String = bytes[i + 1..close].iter().collect();
            let name = tag_name(&raw);
            let closing = raw.starts_with('/');

            if let Some(open) = &skipping {
                if closing && name == *open {
                    skipping = None;
                }
            } else if !closing && DROPPED.contains(&name.as_str()) {
                // Self-closing <svg/> drops nothing; only a real element opens
                // a skip.
                if !raw.ends_with('/') {
                    skipping = Some(name);
                }
            } else if BLOCK.contains(&name.as_str()) {
                push_break(&mut out);
            } else if name == "img" && !closing {
                // Alt text is content the author wrote for exactly this case.
                if let Some(alt) = attribute(&raw, "alt") {
                    if !alt.trim().is_empty() {
                        push_word(&mut out, &format!("[{}]", alt.trim()));
                    }
                }
            }
            i = close + 1;
            continue;
        }
        if skipping.is_some() {
            i += 1;
            continue;
        }
        if bytes[i] == '&' {
            if let Some(semi) = find_within(&bytes, i + 1, ';', 12) {
                let entity: String = bytes[i + 1..semi].iter().collect();
                if let Some(c) = decode_entity(&entity) {
                    push_char(&mut out, c);
                    i = semi + 1;
                    continue;
                }
            }
        }
        push_char(&mut out, bytes[i]);
        i += 1;
    }
    // Collapse the runs of blank lines that block boundaries leave behind.
    let mut text = String::with_capacity(out.len());
    let mut blanks = 0;
    for line in out.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
            text.push('\n');
        } else {
            blanks = 0;
            text.push_str(line);
            text.push('\n');
        }
    }
    text.trim().to_string() + "\n"
}

fn push_char(out: &mut String, c: char) {
    // Every run of whitespace in the source, newlines included, is one space:
    // in HTML the line breaks are the author's formatting, not the reader's.
    // Block tags are what put a line back in.
    if c.is_whitespace() {
        if !out.is_empty() && !out.ends_with(' ') && !out.ends_with('\n') {
            out.push(' ');
        }
    } else {
        out.push(c);
    }
}

fn push_word(out: &mut String, word: &str) {
    if !out.is_empty() && !out.ends_with(' ') && !out.ends_with('\n') {
        out.push(' ');
    }
    out.push_str(word);
}

fn push_break(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn tag_name(raw: &str) -> String {
    raw.trim_start_matches('/')
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn attribute(raw: &str, name: &str) -> Option<String> {
    let lower = raw.to_ascii_lowercase();
    let at = lower.find(&format!("{name}="))? + name.len() + 1;
    let rest = &raw[at..];
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let end = rest[1..].find(quote)? + 1;
        Some(rest[1..end].to_string())
    } else {
        Some(rest.split_whitespace().next()?.to_string())
    }
}

fn find(hay: &[char], from: usize, needle: char) -> Option<usize> {
    (from..hay.len()).find(|&i| hay[i] == needle)
}

/// Bounded search: `&` is a normal character in prose, and scanning to the end
/// of a megabyte for a `;` that is not coming is how a "fast" pass becomes
/// quadratic.
fn find_within(hay: &[char], from: usize, needle: char, max: usize) -> Option<usize> {
    (from..hay.len().min(from + max)).find(|&i| hay[i] == needle)
}

fn decode_entity(entity: &str) -> Option<char> {
    if let Some(hex) = entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X")) {
        return char::from_u32(u32::from_str_radix(hex, 16).ok()?);
    }
    if let Some(dec) = entity.strip_prefix('#') {
        return char::from_u32(dec.parse().ok()?);
    }
    Some(match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "mdash" => '—',
        "ndash" => '–',
        "hellip" => '…',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "euro" => '€',
        "pound" => '£',
        "deg" => '°',
        "laquo" => '«',
        "raquo" => '»',
        "ldquo" => '“',
        "rdquo" => '”',
        "lsquo" => '‘',
        "rsquo" => '’',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::to_text;

    #[test]
    fn scripts_and_styles_are_content_free() {
        let html = "<p>Kept</p><script>var x = '<p>not this</p>';</script><style>p{}</style>";
        let out = to_text(html);
        assert!(out.contains("Kept"));
        assert!(!out.contains("not this"));
        assert!(!out.contains("var x"));
    }

    #[test]
    fn inline_tags_do_not_break_a_sentence() {
        let out = to_text("<p>one <em>two</em> three</p>");
        assert_eq!(out.trim(), "one two three");
    }

    #[test]
    fn block_tags_do() {
        let out = to_text("<h1>Title</h1><p>Body</p><ul><li>a</li><li>b</li></ul>");
        let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines, vec!["Title", "Body", "a", "b"]);
    }

    #[test]
    fn entities_come_back_as_characters() {
        let out = to_text("<p>caf&eacute; &amp; bar &#8212; &#x2019;s &nbsp;fine</p>");
        assert!(out.contains("& bar"), "{out}");
        assert!(out.contains('—'), "{out}");
        assert!(out.contains('’'), "{out}");
        // An entity we do not know stays as it was rather than vanishing.
        assert!(out.contains("&eacute;"), "{out}");
    }

    #[test]
    fn alt_text_is_content_the_author_wrote() {
        let out = to_text(r#"<p>See <img src="x.png" alt="the chart"> here</p>"#);
        assert!(out.contains("[the chart]"), "{out}");
    }

    #[test]
    fn broken_markup_is_normal_input() {
        // Truncated page, stray <, unclosed tags: web input, not an error.
        // An unclosed <script> swallows the rest rather than leaking code.
        let out = to_text("<p>prose<div>more<script>secret();");
        assert!(out.contains("prose") && out.contains("more"), "{out}");
        assert!(!out.contains("secret"), "{out}");
        assert!(to_text("a < b").contains('a'));
        assert_eq!(to_text("").trim(), "");
    }

    #[test]
    fn whitespace_collapses_the_way_a_reader_expects() {
        let out = to_text("<p>a\n\n   b\t\tc</p>\n\n\n<p>d</p>");
        assert_eq!(out.trim(), "a b c\nd");
    }
}
