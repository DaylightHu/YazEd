use ropey::{Rope, RopeSlice};
use yazi_shared::{id::Id, url::UrlBuf};

use super::View;

#[derive(Clone, Copy, Debug, Default)]
pub enum PositionEncoding {
	Utf8,
	#[default]
	Utf16,
	Utf32,
}

impl PositionEncoding {
	pub fn units(self, ch: char) -> usize {
		match self {
			Self::Utf8 => ch.len_utf8(),
			Self::Utf16 => ch.len_utf16(),
			Self::Utf32 => 1,
		}
	}
}

#[derive(Clone, Debug)]
pub struct ViewSource {
	pub url: UrlBuf,
	pub identity: Id,
	pub revision: usize,
	pub cursor: usize,
	pub text: Rope,
}

impl ViewSource {
	pub fn cursor_position(&self, encoding: PositionEncoding) -> (usize, usize) {
		let line = self.text.char_to_line(self.cursor);
		let start = self.text.line_to_char(line);
		(line, self.text.slice(start..self.cursor).chars().map(|c| encoding.units(c)).sum())
	}

	pub fn line(&self, line: usize) -> Option<RopeSlice<'_>> {
		self.text.get_line(line)
	}
}

#[derive(Clone, Copy, Debug)]
pub struct ViewPosition {
	pub(super) cursor: usize,
	pub(super) goal: Option<usize>,
	pub(super) top: usize,
	pub(super) left: usize,
}

pub enum ViewReturn {
	Document(Box<View>),
	Position(ViewPosition),
}

#[derive(Clone, Debug)]
pub struct ViewDefinition {
	pub request: Id,
	pub targets: Vec<DefinitionTarget>,
	pub selected: usize,
	pub opening: bool,
}

#[derive(Clone, Debug)]
pub struct DefinitionTarget {
	pub url: UrlBuf,
	pub line: usize,
	pub column: usize,
	pub encoding: PositionEncoding,
	pub first_line: usize,
	pub lines: Vec<String>,
}
