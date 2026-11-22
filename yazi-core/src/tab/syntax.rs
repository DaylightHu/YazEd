use std::{borrow::Cow, collections::{BTreeMap, HashMap}, ops::Range, path::Path, sync::{Arc, OnceLock, Weak, mpsc}, thread};

use parking_lot::Mutex;
use ratatui_core::style::Style;
use ropey::{Rope, RopeSlice};
use syntect::{highlighting::{self, HighlightState, RangedHighlightIterator, Theme}, parsing::{ParseState, ScopeStack, SyntaxReference}};
use yazi_shared::id::{Id, Ids};

use crate::highlighter::{Highlighter, MAX_SYNTAX_LINE_BYTES, SYNTAXES};

const CHECKPOINT_INTERVAL: usize = 64;
static WORKER: OnceLock<Option<mpsc::Sender<Work>>> = OnceLock::new();

type LineStyles = Vec<(usize, Style)>;
type Reply = Mutex<Option<StyledLines>>;

#[derive(Debug)]
pub(super) struct Syntax {
	id:          Id,
	language:    &'static SyntaxReference,
	theme:       Option<Arc<Theme>>,
	top:         usize,
	lines:       Vec<LineStyles>,
	valid:       usize,
	plain_from:  Option<usize>,
	invalidated: Option<usize>,
	pending:     Option<(Range<usize>, Arc<Reply>)>,
}

impl Clone for Syntax {
	fn clone(&self) -> Self { Self::new(self.language) }
}

impl Drop for Syntax {
	fn drop(&mut self) {
		self.pending = None;
		if let Some(worker) = WORKER.get().and_then(Option::as_ref) {
			let _ = worker.send(Work::Close(self.id));
		}
	}
}

impl Syntax {
	fn new(language: &'static SyntaxReference) -> Self {
		static IDS: Ids = Ids::new();
		Self {
			id: IDS.next(),
			language,
			theme: None,
			top: 0,
			lines: Vec::new(),
			valid: 0,
			plain_from: None,
			invalidated: None,
			pending: None,
		}
	}

	pub(super) fn detect(path: &Path, first: RopeSlice<'_>) -> Option<Box<Self>> {
		let language = Highlighter::syntax_for_path(path).or_else(|| {
			if first.len_bytes() <= MAX_SYNTAX_LINE_BYTES {
				SYNTAXES.find_syntax_by_first_line(&Cow::<str>::from(first))
			} else {
				None
			}
		})?;
		(language.name != "Plain Text").then(|| Box::new(Self::new(language)))
	}

	pub(super) fn invalidate(&mut self, line: usize, lines_shifted: bool) {
		self.valid = self.valid.min(line.saturating_sub(self.top));
		if lines_shifted {
			self.lines.truncate(self.valid);
		} else if let Some(styles) = line.checked_sub(self.top).and_then(|row| self.lines.get_mut(row))
		{
			// Keep colors on untouched rows while the worker updates their scopes.
			styles.clear();
		}
		self.invalidated = Some(self.invalidated.map_or(line, |previous| previous.min(line)));
		if self.plain_from.is_some_and(|row| row >= line) {
			self.plain_from = None;
		}
		self.pending = None;
	}

	pub(super) fn prepare(&mut self, text: &Rope, rows: Range<usize>, ready: fn()) {
		let theme = Highlighter::load_theme();
		if self.theme.as_ref().is_none_or(|previous| !Arc::ptr_eq(previous, &theme)) {
			self.invalidate(0, true);
			self.theme = Some(theme.clone());
		}

		let completed = self.pending.as_ref().and_then(|(_, reply)| reply.lock().take());
		if let Some(styled) = completed {
			self.pending = None;
			self.invalidated = None;
			self.lines.truncate(self.valid);
			self.lines.extend(styled.lines);
			self.valid = self.lines.len();
			self.plain_from = styled.plain_from;
		}

		if let Some(discard) = rows.start.checked_sub(self.top) {
			self.lines.drain(..discard.min(self.lines.len()));
			self.valid = self.valid.saturating_sub(discard);
		} else {
			self.lines.clear();
			self.valid = 0;
		}
		self.top = rows.start;
		self.lines.truncate(rows.len());
		self.valid = self.valid.min(rows.len());

		let start = self.top + self.valid;
		let end = rows.end.min(self.plain_from.unwrap_or(rows.end));
		if start >= end {
			self.pending = None;
			return;
		}

		let missing = start..end;
		if self.pending.as_ref().is_some_and(|(rows, _)| *rows == missing) {
			return;
		}
		self.pending = None;
		let Some(worker) = WORKER.get_or_init(start_worker) else { return };

		let reply = Arc::new(Mutex::new(None));
		let request = Request {
			id: self.id,
			language: self.language,
			theme,
			text: text.clone(),
			rows: missing.clone(),
			invalidated: self.invalidated,
			reply: Arc::downgrade(&reply),
			ready,
		};
		if worker.send(Work::Highlight(request)).is_ok() {
			self.pending = Some((missing, reply));
		}
	}

	pub(super) fn styles(&self, row: usize) -> &[(usize, Style)] {
		row.checked_sub(self.top).and_then(|row| self.lines.get(row)).map_or(&[], Vec::as_slice)
	}
}

enum Work {
	Highlight(Request),
	Close(Id),
}

struct Request {
	id:          Id,
	language:    &'static SyntaxReference,
	theme:       Arc<Theme>,
	text:        Rope,
	rows:        Range<usize>,
	invalidated: Option<usize>,
	reply:       Weak<Reply>,
	ready:       fn(),
}

fn start_worker() -> Option<mpsc::Sender<Work>> {
	let (tx, rx) = mpsc::channel();
	// The Oniguruma-backed parser state stays on this thread. Only styles cross back to the UI.
	thread::Builder::new()
		.name("yazi-syntax".into())
		.spawn(move || {
			let mut parsers = HashMap::new();
			while let Ok(work) = rx.recv() {
				match work {
					Work::Close(id) => drop(parsers.remove(&id)),
					Work::Highlight(request) if request.reply.strong_count() != 0 => {
						let parser = parsers.entry(request.id).or_insert_with(Parser::default);
						let styled = parser.highlight(&request);
						if let Some(reply) = request.reply.upgrade() {
							*reply.lock() = Some(styled);
							(request.ready)();
						}
					}
					Work::Highlight(_) => {}
				}
			}
		})
		.ok()?;
	Some(tx)
}

type State = (ParseState, HighlightState);

#[derive(Default)]
struct Parser {
	theme:       Option<Arc<Theme>>,
	checkpoints: BTreeMap<usize, State>,
	tail:        Option<(usize, State)>,
	plain_from:  Option<usize>,
}

#[derive(Debug)]
struct StyledLines {
	lines:      Vec<LineStyles>,
	plain_from: Option<usize>,
}

impl Parser {
	fn highlight(&mut self, request: &Request) -> StyledLines {
		if self.theme.as_ref().is_none_or(|theme| !Arc::ptr_eq(theme, &request.theme)) {
			*self = Self { theme: Some(request.theme.clone()), ..Self::default() };
		} else if let Some(row) = request.invalidated {
			// Each checkpoint contains the state before its line.
			self.checkpoints.split_off(&(row + 1));
			if self.tail.as_ref().is_some_and(|(line, _)| *line > row) {
				self.tail = None;
			}
			if self.plain_from.is_some_and(|line| line >= row) {
				self.plain_from = None;
			}
		}

		let range = &request.rows;
		let checkpoint =
			self.checkpoints.range(..=range.start).next_back().map(|(&row, state)| (row, state));
		let tail =
			self.tail.as_ref().filter(|(row, _)| *row <= range.start).map(|(row, state)| (*row, state));
		let highlighter = highlighting::Highlighter::new(&request.theme);
		let (from, (mut parser, mut state)) = checkpoint
			.into_iter()
			.chain(tail)
			.max_by_key(|(row, _)| *row)
			.map(|(row, state)| (row, state.clone()))
			.unwrap_or_else(|| {
				(
					0,
					(ParseState::new(request.language), HighlightState::new(&highlighter, ScopeStack::new())),
				)
			});

		let mut lines = Vec::with_capacity(range.len());
		let mut normalized = String::new();
		let mut next = from;
		for (row, line) in
			(from..range.end.min(self.plain_from.unwrap_or(range.end))).zip(request.text.lines_at(from))
		{
			if request.reply.strong_count() == 0 {
				break;
			}
			if row != 0 && row.is_multiple_of(CHECKPOINT_INTERVAL) {
				self.checkpoints.entry(row).or_insert_with(|| (parser.clone(), state.clone()));
			}

			// Subsequent lines may depend on scopes opened or closed on this one.
			let Some(line) = Self::line(line, &mut normalized) else {
				self.plain_from = Some(row);
				break;
			};
			let Ok(ops) = parser.parse_line(&line, &SYNTAXES) else {
				self.plain_from = Some(row);
				break;
			};
			let regions = RangedHighlightIterator::new(&mut state, &ops, &line, &highlighter);
			if row < range.start {
				regions.for_each(drop);
			} else {
				lines.push(
					regions.map(|(style, _, range)| (range.end, Highlighter::to_style(style))).collect(),
				);
			}
			next = row + 1;
		}

		self.tail = self.plain_from.is_none_or(|row| row > next).then_some((next, (parser, state)));
		StyledLines { lines, plain_from: self.plain_from }
	}

	fn line<'a>(line: RopeSlice<'a>, normalized: &'a mut String) -> Option<Cow<'a, str>> {
		if line.len_bytes() > MAX_SYNTAX_LINE_BYTES + 1 {
			return None;
		}

		let line = Cow::<str>::from(line);
		let line = if let Some(content) = line.strip_suffix("\r\n") {
			normalized.clear();
			normalized.push_str(content);
			normalized.push('\n');
			Cow::Borrowed(normalized.as_str())
		} else {
			line
		};
		(line.len() <= MAX_SYNTAX_LINE_BYTES).then_some(line)
	}
}
