//! `csv-to-json`: a header row becomes keys, every other row becomes an object.
//!
//! Agents are handed CSV constantly — exports, reports, whatever a human had
//! open — and models read JSON far better than they read column alignment.
//! This is the boring conversion that otherwise gets done by asking a model
//! to reformat data, which is slow, costs tokens and quietly invents values.
//!
//! RFC 4180 quoting is honoured: quoted fields may contain the delimiter,
//! newlines and doubled quotes. Delimiter is comma unless `--delimiter=;` is
//! given (or `--delimiter=tab`); `--no-header` numbers the columns instead.
//!
//! stdin: CSV. stdout: a JSON array of objects. Exit 2 with a reason on
//! stderr if the input is not CSV at all.

use std::io::{Read, Write};

fn main() {
    let mut args = Args::default();
    for a in std::env::args().skip(1) {
        if let Some(d) = a.strip_prefix("--delimiter=") {
            args.delimiter = match d {
                "tab" | "\\t" => '\t',
                other => match other.chars().next() {
                    Some(c) => c,
                    None => fail("--delimiter needs a character"),
                },
            };
        } else if a == "--no-header" {
            args.header = false;
        } else {
            fail(&format!("unknown argument {a:?} (--delimiter=, --no-header)"));
        }
    }

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        fail("input is not UTF-8 text");
    }
    match to_json(&input, &args) {
        Ok(json) => {
            let mut out = std::io::stdout();
            let _ = out.write_all(json.as_bytes());
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        }
        Err(why) => fail(&why),
    }
}

fn fail(why: &str) -> ! {
    let _ = writeln!(std::io::stderr(), "{why}");
    std::process::exit(2)
}

pub struct Args {
    pub delimiter: char,
    pub header: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            delimiter: ',',
            header: true,
        }
    }
}

pub fn to_json(input: &str, args: &Args) -> Result<String, String> {
    let rows = parse(input, args.delimiter)?;
    let mut rows = rows.into_iter();
    let header: Vec<String> = match (args.header, rows.next()) {
        (_, None) => return Ok("[]".to_string()),
        (true, Some(first)) => first,
        (false, Some(first)) => {
            // Put the row back: without a header it is data like any other.
            let width = first.len();
            let names: Vec<String> = (1..=width).map(|i| format!("column_{i}")).collect();
            return Ok(render(&names, std::iter::once(first).chain(rows)));
        }
    };
    if header.iter().all(|h| h.trim().is_empty()) {
        return Err("the first row is empty, so there are no column names".into());
    }
    Ok(render(&header, rows))
}

fn render(header: &[String], rows: impl Iterator<Item = Vec<String>>) -> String {
    let mut out = String::from("[");
    let mut first = true;
    for row in rows {
        // A row of one empty field is what a trailing newline looks like.
        if row.len() == 1 && row[0].trim().is_empty() {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        out.push('{');
        for (i, name) in header.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let key = if name.trim().is_empty() {
                format!("column_{}", i + 1)
            } else {
                name.trim().to_string()
            };
            escape(&key, &mut out);
            out.push(':');
            // Ragged rows are common in exports; a missing cell is null
            // rather than a refusal, and an extra one is not silently lost —
            // it lands under its column number.
            match row.get(i) {
                Some(v) => value(v, &mut out),
                None => out.push_str("null"),
            }
        }
        for (i, extra) in row.iter().enumerate().skip(header.len()) {
            out.push(',');
            escape(&format!("column_{}", i + 1), &mut out);
            out.push(':');
            value(extra, &mut out);
        }
        out.push('}');
    }
    out.push(']');
    out
}

/// Numbers and booleans go in unquoted; everything else is a string.
///
/// Deliberately conservative: `007`, `+1 555`, `1.2.3` and dates stay
/// strings, because turning an identifier into a number loses its leading
/// zeros and that is data loss, not tidying.
fn value(raw: &str, out: &mut String) {
    let t = raw.trim();
    match t {
        "" => out.push_str("null"),
        "true" | "false" => out.push_str(t),
        _ if is_plain_number(t) => out.push_str(t),
        _ => escape(raw, out),
    }
}

fn is_plain_number(t: &str) -> bool {
    let body = t.strip_prefix('-').unwrap_or(t);
    if body.is_empty() || !body.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return false;
    }
    if body.matches('.').count() > 1 || body.starts_with('.') || body.ends_with('.') {
        return false;
    }
    // Leading zeros mean an identifier: order numbers, zip codes, IDs.
    !(body.len() > 1 && body.starts_with('0') && !body.starts_with("0."))
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

/// RFC 4180 with the tolerances real files need.
fn parse(input: &str, delimiter: char) -> Result<Vec<Vec<String>>, String> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = input.chars().peekable();
    // A byte-order mark at the start of an export is invisible to the person
    // who made it and would otherwise become part of the first column name.
    if chars.peek() == Some(&'\u{feff}') {
        chars.next();
    }
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            c if c == delimiter => row.push(std::mem::take(&mut field)),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            c => field.push(c),
        }
    }
    if quoted {
        return Err("a quoted field is never closed; this is not valid CSV".into());
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(input: &str) -> String {
        to_json(input, &Args::default()).unwrap()
    }

    #[test]
    fn a_header_row_becomes_the_keys() {
        assert_eq!(
            convert("name,age\nAda,36\nGrace,45\n"),
            r#"[{"name":"Ada","age":36},{"name":"Grace","age":45}]"#
        );
    }

    #[test]
    fn quoting_survives_commas_quotes_and_newlines() {
        let csv = "a,b\n\"x,y\",\"she said \"\"hi\"\"\"\n\"two\nlines\",z\n";
        let out = convert(csv);
        assert!(out.contains(r#""a":"x,y""#), "{out}");
        assert!(out.contains(r#"she said \"hi\""#), "{out}");
        assert!(out.contains(r#"two\nlines"#), "{out}");
    }

    #[test]
    fn identifiers_do_not_become_numbers() {
        // Losing the leading zero of an order number is data loss dressed up
        // as type inference.
        let out = convert("id,qty,ok\n007,12,true\n");
        assert!(out.contains(r#""id":"007""#), "{out}");
        assert!(out.contains(r#""qty":12"#), "{out}");
        assert!(out.contains(r#""ok":true"#), "{out}");
    }

    #[test]
    fn ragged_rows_are_reported_not_dropped() {
        let out = convert("a,b,c\n1,2\n1,2,3,4\n");
        assert!(out.contains(r#""c":null"#), "{out}");
        assert!(out.contains(r#""column_4":4"#), "{out}");
    }

    #[test]
    fn semicolon_exports_and_headerless_data_both_work() {
        let semi = to_json("a;b\n1;2\n", &Args { delimiter: ';', header: true }).unwrap();
        assert!(semi.contains(r#""a":1"#), "{semi}");
        let none = to_json("1,2\n3,4\n", &Args { delimiter: ',', header: false }).unwrap();
        assert!(none.starts_with(r#"[{"column_1":1,"column_2":2}"#), "{none}");
    }

    #[test]
    fn the_edges_are_answers_not_crashes() {
        assert_eq!(convert(""), "[]");
        assert_eq!(convert("only,a,header\n"), "[]");
        assert_eq!(convert("\u{feff}name\nAda\n"), r#"[{"name":"Ada"}]"#);
        assert!(to_json("a,b\n\"unclosed\n", &Args::default()).is_err());
    }

    #[test]
    fn empty_cells_are_null_and_crlf_is_a_line_ending() {
        let out = convert("a,b\r\n1,\r\n");
        assert_eq!(out, r#"[{"a":1,"b":null}]"#);
    }
}
