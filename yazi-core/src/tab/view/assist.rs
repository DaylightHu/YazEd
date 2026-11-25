use std::{ops::Range, path::PathBuf};

use ropey::Rope;
use yazi_shared::{id::Id, url::UrlLike};

use super::View;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AssistKey {
	pub identity: Id,
	pub revision: u64,
	pub cursor: usize,
	pub request: u64,
}

#[derive(Clone, Debug)]
pub struct AssistSource {
	pub key: AssistKey,
	pub path: PathBuf,
	pub text: Rope,
	pub manual: bool,
}

#[derive(Clone, Debug)]
pub struct AssistEdit {
	pub range: Range<usize>,
	pub text: String,
}

#[derive(Clone, Debug)]
pub struct AssistItem {
	pub label: String,
	pub detail: String,
	pub edits: Vec<AssistEdit>,
}

#[derive(Clone, Debug)]
pub struct AssistSignature {
	pub label: String,
	pub parameter: Option<Range<usize>>,
}

#[derive(Clone, Debug)]
pub struct ViewAssist {
	identity: Id,
	request: u64,
	pub items: Vec<AssistItem>,
	pub selected: usize,
	pub signature: Option<AssistSignature>,
	pub status: Option<String>,
	pub dismissed: bool,
}

impl Default for ViewAssist {
	fn default() -> Self {
		Self {
			identity: Id::unique(),
			request: 0,
			items: Vec::new(),
			selected: 0,
			signature: None,
			status: None,
			dismissed: false,
		}
	}
}

impl View {
	pub fn assist_key(&self) -> Option<AssistKey> {
		let assist = self.assist.as_ref()?;
		Some(AssistKey {
			identity: assist.identity,
			revision: self.assist_revision,
			cursor: self.cursor,
			request: assist.request,
		})
	}

	pub fn assist_source(&mut self) -> Option<AssistSource> {
		// Rope clones share their storage. Serialization happens on the worker.
		if !self.focused || !self.insert || self.saving || self.text.len_bytes() > 4 * 1024 * 1024 {
			return None;
		}
		let path = self.url.as_local()?.to_owned();
		self.assist.get_or_insert_with(Default::default);
		Some(AssistSource { key: self.assist_key()?, path, text: self.text.clone(), manual: false })
	}

	pub fn assist_request(&mut self) {
		self.assist.get_or_insert_with(Default::default).request += 1;
	}

	pub fn assist_clear(&mut self) {
		if let Some(assist) = &mut self.assist {
			assist.items.clear();
			assist.signature = None;
			assist.status = None;
			assist.selected = 0;
			assist.dismissed = false;
		}
	}

	pub fn assist_dismiss(&mut self) {
		self.assist_clear();
		if let Some(assist) = &mut self.assist {
			assist.dismissed = true;
		}
	}

	pub fn assist_accept(&mut self) -> bool {
		let Some(assist) = &mut self.assist else { return false };
		let Some(item) = assist.items.get(assist.selected) else { return false };
		let mut edits = item.edits.clone();
		let Some(primary) = edits.first() else { return false };
		let mut cursor = primary.range.start + primary.text.chars().count();
		for edit in edits.iter().skip(1).filter(|e| e.range.end <= primary.range.start) {
			cursor = cursor
				.saturating_add_signed(edit.text.chars().count() as isize - edit.range.len() as isize);
		}
		edits.sort_by_key(|edit| (edit.range.start, edit.range.end));
		if edits.iter().any(|e| e.range.start > e.range.end || e.range.end > self.text.len_chars())
			|| edits.windows(2).any(|pair| {
				pair[0].range.end > pair[1].range.start || pair[0].range.start == pair[1].range.start
			}) {
			return false;
		}
		self.assist_clear();
		self.end_change();
		for edit in edits.into_iter().rev() {
			self.replace(edit.range, &edit.text);
		}
		self.set_cursor(cursor.min(self.text.len_chars()));
		self.end_change();
		true
	}
}
