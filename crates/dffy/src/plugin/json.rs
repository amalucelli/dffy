//! The JSON of the renderer protocol: a reader into a small value tree and
//! a string writer. Hand-written because the protocol needs a few dozen
//! lines of it and dffy has no JSON crate.

use std::fmt::Write as _;

/// Arrays and objects nested deeper than this are refused, so a hostile
/// renderer can't exhaust the stack.
const MAX_DEPTH: usize = 64;

/// Documents with more values than this are refused: a value costs 32
/// bytes parsed, so a 64MB answer of `[0,0,…]` would take a gigabyte.
const MAX_VALUES: usize = 2_000_000;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Self>),
    /// In document order; a repeated key's first value wins on lookup.
    Object(Vec<(String, Self)>),
}

impl Value {
    /// The value of `key`, for an object that has it.
    pub(crate) fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    /// A whole number that fits in a `usize`.
    pub(crate) fn as_usize(&self) -> Option<usize> {
        match *self {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "checked: whole, non-negative and below 2^53, where f64 is exact"
            )]
            Self::Number(n) if n.fract() == 0.0 && (0.0..9e15).contains(&n) => {
                usize::try_from(n as u64).ok()
            }
            _ => None,
        }
    }
}

/// Parses one JSON document, surrounding whitespace allowed.
pub(crate) fn parse(input: &[u8]) -> Result<Value, String> {
    parse_at_most(input, MAX_VALUES)
}

fn parse_at_most(input: &[u8], values: usize) -> Result<Value, String> {
    let mut parser = Parser {
        input,
        at: 0,
        values_left: values,
    };
    let value = parser.value(0)?;
    parser.blank();
    if parser.at < input.len() {
        return Err(parser.error("text after the JSON value"));
    }
    Ok(value)
}

struct Parser<'a> {
    input: &'a [u8],
    at: usize,
    values_left: usize,
}

impl Parser<'_> {
    fn error(&self, what: &str) -> String {
        format!("{what} at byte {}", self.at)
    }

    fn blank(&mut self) {
        while self
            .input
            .get(self.at)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.at += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.blank();
        self.input.get(self.at).copied()
    }

    fn literal(&mut self, word: &str, value: Value) -> Result<Value, String> {
        if self.input[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(self.error("unexpected character"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err(self.error("nested too deeply"));
        }
        let Some(left) = self.values_left.checked_sub(1) else {
            return Err(self.error("too many values"));
        };
        self.values_left = left;
        match self.peek() {
            None => Err(self.error("unexpected end")),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'"') => self.string().map(Value::String),
            Some(b'[') => {
                self.at += 1;
                let mut items = Vec::new();
                if self.peek() == Some(b']') {
                    self.at += 1;
                    return Ok(Value::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b']') => {
                            self.at += 1;
                            return Ok(Value::Array(items));
                        }
                        _ => return Err(self.error("expected , or ]")),
                    }
                }
            }
            Some(b'{') => {
                self.at += 1;
                let mut fields = Vec::new();
                if self.peek() == Some(b'}') {
                    self.at += 1;
                    return Ok(Value::Object(fields));
                }
                loop {
                    if self.peek() != Some(b'"') {
                        return Err(self.error("expected a key"));
                    }
                    let key = self.string()?;
                    if self.peek() != Some(b':') {
                        return Err(self.error("expected :"));
                    }
                    self.at += 1;
                    fields.push((key, self.value(depth + 1)?));
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b'}') => {
                            self.at += 1;
                            return Ok(Value::Object(fields));
                        }
                        _ => return Err(self.error("expected , or }")),
                    }
                }
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("unexpected character")),
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.at;
        while self
            .input
            .get(self.at)
            .is_some_and(|b| matches!(b, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'))
        {
            self.at += 1;
        }
        std::str::from_utf8(&self.input[start..self.at])
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Value::Number)
            .ok_or_else(|| self.error("bad number"))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .input
            .get(self.at..self.at + 4)
            .and_then(|d| std::str::from_utf8(d).ok())
            .and_then(|d| u32::from_str_radix(d, 16).ok())
            .ok_or_else(|| self.error("bad \\u escape"))?;
        self.at += 4;
        Ok(digits)
    }

    /// Takes a `\u` escape of a low surrogate, the second half of a pair;
    /// anything else is left for the string to read.
    fn low_surrogate(&mut self) -> Option<u32> {
        let digits = self.input.get(self.at..self.at + 6)?.strip_prefix(b"\\u")?;
        let low = u32::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()?;
        (0xdc00..0xe000).contains(&low).then(|| {
            self.at += 6;
            low
        })
    }

    /// A string, the cursor on its opening quote.
    fn string(&mut self) -> Result<String, String> {
        self.at += 1;
        let mut out = Vec::new();
        loop {
            let Some(&b) = self.input.get(self.at) else {
                return Err(self.error("unterminated string"));
            };
            self.at += 1;
            match b {
                b'"' => break,
                b'\\' => {
                    let Some(&e) = self.input.get(self.at) else {
                        return Err(self.error("unterminated string"));
                    };
                    self.at += 1;
                    let c = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let high = self.hex4()?;
                            let low = (0xd800..0xdc00)
                                .contains(&high)
                                .then(|| self.low_surrogate())
                                .flatten();
                            let code = match low {
                                Some(low) => 0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00),
                                None => high,
                            };
                            char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER)
                        }
                        _ => return Err(self.error("bad escape")),
                    };
                    let mut buf = [0; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
                0..0x20 => return Err(self.error("control character in a string")),
                b => out.push(b),
            }
        }
        String::from_utf8(out).map_err(|_| self.error("string isn't UTF-8"))
    }
}

/// Appends `s` as a JSON string, quotes included.
pub(crate) fn string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Appends `value` as compact JSON. A repeated key is written as often as
/// it was read.
pub(crate) fn write(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            let _ = write!(out, "{n}");
        }
        Value::String(text) => string(out, text),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(out, item);
            }
            out.push(']');
        }
        Value::Object(fields) => {
            out.push('{');
            for (i, (key, item)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                string(out, key);
                out.push(':');
                write(out, item);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_round_trip_through_escapes() {
        let text = "a \"quote\", a \\ and\ttabs\nnew\u{1}lines é 🦀";
        let mut out = String::new();
        string(&mut out, text);
        assert_eq!(parse(out.as_bytes()), Ok(Value::String(text.to_owned())));
        assert_eq!(
            parse(concat!("\"", "\\", "ud83e", "\\", "udd80 ", "\\", "u00e9\"").as_bytes()),
            Ok(Value::String("🦀 é".to_owned()))
        );
    }

    /// A high surrogate without its low half becomes U+FFFD, and the
    /// escape after it keeps its own meaning.
    #[test]
    fn a_lone_high_surrogate_is_replaced() {
        assert_eq!(
            parse(concat!("\"", "\\", "ud800", "\\", "u0041\"").as_bytes()),
            Ok(Value::String("\u{fffd}A".to_owned()))
        );
        assert_eq!(
            parse(concat!("\"", "\\", "ud800", "\\", "ud83e", "\\", "udd80\"").as_bytes()),
            Ok(Value::String("\u{fffd}🦀".to_owned()))
        );
    }

    #[test]
    fn documents_parse_into_values() {
        let value = parse(br#" {"version": 1, "old": null, "new": [true, -2.5e1, {}], "x": "y"} "#)
            .expect("valid");
        assert_eq!(value.get("version").and_then(Value::as_usize), Some(1));
        assert_eq!(value.get("old"), Some(&Value::Null));
        assert_eq!(
            value.get("new"),
            Some(&Value::Array(vec![
                Value::Bool(true),
                Value::Number(-25.0),
                Value::Object(Vec::new())
            ]))
        );
    }

    #[test]
    fn malformed_documents_are_refused() {
        for bad in [
            &b""[..],
            b"{",
            b"[1,]",
            b"{\"a\" 1}",
            b"\"open",
            b"nul",
            b"1 2",
            b"\"\x01\"",
        ] {
            assert!(parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        let deep = "[".repeat(MAX_DEPTH + 2);
        assert!(parse(deep.as_bytes()).is_err());
    }

    /// Every value counts, containers and their items alike.
    #[test]
    fn documents_past_the_value_cap_are_refused() {
        assert!(parse_at_most(b"[1, {\"a\": 2}]", 4).is_ok());
        assert!(parse_at_most(b"[1, {\"a\": 2}, 3]", 4).is_err());
    }
}
