//! Non-executing JSON extraction for logs, Markdown and tool output.

use std::{ops::Range, str::CharIndices};

use serde_json::Value;

pub(super) struct Document {
    pub range: Range<usize>,
    pub value: Value,
    pub opaque: bool,
}

pub(super) fn documents(text: &str, start: usize) -> Documents<'_> {
    Documents {
        text,
        characters: text[start..].char_indices(),
        base: start,
        finished: false,
    }
}

pub(super) struct Documents<'a> {
    text: &'a str,
    characters: CharIndices<'a>,
    base: usize,
    finished: bool,
}

impl Documents<'_> {
    fn document(&mut self, range: Range<usize>) -> Option<Document> {
        let raw = &self.text[range.clone()];
        match serde_json::from_str::<Value>(raw) {
            Ok(value) => Some(Document {
                range,
                value,
                opaque: false,
            }),
            Err(_) if sensitive_escaped_key(raw) => Some(Document {
                range,
                value: Value::String(raw.to_owned()),
                opaque: true,
            }),
            Err(_) => None,
        }
    }
}

impl Iterator for Documents<'_> {
    type Item = Document;

    fn next(&mut self) -> Option<Document> {
        if self.finished {
            return None;
        }
        let mut start = None;
        let mut stack = Vec::new();
        let mut quoted = false;
        let mut escaped = false;
        while let Some((relative, ch)) = self.characters.next() {
            let offset = self.base + relative;
            if start.is_none() {
                if !matches!(ch, '{' | '[') {
                    continue;
                }
                start = Some(offset);
            }
            if quoted {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    quoted = false;
                }
                continue;
            }
            match ch {
                '"' => quoted = true,
                '{' => stack.push('}'),
                '[' => stack.push(']'),
                '}' | ']' => {
                    let matched = stack.pop() == Some(ch);
                    if !matched || stack.is_empty() {
                        let begin = start.take().expect("JSON candidate start");
                        stack.clear();
                        if let Some(document) = self.document(begin..offset + 1) {
                            return Some(document);
                        }
                    }
                }
                _ => {}
            }
        }
        self.finished = true;
        start.and_then(|start| self.document(start..self.text.len()))
    }
}

// Truncated, malformed or over-depth JSON still has recognizable JSON string
// keys. If a sensitive escaped key is present, mask the whole candidate rather
// than falling back to an undecoded field-name scan.
fn sensitive_escaped_key(text: &str) -> bool {
    let mut characters = text.char_indices();
    while let Some((start, ch)) = characters.next() {
        if ch != '"' {
            continue;
        }
        let mut escaped = false;
        for (offset, next) in characters.by_ref() {
            if escaped {
                escaped = false;
            } else if next == '\\' {
                escaped = true;
            } else if next == '"' {
                let end = offset + 1;
                let raw = &text[start..end];
                if raw.contains('\\') && text[end..].trim_start().starts_with(':') {
                    if let Ok(name) = serde_json::from_str::<String>(raw) {
                        if super::sensitive_name(&name) || super::auth::basic_auth_property(&name) {
                            return true;
                        }
                    }
                }
                break;
            }
        }
    }
    false
}
