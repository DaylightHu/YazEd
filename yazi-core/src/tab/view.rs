use std::{borrow::Cow, fs::Metadata, io::{self, Read}, ops::{Range, RangeInclusive}, time::SystemTime};

use ratatui_core::{buffer::CellWidth, layout::{Position, Rect}, style::Style};
use ropey::{Rope, RopeSlice};
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};
use yazi_macro::impl_data_any;
use yazi_scheduler::NotifyProxy;
use yazi_shared::{scope::Scope, url::{UrlBuf, UrlLike}};

use super::{search::{Search, SearchHit}, syntax::Syntax};

mod assist;
pub use assist::*;

#[derive(Clone, Debug)]
pub struct View {
	pub url:     UrlBuf,
	pub focused: bool,
	pub insert:  bool,
	pub saving:  bool,
	pub stamp:   Option<FileStamp>,
	pub assist:  Option<Box<ViewAssist>>,
	assist_revision: u64,
	count:           Option<usize>,

	text:    Rope,
	bom:     bool,
	crlf:    bool,
	cursor:  usize,
	column:  usize,
	goal:    Option<usize>,
	top:     usize,
	left:    usize,
	search:  Option<Box<Search>>,
	syntax:  Option<Box<Syntax>>,
	visual:  Option<ViewSelection>,
	undo:    Vec<Change>,
	joining: bool,
	version: usize,
	saved:   usize,
	next:    usize,
}

#[derive(Clone, Copy, Debug)]
struct ViewSelection {
	anchor:   usize,
	linewise: bool,
}

pub enum ViewSelectionSpan {
	Bytes(Range<usize>),
	Lines(RangeInclusive<usize>),
}

pub enum ViewSelectionOp {
	Yank,
	Delete,
	Change,
}

pub struct ViewRegister {
	text:     String,
	linewise: bool,
	crlf:     bool,
}

#[derive(Clone, Debug)]
struct Change {
	cursor:  usize,
	version: usize,
	edits:   Vec<Edit>,
}

#[derive(Clone, Debug)]
struct Edit {
	start:    usize,
	inserted: usize,
	removed:  String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileStamp {
	len:      u64,
	modified: SystemTime,
	#[cfg(unix)]
	identity: (u64, u64, i64, i64),
}

impl FileStamp {
	pub fn new(meta: &Metadata) -> io::Result<Self> {
		#[cfg(unix)]
		use std::os::unix::fs::MetadataExt;

		Ok(Self {
			len:                   meta.len(),
			modified:              meta.modified()?,
			#[cfg(unix)]
			identity:              (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()),
		})
	}
}

impl_data_any!(View);

impl View {
	pub fn read(url: UrlBuf, reader: impl Read, scope: &Scope) -> io::Result<Self> {
		let mut text = Rope::from_reader(TextReader { reader, scope })?;
		let bom = text.get_char(0) == Some('\u{feff}');
		if bom {
			text.remove(..1);
		}

		let first = text.line(0);
		let crlf = first.len_chars() >= 2
			&& first.char(first.len_chars() - 1) == '\n'
			&& first.char(first.len_chars() - 2) == '\r';
		let syntax = url.as_local().and_then(|path| Syntax::detect(path, first));
		Ok(Self {
			url,
			focused: true,
			insert: false,
			saving: false,
			stamp: None,
			assist: None,
			assist_revision: 0,
			count: None,
			text,
			bom,
			crlf,
			cursor: 0,
			column: 0,
			goal: None,
			top: 0,
			left: 0,
			search: None,
			syntax,
			visual: None,
			undo: Vec::new(),
			joining: false,
			version: 0,
			saved: 0,
			next: 0,
		})
	}

	pub fn dirty(&self) -> bool { self.version != self.saved }

	pub fn protect(&self) -> bool {
		if self.saving {
			NotifyProxy::push_warn("Saving text", "Wait for the save to finish.");
		} else if self.dirty() {
			NotifyProxy::push_warn(
				"Unsaved changes",
				format!("{}: save with :w or discard with :q!.", self.url.display()),
			);
		} else {
			return false;
		}
		true
	}

	pub fn end_change(&mut self) { self.joining = false; }

	pub fn snapshot(&mut self) -> (Rope, bool) {
		self.end_change();
		self.saving = true;
		(self.text.clone(), self.bom)
	}

	pub fn mark_saved(&mut self, stamp: Option<FileStamp>) {
		self.stamp = stamp;
		self.saved = self.version;
	}

	pub fn newline(&self) -> &'static str { if self.crlf { "\r\n" } else { "\n" } }

	pub fn pending_count(&self) -> Option<usize> { self.count }

	pub fn push_count(&mut self, digit: usize) {
		self.count = Some(
			self.count.unwrap_or_default().saturating_mul(10).saturating_add(digit).min(999_999),
		);
	}

	pub fn take_count(&mut self) -> usize { self.count.take().unwrap_or(1) }

	pub fn clear_count(&mut self) -> bool { self.count.take().is_some() }

	pub fn enter_insert(&mut self, after: bool) {
		self.count = None;
		if after {
			let line = self.cursor_line();
			let end = self.text.line_to_char(line) + self.line(line).len_chars();
			if self.cursor < end {
				self.horizontal(false);
			}
		}
		self.insert = true;
	}

	pub fn paste(&mut self, text: &str) -> bool {
		let mut clean = String::with_capacity(text.len());
		let mut chars = text.chars().peekable();
		while let Some(c) = chars.next() {
			match c {
				'\r' | '\n' => {
					if c == '\r' && chars.peek() == Some(&'\n') {
						chars.next();
					}
					clean.push_str(self.newline());
				}
				_ if c == '\t' || !c.is_control() => clean.push(c),
				_ => {}
			}
		}
		self.type_text(&clean)
	}

	pub fn type_text(&mut self, text: &str) -> bool {
		if text.is_empty() {
			return false;
		}
		self.replace(self.cursor..self.cursor, text);
		true
	}

	pub fn type_opener(&mut self, opener: char) -> bool {
		let pair = match opener {
			'(' => "()",
			'[' => "[]",
			'{' => "{}",
			_ => return false,
		};
		let cursor = self.cursor;
		self.replace(cursor..cursor, pair);
		self.set_cursor(cursor + 1);
		true
	}

	pub fn insert_newline(&mut self) -> bool {
		let row = self.cursor_line();
		let start = self.text.line_to_char(row);
		let before = self.text.slice(start..self.cursor).to_string();
		let indent = self.current_indent();
		let (tabs, width) = self.indent_style();
		let opener = before.trim_end().chars().next_back();
		let closer = self.text.get_char(self.cursor);
		let split_pair = matches!(
			(opener, closer),
			(Some('{'), Some('}')) | (Some('['), Some(']')) | (Some('('), Some(')'))
		);
		let increase = matches!(opener, Some('{' | '[' | '('));
		let mut first = String::from(self.newline());
		first.push_str(&indent);
		if increase {
			if tabs {
				first.push('\t');
			} else {
				first.extend(std::iter::repeat_n(' ', width));
			}
		}
		let cursor = self.cursor + first.chars().count();
		let mut inserted = first;
		if split_pair {
			inserted.push_str(self.newline());
			inserted.push_str(&indent);
		}
		self.replace(self.cursor..self.cursor, &inserted);
		if split_pair {
			self.set_cursor(cursor);
		}
		true
	}

	pub fn insert_indent(&mut self) -> bool {
		let (tabs, width) = self.indent_style();
		if tabs {
			self.type_text("\t")
		} else if width == 2 {
			self.type_text("  ")
		} else {
			self.type_text("    ")
		}
	}

	pub fn smart_backspace(&mut self) -> bool {
		if self.cursor > 0 {
			let before = self.text.get_char(self.cursor - 1);
			let after = self.text.get_char(self.cursor);
			if matches!(
				(before, after),
				(Some('('), Some(')')) | (Some('['), Some(']')) | (Some('{'), Some('}'))
			) {
				self.replace(self.cursor - 1..self.cursor + 1, "");
				return true;
			}
		}
		let row = self.cursor_line();
		let start = self.text.line_to_char(row);
		let prefix = self.text.slice(start..self.cursor);
		if prefix.len_chars() > 0 && prefix.chars().all(|c| matches!(c, ' ' | '\t')) {
			let remove = if prefix.char(prefix.len_chars() - 1) == '\t' {
				1
			} else {
				let (_, width) = self.indent_style();
				let spaces = prefix.to_string().bytes().rev().take_while(|b| *b == b' ').count();
				(spaces.saturating_sub(1) % width) + 1
			};
			self.replace(self.cursor - remove..self.cursor, "");
			return true;
		}
		self.delete(true)
	}

	pub fn type_closer(&mut self, closer: char) -> bool {
		if self.text.get_char(self.cursor) == Some(closer) {
			self.set_cursor(self.cursor + 1);
			return true;
		}
		let row = self.cursor_line();
		let start = self.text.line_to_char(row);
		let prefix = self.text.slice(start..self.cursor);
		if prefix.len_chars() > 0 && prefix.chars().all(|c| matches!(c, ' ' | '\t')) {
			let remove = if prefix.char(prefix.len_chars() - 1) == '\t' {
				1
			} else {
				let (_, width) = self.indent_style();
				prefix.to_string().bytes().rev().take_while(|b| *b == b' ').count().min(width)
			};
			self.replace(self.cursor - remove..self.cursor, "");
		}
		let mut bytes = [0; 4];
		self.type_text(closer.encode_utf8(&mut bytes))
	}

	fn current_indent(&self) -> String {
		self.line(self.cursor_line()).chars().take_while(|c| matches!(c, ' ' | '\t')).collect()
	}

	fn indent_style(&self) -> (bool, usize) {
		let row = self.cursor_line();
		let mut min_spaces = usize::MAX;
		for row in row.saturating_sub(99)..=row {
			let line = self.line(row);
			let mut spaces = 0;
			for c in line.chars() {
				match c {
					'\t' => return (true, 1),
					' ' => spaces += 1,
					_ => break,
				}
			}
			if spaces > 0 {
				min_spaces = min_spaces.min(spaces);
			}
		}
		(false, if min_spaces == 2 { 2 } else { 4 })
	}

	pub fn delete(&mut self, backward: bool) -> bool {
		let other = self.adjacent(backward);
		if other == self.cursor {
			return false;
		}
		self.replace(other.min(self.cursor)..other.max(self.cursor), "");
		true
	}

	pub fn visual_mode(&self) -> Option<bool> { self.visual.map(|v| v.linewise) }

	pub fn toggle_visual(&mut self, linewise: bool) {
		self.end_change();
		self.clear_search_match();
		self.visual = match self.visual {
			Some(v) if v.linewise == linewise => None,
			Some(v) => Some(ViewSelection { linewise, ..v }),
			None => Some(ViewSelection { anchor: self.cursor, linewise }),
		};
	}

	pub fn cancel_visual(&mut self) -> bool { self.visual.take().is_some() }

	pub fn swap_visual_end(&mut self) -> bool {
		let Some(visual) = &mut self.visual else { return false };
		std::mem::swap(&mut self.cursor, &mut visual.anchor);
		self.goal = None;
		self.update_column();
		true
	}

	pub fn visual_span(&self) -> Option<ViewSelectionSpan> {
		let visual = self.visual?;
		let start = visual.anchor.min(self.cursor);
		let end = visual.anchor.max(self.cursor);
		Some(if visual.linewise {
			ViewSelectionSpan::Lines(self.text.char_to_line(start)..=self.text.char_to_line(end))
		} else {
			let end = boundary(self.text.slice(..), self.text.char_to_byte(end), false);
			ViewSelectionSpan::Bytes(self.text.char_to_byte(start)..end)
		})
	}

	pub fn finish_visual(&mut self, op: ViewSelectionOp) -> Option<ViewRegister> {
		let span = self.visual_span()?;
		self.visual = None;
		let (mut range, lines) = match span {
			ViewSelectionSpan::Bytes(bytes) => {
				(self.text.byte_to_char(bytes.start)..self.text.byte_to_char(bytes.end), None)
			}
			ViewSelectionSpan::Lines(lines) => (
				self.text.line_to_char(*lines.start())..self.text.line_to_char(lines.end() + 1),
				Some(lines),
			),
		};
		let register = (!range.is_empty() || lines.is_some()).then(|| ViewRegister {
			text:     String::from(self.text.slice(range.clone())),
			linewise: lines.is_some(),
			crlf:     self.crlf,
		});

		match op {
			ViewSelectionOp::Yank => self.set_cursor(range.start),
			ViewSelectionOp::Delete => {
				// The final unterminated line has its separator before it.
				if let Some(lines) = lines
					&& *lines.end() == self.line_count() - 1
					&& *lines.start() > 0
				{
					let previous = lines.start() - 1;
					range.start = self.text.line_to_char(previous) + self.line(previous).len_chars();
				}
				if !range.is_empty() {
					self.replace(range, "");
				}
			}
			ViewSelectionOp::Change => {
				self.insert = true;
				let start = range.start;
				let line_break = lines.is_some_and(|rows| *rows.end() + 1 < self.line_count());
				if line_break {
					self.replace(range, self.newline());
					self.set_cursor(start);
				} else if !range.is_empty() {
					self.replace(range, "");
				}
			}
		}
		register
	}

	pub fn put_register(&mut self, register: &ViewRegister, before: bool) -> bool {
		let mut text = Cow::Borrowed(register.text.as_str());
		if register.crlf != self.crlf && text.contains('\n') {
			text = if self.crlf {
				Cow::Owned(text.replace("\r\n", "\n").replace('\n', "\r\n"))
			} else {
				Cow::Owned(text.replace("\r\n", "\n"))
			};
		}

		let line = self.cursor_line();
		let (at, first_line) = if !register.linewise {
			let end = self.text.line_to_char(line) + self.line(line).len_chars();
			(if before { self.cursor } else { self.adjacent(false).min(end) }, None)
		} else if self.text.len_chars() == 0 {
			if text.is_empty() {
				text = Cow::Borrowed(self.newline());
			}
			(0, Some(0))
		} else if before || line + 1 < self.line_count() {
			if !text.ends_with('\n') {
				text.to_mut().push_str(self.newline());
			}
			let start = self.text.line_to_char(line + usize::from(!before));
			(start, Some(start))
		} else {
			let last = text.strip_suffix("\r\n").or_else(|| text.strip_suffix('\n')).unwrap_or(&text);
			text = Cow::Owned(format!("{}{last}", self.newline()));
			let start = self.text.len_chars();
			(start, Some(start + self.newline().chars().count()))
		};
		if text.is_empty() {
			return false;
		}

		self.end_change();
		self.replace(at..at, &text);
		if let Some(start) = first_line {
			self.set_cursor(start);
		} else {
			self.horizontal(true);
		}
		true
	}

	fn replace(&mut self, range: Range<usize>, text: &str) {
		self.assist_revision += 1;
		if !self.joining {
			self.undo.push(Change { cursor: self.cursor, version: self.version, edits: Vec::new() });
			self.next += 1;
			self.version = self.next;
			self.joining = self.insert;
		}

		let start = range.start;
		let removed =
			if range.is_empty() { String::new() } else { String::from(self.text.slice(range.clone())) };
		let inserted = text.chars().count();
		self.invalidate_syntax(range.clone(), text);
		self.text.remove(range);
		self.text.insert(start, text);

		let edits = &mut self.undo.last_mut().unwrap().edits;
		if let Some(last) = edits.last_mut()
			&& last.start + last.inserted == start
			&& removed.is_empty()
		{
			last.inserted += inserted;
		} else {
			edits.push(Edit { start, inserted, removed });
		}

		let offset = self.text.char_to_byte(start + inserted);
		// Inserting or removing text can join the graphemes on either side of the edit.
		let slice = self.text.slice(..);
		let before = boundary(slice, offset, true);
		let after = boundary(slice, before, false);
		self.cursor = slice.byte_to_char(if offset == 0 { 0 } else { after });
		self.goal = None;
		self.update_column();
	}

	pub fn undo(&mut self) -> bool {
		self.end_change();
		let Some(change) = self.undo.pop() else { return false };
		self.assist_revision += 1;
		for edit in change.edits.into_iter().rev() {
			let range = edit.start..edit.start + edit.inserted;
			self.invalidate_syntax(range.clone(), &edit.removed);
			self.text.remove(range);
			self.text.insert(edit.start, &edit.removed);
		}
		self.cursor = change.cursor;
		self.version = change.version;
		self.goal = None;
		self.update_column();
		true
	}

	fn adjacent(&self, backward: bool) -> usize {
		let slice = self.text.slice(..);
		let offset = boundary(slice, slice.char_to_byte(self.cursor), backward);
		slice.byte_to_char(offset)
	}

	pub fn horizontal(&mut self, backward: bool) {
		self.end_change();
		self.cursor = self.adjacent(backward);
		self.goal = None;
		self.update_column();
	}

	pub fn horizontal_count(&mut self, backward: bool, count: usize) {
		for _ in 0..count {
			let before = self.cursor;
			self.horizontal(backward);
			if self.cursor == before {
				break;
			}
		}
	}

	pub fn line_edge(&mut self, end: bool) {
		self.end_change();
		let line = self.cursor_line();
		let start = self.text.line_to_char(line);
		self.cursor = if end { start + self.line(line).len_chars() } else { start };
		self.goal = None;
		self.update_column();
	}

	pub fn vertical(&mut self, delta: isize) {
		let line = self.cursor_line().saturating_add_signed(delta).min(self.line_count() - 1);
		self.move_to_line(line);
	}

	pub fn move_to_line(&mut self, line: usize) {
		self.end_change();
		let goal = *self.goal.get_or_insert(self.column);
		let mut offset = 0;
		let mut column = 0;
		for grapheme in graphemes(self.line(line)) {
			let next = column + cell_width(grapheme, column);
			if next > goal {
				break;
			}
			column = next;
			offset += grapheme.len_chars();
		}
		self.cursor = self.text.line_to_char(line) + offset;
		self.column = column;
		self.clear_search_match();
	}

	pub fn cursor_line(&self) -> usize { self.text.char_to_line(self.cursor) }

	fn set_cursor(&mut self, cursor: usize) {
		self.cursor = cursor;
		self.goal = None;
		self.update_column();
	}

	fn update_column(&mut self) {
		self.clear_search_match();
		let start = self.text.line_to_char(self.cursor_line());
		let prefix = self.text.slice(start..self.cursor);
		self.column = graphemes(prefix).fold(0, |col, g| col + cell_width(g, col));
	}

	pub fn start_search(&mut self, query: String, backward: bool) -> bool {
		self.search = Search::new(query, backward).map(Box::new);
		self.find(backward, false).unwrap_or(false)
	}

	pub fn repeat_search(&mut self, reverse: bool) -> Option<bool> {
		self.find(self.search.as_ref()?.backward ^ reverse, true)
	}

	fn find(&mut self, backward: bool, repeat: bool) -> Option<bool> {
		let search = self.search.as_ref()?;
		let from = search
			.hit
			.as_ref()
			.map_or_else(|| self.text.char_to_byte(self.cursor), |hit| hit.bytes.start);
		let from = (from + usize::from(repeat && !backward)).min(self.text.len_bytes());
		let Some(bytes) = search.find(&self.text, from, backward) else {
			return Some(false);
		};

		let slice = self.text.slice(..);
		// The query can start or end inside a grapheme, but the cursor and highlight use whole graphemes.
		let (start, _) = grapheme_bounds(slice, bytes.start);
		let (_, end) = grapheme_bounds(slice, bytes.end);
		self.end_change();
		self.cursor = self.text.byte_to_char(start);
		self.goal = None;
		self.update_column();

		let line = self.cursor_line();
		let end_column = graphemes(self.text.byte_slice(start..end))
			.fold(self.column, |col, g| col + cell_width(g, col));
		self.search.as_mut()?.hit =
			Some(SearchHit { bytes, line, line_start: self.text.line_to_byte(line), end_column });
		Some(true)
	}

	pub fn match_on_line(&self, line: usize) -> Option<Range<usize>> {
		let hit = self.search.as_ref()?.hit.as_ref().filter(|hit| hit.line == line)?;
		Some(hit.bytes.start - hit.line_start..hit.bytes.end - hit.line_start)
	}

	fn clear_search_match(&mut self) {
		if let Some(search) = &mut self.search {
			search.hit = None;
		}
	}

	fn invalidate_syntax(&mut self, range: Range<usize>, inserted: &str) {
		if let Some(syntax) = &mut self.syntax {
			let row = self.text.char_to_line(range.start);
			let lines_shifted =
				row != self.text.char_to_line(range.end) || inserted.as_bytes().contains(&b'\n');
			syntax.invalidate(row, lines_shifted);
		}
	}

	pub fn prepare_syntax(&mut self, rows: Range<usize>, ready: fn()) {
		if let Some(syntax) = &mut self.syntax {
			syntax.prepare(&self.text, rows, ready);
		}
	}

	/// Style runs ending at exclusive UTF-8 byte offsets within the line.
	pub fn syntax_styles(&self, row: usize) -> &[(usize, Style)] {
		self.syntax.as_ref().map_or(&[], |s| s.styles(row))
	}

	pub fn line_count(&self) -> usize { self.text.len_lines() }

	pub fn line_byte(&self, index: usize) -> usize { self.text.line_to_byte(index) }

	pub fn line(&self, index: usize) -> RopeSlice<'_> {
		let line = self.text.line(index);
		let mut end = line.len_chars();
		if end > 0 && line.char(end - 1) == '\n' {
			end -= 1;
			if end > 0 && line.char(end - 1) == '\r' {
				end -= 1;
			}
		}
		line.slice(..end)
	}

	pub fn reveal(&mut self, width: usize, height: usize) {
		(self.top, self.left) = self.viewport(width, height);
	}

	pub fn gutter_width(&self, width: usize) -> usize {
		if width < 3 {
			return 0;
		}
		let digits = self.line_count().max(1).ilog10() as usize + 1;
		(digits + 1).min(width - 1)
	}

	pub fn viewport(&self, width: usize, height: usize) -> (usize, usize) {
		let width = width.saturating_sub(self.gutter_width(width)).max(1);
		let line = self.cursor_line();
		let top = self
			.top
			.clamp(line.saturating_sub(height.saturating_sub(1)), line)
			.min(self.line_count().saturating_sub(height.max(1)));
		let left = if self
			.search
			.as_ref()
			.and_then(|search| search.hit.as_ref())
			.is_some_and(|hit| hit.end_column > self.left.saturating_add(width))
		{
			self.column
		} else {
			self.left.clamp(self.column.saturating_sub(width.saturating_sub(1)), self.column)
		};
		(top, left)
	}

	pub fn screen_cursor(&self, area: Rect) -> Option<Position> {
		if area.width == 0 || area.height < 2 {
			return None;
		}
		let gutter = self.gutter_width(area.width as usize);
		let (top, left) = self.viewport(area.width as usize, area.height as usize - 1);
		Some(Position {
			x: area.x + gutter as u16 + (self.column - left) as u16,
			y: area.y + (self.cursor_line() - top) as u16,
		})
	}

	pub fn cursor_cells(&self) -> usize {
		let row = self.cursor_line();
		let line = self.line(row);
		let start = self.text.char_to_byte(self.cursor) - self.line_byte(row);
		if start >= line.len_bytes() || line.byte(start) == b'\t' {
			return 1;
		}
		let end = boundary(line, start, false);
		cell_width(line.byte_slice(start..end), self.column).max(1)
	}
}

struct TextReader<'a, R> {
	reader: R,
	scope:  &'a Scope,
}

impl<R: Read> Read for TextReader<'_, R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		if self.scope.is_cancelled() {
			return Err(io::ErrorKind::Interrupted.into());
		}
		let len = self.reader.read(buf)?;
		if buf[..len].iter().any(|b| matches!(b, 0..=8 | 11..=12 | 14..=31 | 127)) {
			return Err(io::ErrorKind::InvalidData.into());
		}
		Ok(len)
	}
}

pub fn graphemes(slice: RopeSlice<'_>) -> impl Iterator<Item = RopeSlice<'_>> {
	let mut cursor = GraphemeCursor::new(0, slice.len_bytes(), true);
	std::iter::from_fn(move || {
		let start = cursor.cur_cursor();
		let end = seek_boundary(slice, &mut cursor, false)?;
		Some(slice.byte_slice(start..end))
	})
}

pub fn cell_width(grapheme: RopeSlice<'_>, column: usize) -> usize {
	let text = grapheme_text(grapheme);
	if text == "\t" { 4 - column % 4 } else { text.cell_width() as usize }
}

pub fn grapheme_text(grapheme: RopeSlice<'_>) -> Cow<'_, str> {
	if grapheme.char(0).is_control() && grapheme.char(0) != '\t' {
		Cow::Borrowed("�")
	} else {
		grapheme.into()
	}
}

fn boundary(slice: RopeSlice<'_>, offset: usize, backward: bool) -> usize {
	let mut cursor = GraphemeCursor::new(offset, slice.len_bytes(), true);
	seek_boundary(slice, &mut cursor, backward).unwrap_or(if backward {
		0
	} else {
		slice.len_bytes()
	})
}

fn grapheme_bounds(slice: RopeSlice<'_>, offset: usize) -> (usize, usize) {
	let previous = boundary(slice, offset, true);
	let next = boundary(slice, previous, false);
	if next > offset && offset > previous { (previous, next) } else { (offset, offset) }
}

fn seek_boundary(
	slice: RopeSlice<'_>,
	cursor: &mut GraphemeCursor,
	backward: bool,
) -> Option<usize> {
	let offset = cursor.cur_cursor();
	if (backward && offset == 0) || (!backward && offset == slice.len_bytes()) {
		return None;
	}
	let at = if backward { offset - 1 } else { offset };
	let (mut chunk, mut start, ..) = slice.chunk_at_byte(at);

	loop {
		let result = if backward {
			cursor.prev_boundary(chunk, start)
		} else {
			cursor.next_boundary(chunk, start)
		};
		match result {
			Ok(value) => return value,
			Err(GraphemeIncomplete::PrevChunk) => (chunk, start, _, _) = slice.chunk_at_byte(start - 1),
			Err(GraphemeIncomplete::NextChunk) => {
				(chunk, start, _, _) = slice.chunk_at_byte(start + chunk.len())
			}
			Err(GraphemeIncomplete::PreContext(end)) => {
				let (context, start, ..) = slice.chunk_at_byte(end - 1);
				cursor.provide_context(&context[..end - start], start);
			}
			Err(GraphemeIncomplete::InvalidOffset) => unreachable!(),
		}
	}
}
