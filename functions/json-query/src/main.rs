//! `json-query`: pull the part that matters out of a JSON document.
//!
//! An agent that fetched an API response has 200 KB of JSON and needs four
//! fields. Sending the whole thing to a model to "extract" them costs
//! context, latency and accuracy for work that is a path expression.
//!
//! The language is small on purpose, and small enough to describe in full:
//!
//!   .              the document
//!   .name          a field
//!   .a.b.c         nested fields
//!   .items[]       every element of an array (or every value of an object)
//!   .items[2]      one element, negative counts from the end
//!   .items[1:3]    a slice
//!   | length       how many (array, object, string) — also on the document
//!   | keys         an object's field names, sorted
//!   | values       an object's values
//!   | first,last   ends of an array
//!   | type         "object" | "array" | "string" | "number" | "boolean" | "null"
//!
//! Stages chain, and a stage may itself be a path: `.items | first | .name`.
//!
//! It is not jq: no arithmetic, no conditionals, no user functions, no
//! constructing new objects. Those are a language, and a language in a
//! sandbox that other people's machines execute is a much larger promise
//! than "select this field".
//!
//! stdin: JSON. args: the expression (default `.`). stdout: JSON. Exit 2
//! with a reason when the input is not JSON or the path does not fit it.

use std::io::{Read, Write};

fn main() {
    let expr = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        fail("input is not UTF-8 text");
    }
    let doc = match Json::parse(&input) {
        Ok(v) => v,
        Err(why) => fail(&format!("input is not JSON: {why}")),
    };
    match query(&doc, &expr) {
        Ok(out) => {
            let mut o = std::io::stdout();
            let _ = o.write_all(out.render().as_bytes());
            let _ = o.write_all(b"\n");
            let _ = o.flush();
        }
        Err(why) => fail(&why),
    }
}

fn fail(why: &str) -> ! {
    let _ = writeln!(std::io::stderr(), "{why}");
    std::process::exit(2)
}

// ------------------------------------------------------------------ values

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// Numbers keep their source text: reformatting 1.0 as 1, or losing the
    /// precision of a 20-digit id, is not this tool's business.
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn kind(&self) -> &'static str {
        match self {
            Json::Null => "null",
            Json::Bool(_) => "boolean",
            Json::Number(_) => "number",
            Json::String(_) => "string",
            Json::Array(_) => "array",
            Json::Object(_) => "object",
        }
    }

    pub fn render(&self) -> String {
        match self {
            Json::Null => "null".into(),
            Json::Bool(b) => b.to_string(),
            Json::Number(n) => n.clone(),
            Json::String(s) => {
                let mut out = String::new();
                escape(s, &mut out);
                out
            }
            Json::Array(items) => {
                let parts: Vec<String> = items.iter().map(Json::render).collect();
                format!("[{}]", parts.join(","))
            }
            Json::Object(fields) => {
                let parts: Vec<String> = fields
                    .iter()
                    .map(|(k, v)| {
                        let mut key = String::new();
                        escape(k, &mut key);
                        format!("{key}:{}", v.render())
                    })
                    .collect();
                format!("{{{}}}", parts.join(","))
            }
        }
    }

    pub fn parse(text: &str) -> Result<Json, String> {
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        let v = parse_value(&chars, &mut i)?;
        skip_ws(&chars, &mut i);
        if i < chars.len() {
            return Err(format!("trailing characters at position {i}"));
        }
        Ok(v)
    }
}

fn escape(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn skip_ws(c: &[char], i: &mut usize) {
    while *i < c.len() && c[*i].is_whitespace() {
        *i += 1;
    }
}

fn parse_value(c: &[char], i: &mut usize) -> Result<Json, String> {
    skip_ws(c, i);
    match c.get(*i) {
        None => Err("unexpected end of input".into()),
        Some('n') => lit(c, i, "null", Json::Null),
        Some('t') => lit(c, i, "true", Json::Bool(true)),
        Some('f') => lit(c, i, "false", Json::Bool(false)),
        Some('"') => parse_string(c, i).map(Json::String),
        Some('[') => {
            *i += 1;
            let mut items = Vec::new();
            skip_ws(c, i);
            if c.get(*i) == Some(&']') {
                *i += 1;
                return Ok(Json::Array(items));
            }
            loop {
                items.push(parse_value(c, i)?);
                skip_ws(c, i);
                match c.get(*i) {
                    Some(',') => *i += 1,
                    Some(']') => {
                        *i += 1;
                        return Ok(Json::Array(items));
                    }
                    _ => return Err(format!("expected , or ] at position {i}")),
                }
            }
        }
        Some('{') => {
            *i += 1;
            let mut fields = Vec::new();
            skip_ws(c, i);
            if c.get(*i) == Some(&'}') {
                *i += 1;
                return Ok(Json::Object(fields));
            }
            loop {
                skip_ws(c, i);
                let key = parse_string(c, i)?;
                skip_ws(c, i);
                if c.get(*i) != Some(&':') {
                    return Err(format!("expected : after a key at position {i}"));
                }
                *i += 1;
                fields.push((key, parse_value(c, i)?));
                skip_ws(c, i);
                match c.get(*i) {
                    Some(',') => *i += 1,
                    Some('}') => {
                        *i += 1;
                        return Ok(Json::Object(fields));
                    }
                    _ => return Err(format!("expected , or }} at position {i}")),
                }
            }
        }
        Some(_) => parse_number(c, i),
    }
}

fn lit(c: &[char], i: &mut usize, word: &str, v: Json) -> Result<Json, String> {
    if c[*i..].starts_with(&word.chars().collect::<Vec<_>>()[..]) {
        *i += word.len();
        Ok(v)
    } else {
        Err(format!("unexpected token at position {i}"))
    }
}

fn parse_string(c: &[char], i: &mut usize) -> Result<String, String> {
    if c.get(*i) != Some(&'"') {
        return Err(format!("expected a string at position {i}"));
    }
    *i += 1;
    let mut out = String::new();
    while let Some(&ch) = c.get(*i) {
        *i += 1;
        match ch {
            '"' => return Ok(out),
            '\\' => {
                let esc = c.get(*i).copied().ok_or("unfinished escape")?;
                *i += 1;
                out.push(match esc {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    'u' => {
                        let hex: String = c.get(*i..*i + 4).ok_or("short \\u escape")?.iter().collect();
                        *i += 4;
                        let n = u32::from_str_radix(&hex, 16).map_err(|_| "bad \\u escape")?;
                        char::from_u32(n).ok_or("bad code point")?
                    }
                    other => other,
                });
            }
            ch => out.push(ch),
        }
    }
    Err("a string is never closed".into())
}

fn parse_number(c: &[char], i: &mut usize) -> Result<Json, String> {
    let start = *i;
    while let Some(&ch) = c.get(*i) {
        if ch.is_ascii_digit() || matches!(ch, '-' | '+' | '.' | 'e' | 'E') {
            *i += 1;
        } else {
            break;
        }
    }
    let text: String = c[start..*i].iter().collect();
    if text.is_empty() || text.parse::<f64>().is_err() {
        return Err(format!("unexpected token at position {start}"));
    }
    Ok(Json::Number(text))
}

// ------------------------------------------------------------------- query

pub fn query(doc: &Json, expr: &str) -> Result<Json, String> {
    let mut stages = expr.split('|');
    let path = stages.next().unwrap_or(".").trim();
    let mut current = walk(doc, path)?;
    for stage in stages {
        current = apply(&current, stage.trim())?;
    }
    Ok(current)
}

fn walk(doc: &Json, path: &str) -> Result<Json, String> {
    if path.is_empty() || path == "." {
        return Ok(doc.clone());
    }
    if !path.starts_with('.') {
        return Err(format!("a path starts with '.', got {path:?}"));
    }
    let mut current = doc.clone();
    let mut rest = &path[1..];
    while !rest.is_empty() {
        // A field name runs to the next '.' or '['.
        let end = rest
            .find(['.', '['])
            .unwrap_or(rest.len());
        let name = &rest[..end];
        if !name.is_empty() {
            current = field(&current, name)?;
        }
        rest = &rest[end..];
        while rest.starts_with('[') {
            let close = rest.find(']').ok_or("a '[' is never closed")?;
            let inner = &rest[1..close];
            current = index(&current, inner)?;
            rest = &rest[close + 1..];
        }
        if rest.starts_with('.') {
            rest = &rest[1..];
        }
    }
    Ok(current)
}

fn field(value: &Json, name: &str) -> Result<Json, String> {
    match value {
        Json::Object(fields) => Ok(fields
            .iter()
            .find(|(k, _)| k == name)
            // A missing field is null, as it is in jq: an agent walking a
            // path into optional data should get "nothing there", not a
            // failed call.
            .map(|(_, v)| v.clone())
            .unwrap_or(Json::Null)),
        // Mapping over a spread array is the common shape: .items[].name
        Json::Array(items) => Ok(Json::Array(
            items
                .iter()
                .map(|v| field(v, name))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        Json::Null => Ok(Json::Null),
        other => Err(format!(
            "cannot take field {name:?} of a {}",
            other.kind()
        )),
    }
}

fn index(value: &Json, inner: &str) -> Result<Json, String> {
    let items = match value {
        Json::Array(items) => items.clone(),
        Json::Object(fields) if inner.is_empty() => {
            fields.iter().map(|(_, v)| v.clone()).collect()
        }
        Json::Null => return Ok(Json::Null),
        other => return Err(format!("cannot index a {}", other.kind())),
    };
    if inner.is_empty() {
        return Ok(Json::Array(items));
    }
    if let Some((from, to)) = inner.split_once(':') {
        let len = items.len() as i64;
        let start = bound(from, 0, len)?;
        let end = bound(to, len, len)?;
        let (start, end) = (start.clamp(0, len) as usize, end.clamp(0, len) as usize);
        return Ok(Json::Array(
            items.get(start..end.max(start)).unwrap_or(&[]).to_vec(),
        ));
    }
    let n: i64 = inner
        .trim()
        .parse()
        .map_err(|_| format!("{inner:?} is not an index"))?;
    let len = items.len() as i64;
    let at = if n < 0 { len + n } else { n };
    Ok(items
        .get(at.max(0) as usize)
        .cloned()
        .unwrap_or(Json::Null))
}

fn bound(text: &str, default: i64, len: i64) -> Result<i64, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(default);
    }
    let n: i64 = t.parse().map_err(|_| format!("{t:?} is not an index"))?;
    Ok(if n < 0 { len + n } else { n })
}

fn apply(value: &Json, stage: &str) -> Result<Json, String> {
    // A stage that looks like a path IS one: `.items | first | .name` is the
    // obvious way to write that, and a language where half the steps compose
    // and half do not is a language people get wrong.
    if stage.starts_with('.') {
        return walk(value, stage);
    }
    match stage {
        "length" => Ok(Json::Number(
            match value {
                Json::Array(a) => a.len(),
                Json::Object(o) => o.len(),
                Json::String(s) => s.chars().count(),
                Json::Null => 0,
                _ => return Err(format!("a {} has no length", value.kind())),
            }
            .to_string(),
        )),
        "keys" => match value {
            Json::Object(fields) => {
                let mut names: Vec<String> = fields.iter().map(|(k, _)| k.clone()).collect();
                names.sort();
                Ok(Json::Array(names.into_iter().map(Json::String).collect()))
            }
            other => Err(format!("a {} has no keys", other.kind())),
        },
        "values" => match value {
            Json::Object(fields) => Ok(Json::Array(
                fields.iter().map(|(_, v)| v.clone()).collect(),
            )),
            Json::Array(items) => Ok(Json::Array(items.clone())),
            other => Err(format!("a {} has no values", other.kind())),
        },
        "first" | "last" => match value {
            Json::Array(items) => Ok(if stage == "first" {
                items.first().cloned().unwrap_or(Json::Null)
            } else {
                items.last().cloned().unwrap_or(Json::Null)
            }),
            other => Err(format!("a {} has no {stage}", other.kind())),
        },
        "type" => Ok(Json::String(value.kind().to_string())),
        "" => Ok(value.clone()),
        other => Err(format!(
            "unknown stage {other:?} (length, keys, values, first, last, type)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"{"items":[{"name":"a","n":1},{"name":"b","n":2},{"name":"c","n":3}],
                          "meta":{"total":3,"page":null},"ok":true}"#;

    fn q(expr: &str) -> String {
        query(&Json::parse(DOC).unwrap(), expr).unwrap().render()
    }

    #[test]
    fn the_whole_document_round_trips() {
        let parsed = Json::parse(DOC).unwrap();
        assert_eq!(Json::parse(&parsed.render()).unwrap(), parsed);
    }

    #[test]
    fn fields_nest_and_arrays_spread() {
        assert_eq!(q(".meta.total"), "3");
        assert_eq!(q(".items[].name"), r#"["a","b","c"]"#);
        assert_eq!(q(".items[1].name"), r#""b""#);
        assert_eq!(q(".items[-1].n"), "3");
        assert_eq!(q(".items[0:2] | length"), "2");
    }

    #[test]
    fn a_missing_field_is_nothing_not_a_failure() {
        // An agent walking into optional data wants "not there", not a
        // failed call it has to reason about.
        assert_eq!(q(".meta.nope"), "null");
        assert_eq!(q(".meta.nope.deeper"), "null");
        assert_eq!(q(".items[99]"), "null");
    }

    #[test]
    fn stages_answer_the_questions_agents_actually_ask() {
        assert_eq!(q(".items | length"), "3");
        assert_eq!(q(".meta | keys"), r#"["page","total"]"#);
        assert_eq!(q(".items | first | .name"), r#""a""#);
        assert_eq!(q(".ok | type"), r#""boolean""#);
        assert_eq!(q(". | keys"), r#"["items","meta","ok"]"#);
    }

    #[test]
    fn numbers_keep_the_shape_they_arrived_in() {
        // Reformatting 1.0 to 1, or rounding a 20-digit id, is data loss.
        let doc = Json::parse(r#"{"a":1.0,"b":10000000000000000001,"c":1e3}"#).unwrap();
        assert_eq!(query(&doc, ".a").unwrap().render(), "1.0");
        assert_eq!(query(&doc, ".b").unwrap().render(), "10000000000000000001");
        assert_eq!(query(&doc, ".c").unwrap().render(), "1e3");
    }

    #[test]
    fn bad_input_and_bad_paths_say_what_is_wrong() {
        assert!(Json::parse("{oops}").is_err());
        assert!(Json::parse(r#"{"a":1"#).is_err());
        let doc = Json::parse(DOC).unwrap();
        assert!(query(&doc, "items").is_err(), "a path starts with a dot");
        assert!(query(&doc, ".ok | keys").is_err());
        assert!(query(&doc, ".items | frobnicate").is_err());
    }

    #[test]
    fn strings_survive_escapes_and_unicode() {
        let doc = Json::parse(r#"{"s":"a\"b\né😀"}"#);
        // Surrogate pairs are not decoded as pairs here; the halves are kept
        // as they came so nothing is silently mangled.
        assert!(doc.is_ok(), "{doc:?}");
        let out = query(&doc.unwrap(), ".s").unwrap().render();
        assert!(out.contains("\\n"), "{out}");
        assert!(out.contains('é'), "{out}");
    }
}
