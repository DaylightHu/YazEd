use std::ops::Range;

use serde_json::{Value, json};
use yazi_core::tab::{AssistEdit, AssistItem, AssistSignature, AssistSource};

#[derive(Clone, Copy)]
pub(super) enum Encoding {
	Utf8,
	Utf16,
	Utf32,
}

impl Encoding {
	fn units(self, text: &str) -> usize {
		match self {
			Self::Utf8 => text.len(),
			Self::Utf16 => text.encode_utf16().count(),
			Self::Utf32 => text.chars().count(),
		}
	}

	pub(super) fn position(self, source: &AssistSource, offset: usize) -> Value {
		let line = source.text.char_to_line(offset);
		let start = source.text.line_to_char(line);
		let character: usize = source.text.slice(start..offset).chunks().map(|s| self.units(s)).sum();
		json!({"line": line, "character": character})
	}

	fn offset(self, source: &AssistSource, position: &Value) -> Option<usize> {
		let row = usize::try_from(position["line"].as_u64()?).ok()?;
		let column = usize::try_from(position["character"].as_u64()?).ok()?;
		let line = source.text.get_line(row)?;
		let mut units = 0;
		let mut chars = 0;
		for c in line.chars() {
			if units == column {
				break;
			}
			if matches!(c, '\r' | '\n') {
				return None;
			}
			units += match self {
				Self::Utf8 => c.len_utf8(),
				Self::Utf16 => c.len_utf16(),
				Self::Utf32 => 1,
			};
			chars += 1;
		}
		(units == column).then(|| source.text.line_to_char(row) + chars)
	}

	fn range(self, source: &AssistSource, range: &Value) -> Option<Range<usize>> {
		let start = self.offset(source, &range["start"])?;
		let end = self.offset(source, &range["end"])?;
		(start <= end).then_some(start..end)
	}
}

pub(super) fn prefix(source: &AssistSource) -> Range<usize> {
	let cursor = source.key.cursor;
	let start = source
		.text
		.chars_at(cursor)
		.reversed()
		.take(128)
		.take_while(|c| c.is_alphanumeric() || *c == '_')
		.count();
	cursor - start..cursor
}

pub(super) fn wants_completion(source: &AssistSource) -> bool {
	let word = prefix(source);
	let last = source.key.cursor.checked_sub(1).and_then(|i| source.text.get_char(i));
	word.len() >= 2 || matches!(last, Some('.' | '>' | ':')) || source.manual
}

pub(super) fn in_call(source: &AssistSource) -> bool {
	// Bound context inspection so a malformed file cannot stall the worker.
	let start = source.key.cursor.saturating_sub(4096);
	let mut chars = source.text.slice(start..source.key.cursor).chars().peekable();
	let (mut depth, mut quote, mut block, mut line_comment) = (0_usize, None, false, false);
	while let Some(c) = chars.next() {
		if line_comment {
			if c == '\n' {
				line_comment = false;
			}
			continue;
		}
		if block {
			if c == '*' && chars.peek() == Some(&'/') {
				chars.next();
				block = false;
			}
			continue;
		}
		if let Some(q) = quote {
			if c == '\\' {
				chars.next();
			} else if c == q {
				quote = None;
			}
			continue;
		}
		match c {
			'"' | '\'' => quote = Some(c),
			'/' if chars.peek() == Some(&'/') => {
				chars.next();
				line_comment = true;
			}
			'/' if chars.peek() == Some(&'*') => {
				chars.next();
				block = true;
			}
			'(' => depth += 1,
			')' => depth = depth.saturating_sub(1),
			'{' | '}' | ';' if depth == 0 => depth = 0,
			_ => {}
		}
	}
	depth > 0 && quote.is_none() && !block && !line_comment
}

pub(super) fn items(value: Value, source: &AssistSource, encoding: Encoding) -> Vec<AssistItem> {
	let entries = value.as_array().or_else(|| value["items"].as_array());
	let defaults = &value["itemDefaults"];
	let mut items = Vec::new();
	for item in entries.into_iter().flatten().take(512) {
		if items.len() >= 80 {
			break;
		}
		let format = item.get("insertTextFormat").or_else(|| defaults.get("insertTextFormat"));
		if format.and_then(Value::as_u64) == Some(2) {
			continue;
		}
		let Some(label) = item["label"].as_str().filter(|s| !s.is_empty() && s.len() <= 512) else {
			continue;
		};
		let edit = &item["textEdit"];
		let range = edit
			.get("range")
			.or_else(|| edit.get("insert"))
			.or_else(|| defaults["editRange"].get("insert"))
			.or_else(|| defaults.get("editRange"));
		let range = match range {
			Some(range) => encoding.range(source, range),
			None => Some(prefix(source)),
		};
		let Some(range) = range else { continue };
		if !range.contains(&source.key.cursor) && range.end != source.key.cursor {
			continue;
		}
		let text = edit["newText"]
			.as_str()
			.or_else(|| item["textEditText"].as_str())
			.or_else(|| item["insertText"].as_str())
			.unwrap_or(label);
		if text.len() > 64 * 1024 || source.text.slice(range.clone()) == text {
			continue;
		}
		let mut edits = vec![AssistEdit { range, text: text.into() }];
		let extras = item["additionalTextEdits"].as_array();
		if extras.is_some_and(|e| e.len() > 32) {
			continue;
		}
		let mut valid = true;
		for extra in extras.into_iter().flatten() {
			match (encoding.range(source, &extra["range"]), extra["newText"].as_str()) {
				(Some(range), Some(text)) if text.len() <= 64 * 1024 => {
					edits.push(AssistEdit { range, text: text.into() })
				}
				_ => {
					valid = false;
					break;
				}
			}
		}
		if !valid {
			continue;
		}
		let detail =
			item["detail"].as_str().or_else(|| item["labelDetails"]["detail"].as_str()).unwrap_or("");
		items.push(AssistItem { label: clean(label, 200), detail: clean(detail, 500), edits });
	}
	items
}

pub(super) fn signature(value: Value) -> Option<AssistSignature> {
	let signatures = value["signatures"].as_array()?;
	let active = value["activeSignature"].as_u64().unwrap_or(0) as usize;
	let signature = signatures.get(active).or_else(|| signatures.first())?;
	let label = signature["label"].as_str()?.to_owned();
	if label.len() > 4096 || label.contains(char::is_control) {
		return None;
	}
	let parameter = signature
		.get("activeParameter")
		.or_else(|| value.get("activeParameter"))
		.and_then(Value::as_u64)
		.unwrap_or(0) as usize;
	let parameter = signature["parameters"].get(parameter).and_then(|p| {
		if let Some(text) = p["label"].as_str() {
			let start = label.find(text)?;
			return Some(start..start + text.len());
		}
		let offsets = p["label"].as_array()?;
		let start = utf16_byte(&label, offsets.first()?.as_u64()? as usize)?;
		let end = utf16_byte(&label, offsets.get(1)?.as_u64()? as usize)?;
		(start <= end).then_some(start..end)
	});
	Some(AssistSignature { label, parameter })
}

fn utf16_byte(text: &str, offset: usize) -> Option<usize> {
	let mut units = 0;
	for (byte, c) in text.char_indices() {
		if units == offset {
			return Some(byte);
		}
		units += c.len_utf16();
	}
	(units == offset).then_some(text.len())
}

pub(super) fn clean(text: &str, limit: usize) -> String {
	text.chars().take(limit).map(|c| if c.is_control() { ' ' } else { c }).collect()
}

#[cfg(test)]
mod tests {
	use super::*;
	use yazi_core::tab::View;
	use yazi_shared::scope::Scope;

	fn view(text: &str) -> View {
		let mut view = View::read(
			std::path::PathBuf::from("/tmp/completion.txt").into(),
			text.as_bytes(),
			&Scope::default(),
		)
		.unwrap();
		view.insert = true;
		view.line_edge(true);
		view
	}

	#[test]
	fn unicode_positions_and_text_edits() {
		let mut view = view("😀 名字 TCSA");
		let source = view.assist_source().unwrap();
		assert_eq!(prefix(&source), 5..9);
		assert!(wants_completion(&source));
		for (encoding, start, end) in
			[(Encoding::Utf8, 12, 16), (Encoding::Utf16, 6, 10), (Encoding::Utf32, 5, 9)]
		{
			let value = json!([{"label": "TCSANOW", "textEdit": {"range": {
				"start": {"line": 0, "character": start}, "end": {"line": 0, "character": end}
			}, "newText": "TCSANOW"}}]);
			let items = items(value, &source, encoding);
			assert_eq!(items.len(), 1);
			assert_eq!(items[0].edits[0].range, 5..9);
			assert_eq!(encoding.position(&source, 9)["character"], end);
		}
		assert!(Encoding::Utf16.offset(&source, &json!({"line":0,"character":1})).is_none());
	}

	#[test]
	fn revisions_change_inside_one_undo_group() {
		let mut view = view("TCSA");
		let before = view.assist_source().unwrap();
		view.type_text("N");
		let after = view.assist_source().unwrap();
		view.type_text("O");
		assert_ne!(before.key.revision, after.key.revision);
		assert_ne!(after.key.revision, view.assist_source().unwrap().key.revision);
		view.undo();
		assert_ne!(before.key.revision, view.assist_source().unwrap().key.revision);
	}

	#[test]
	fn completion_acceptance_and_undo_preserve_prefix() {
		let mut view = view("TCSA");
		view.assist_source();
		view.assist.as_mut().unwrap().items = vec![AssistItem {
			label: "TCSANOW".into(),
			detail: String::new(),
			edits: vec![AssistEdit { range: 0..4, text: "TCSANOW".into() }],
		}];
		assert!(view.assist_accept());
		assert_eq!(view.line(0).to_string(), "TCSANOW");
		assert!(view.undo());
		assert_eq!(view.line(0).to_string(), "TCSA");
	}

	#[test]
	fn signature_offsets_use_utf16_and_comments_hide_calls() {
		let help =
			signature(json!({"signatures":[{"label":"f(😀, int x)","parameters":[{"label":[6,11]}]}]}))
				.unwrap();
		assert_eq!(&help.label[help.parameter.unwrap()], "int x");
		assert!(in_call(&view("f(1, g(2), ").assist_source().unwrap()));
		assert!(!in_call(&view("// f(").assist_source().unwrap()));
		assert!(!in_call(&view("f(\"abc(").assist_source().unwrap()));
	}
}
