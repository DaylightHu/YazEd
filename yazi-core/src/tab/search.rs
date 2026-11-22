use std::ops::Range;

use ropey::Rope;

#[derive(Clone, Debug)]
pub(super) struct Search {
	pub(super) backward: bool,
	pub(super) hit:      Option<SearchHit>,
	forward:             Matcher,
	reverse:             Matcher,
}

#[derive(Clone, Debug)]
pub(super) struct SearchHit {
	pub(super) bytes:      Range<usize>,
	pub(super) line:       usize,
	pub(super) line_start: usize,
	pub(super) end_column: usize,
}

impl Search {
	pub(super) fn new(query: String, backward: bool) -> Option<Self> {
		if query.is_empty() {
			return None;
		}
		let bytes = query.into_bytes();
		let reverse = bytes.iter().copied().rev().collect();
		Some(Self { backward, hit: None, forward: Matcher::new(bytes), reverse: Matcher::new(reverse) })
	}

	pub(super) fn find(&self, text: &Rope, from: usize, backward: bool) -> Option<Range<usize>> {
		let total = text.len_bytes();
		let length = self.forward.bytes.len();
		if length > total {
			return None;
		}

		// Include enough bytes past the starting position to test a match that crosses it.
		let limit = from.saturating_add(length - 1).min(total);
		let start = if backward {
			self.reverse.scan(text.bytes_at(limit).reversed()).map(|end| limit - end).or_else(|| {
				self.reverse.scan(text.bytes_at(total).reversed().take(total - from)).map(|end| total - end)
			})?
		} else {
			self
				.forward
				.scan(text.bytes_at(from))
				.map(|end| from + end - length)
				.or_else(|| self.forward.scan(text.bytes().take(limit)).map(|end| end - length))?
		};

		Some(start..start + length)
	}
}

#[derive(Clone, Debug)]
struct Matcher {
	bytes:    Vec<u8>,
	fallback: Vec<usize>,
}

impl Matcher {
	fn new(bytes: Vec<u8>) -> Self {
		let mut fallback = vec![0; bytes.len()];
		let mut matched = 0;
		for i in 1..bytes.len() {
			matched = advance(&bytes, &fallback, matched, bytes[i]);
			fallback[i] = matched;
		}
		Self { bytes, fallback }
	}

	fn scan(&self, input: impl Iterator<Item = u8>) -> Option<usize> {
		let mut matched = 0;
		for (i, byte) in input.enumerate() {
			matched = advance(&self.bytes, &self.fallback, matched, byte);
			if matched == self.bytes.len() {
				return Some(i + 1);
			}
		}
		None
	}
}

// Continue from the longest prefix that still matches instead of rereading the rope.
fn advance(pattern: &[u8], fallback: &[usize], mut matched: usize, byte: u8) -> usize {
	while matched > 0 && byte != pattern[matched] {
		matched = fallback[matched - 1];
	}
	matched + usize::from(byte == pattern[matched])
}
