//! dffy's renderer protocol, version 1: a request with both sides of a
//! file in, blocks for each side out (see docs/plugins.md in the dffy
//! repository).

use serde_json::{Map, Value, json};

use crate::render::{self, Block, Span};

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
        Some(Value::String(text)) => Ok(blocks(&render::render(text, width))),
        Some(_) => Err(format!("{key} isn't text or null")),
    };
    Ok(json!({"version": VERSION, "old": side("old")?, "new": side("new")?}))
}

fn blocks(blocks: &[Block]) -> Value {
    blocks
        .iter()
        .map(|block| {
            let mut object = Map::new();
            object.insert("kind".to_owned(), json!(block.kind));
            object.insert(
                "source".to_owned(),
                json!([block.source.start, block.source.end]),
            );
            let lines: Vec<Value> = block
                .lines
                .iter()
                .map(|line| Value::Array(line.iter().map(span).collect()))
                .collect();
            object.insert("lines".to_owned(), Value::Array(lines));
            if let Some(language) = &block.language {
                object.insert("language".to_owned(), json!(language));
            }
            Value::Object(object)
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
    if let Some(url) = &span.url {
        object.insert("url".to_owned(), json!(url));
    }
    if let Some(level) = span.level {
        object.insert("level".to_owned(), json!(level));
    }
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
    fn sides_render_and_missing_ones_stay_null() {
        let (answer, ok) =
            answer(br#"{"version":1,"path":"a.md","old":null,"new":"Hi *you*\n","width":40,"background":"dark"}"#);
        assert!(ok);
        let value: Value = serde_json::from_str(&answer).expect("JSON");
        assert_eq!(value["old"], Value::Null);
        assert_eq!(
            value["new"],
            json!([
                {
                    "kind": "paragraph",
                    "source": [0, 1],
                    "lines": [["Hi ", {"text": "you", "style": ["italic"]}]]
                },
                {"kind": "gap", "source": [1, 1], "lines": [[]]}
            ])
        );
    }
}
