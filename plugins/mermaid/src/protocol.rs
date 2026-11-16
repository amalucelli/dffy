//! dffy's renderer protocol, version 1: a request with both sides of a
//! file in, blocks for each side out (see docs/plugins.md in the dffy
//! repository).

use dffy_render_mermaid::{Block, Span};
use serde_json::{Map, Value, json};

pub(crate) const VERSION: u64 = 1;

/// The answer to `request`, and whether it's a rendering rather than an
/// error message.
pub(crate) fn answer(request: &[u8]) -> (String, bool) {
    match respond(request) {
        Ok(answer) => (answer.to_string(), true),
        Err(message) => (
            json!({"version": VERSION, "error": message}).to_string(),
            false,
        ),
    }
}

fn respond(request: &[u8]) -> Result<Value, String> {
    let request: Value =
        serde_json::from_slice(request).map_err(|e| format!("the request isn't JSON: {e}"))?;
    match request.get("version").and_then(Value::as_u64) {
        Some(VERSION) => {}
        Some(other) => {
            return Err(format!(
                "protocol {other} isn't supported; this renderer speaks {VERSION}"
            ));
        }
        None => return Err("the request has no version".to_owned()),
    }
    let width = request
        .get("width")
        .and_then(Value::as_u64)
        .and_then(|w| usize::try_from(w).ok())
        .unwrap_or(80);
    let side = |key: &str| match request.get(key) {
        None | Some(Value::Null) => Ok(Value::Null),
        Some(Value::String(text)) => Ok(blocks(
            &dffy_render_mermaid::render(text, width).unwrap_or_else(|| source(text)),
        )),
        Some(_) => Err(format!("{key} isn't text or null")),
    };
    Ok(json!({"version": VERSION, "old": side("old")?, "new": side("new")?}))
}

/// What a diagram that can't be drawn shows: its source, a block per
/// line, so its diff reads as a line diff. A carriage return inside a
/// line shows as `␍`: dffy refuses an answer with one in a span.
fn source(text: &str) -> Vec<Block> {
    text.lines()
        .enumerate()
        .map(|(i, line)| Block {
            kind: "source",
            source: i..i + 1,
            lines: vec![vec![Span {
                text: line.replace('\r', "␍"),
                style: &[],
            }]],
        })
        .collect()
}

fn blocks(blocks: &[Block]) -> Value {
    blocks
        .iter()
        .map(|block| {
            let lines: Vec<Value> = block
                .lines
                .iter()
                .map(|line| Value::Array(line.iter().map(span).collect()))
                .collect();
            json!({
                "kind": block.kind,
                "source": [block.source.start, block.source.end],
                "lines": lines,
            })
        })
        .collect()
}

/// A span as the protocol has it: plain text as a bare string.
fn span(span: &Span) -> Value {
    if span.style.is_empty() {
        return json!(span.text);
    }
    let mut object = Map::new();
    object.insert("text".to_owned(), json!(span.text));
    object.insert("style".to_owned(), json!(span.style));
    Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_of_another_version_get_an_error() {
        let (text, ok) = answer(br#"{"version": 2, "old": null, "new": "x"}"#);
        assert!(!ok);
        let value: Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(value["version"], 1);
        assert!(
            value["error"]
                .as_str()
                .is_some_and(|e| e.contains("protocol 2"))
        );
        assert!(!answer(b"nope").1);
    }

    #[test]
    fn unsupported_diagrams_and_parse_errors_pass_their_source_through() {
        for (text, lines) in [
            (
                "pie title Pets\n  \"Dogs\" : 386\n",
                ["pie title Pets", "  \"Dogs\" : 386"],
            ),
            (
                "graph TD\r\n  A --> [bro\rken",
                ["graph TD", "  A --> [bro␍ken"],
            ),
        ] {
            let request =
                json!({"version": 1, "path": "a.mmd", "old": null, "new": text, "width": 40});
            let (answer, ok) = answer(request.to_string().as_bytes());
            assert!(ok);
            let value: Value = serde_json::from_str(&answer).expect("JSON");
            let expected: Vec<Value> = lines
                .iter()
                .enumerate()
                .map(|(i, line)| json!({"kind": "source", "source": [i, i + 1], "lines": [[line]]}))
                .collect();
            assert_eq!(value["new"], Value::Array(expected), "{text}");
        }
    }
}
