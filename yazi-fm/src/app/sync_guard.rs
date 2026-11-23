use std::{io::Write, sync::atomic::{AtomicU8, Ordering}};

use ratatui_core::layout::Position;
use yazi_macro::writef;
use yazi_tty::{TTY, sequence::{BeginSyncUpdate, EndSyncUpdate, HideCursor, MoveTo, SetCursorStyle, ShowCursor}};

static DEPTH: AtomicU8 = AtomicU8::new(0);

pub(super) struct SyncGuard {
	finished: bool,
}

pub(super) enum FrameCursor {
	Hidden,
	Native(Position, SetCursorStyle),
	Graphical(Position),
}

impl From<Option<(Position, SetCursorStyle)>> for FrameCursor {
	fn from(cursor: Option<(Position, SetCursorStyle)>) -> Self {
		match cursor {
			Some((position, style)) => Self::Native(position, style),
			None => Self::Hidden,
		}
	}
}

impl SyncGuard {
	pub(super) fn enter() -> Self {
		if DEPTH.fetch_add(1, Ordering::Relaxed) == 0 {
			write!(TTY.writer(), "{BeginSyncUpdate}").ok();
		}

		Self { finished: false }
	}

	pub(super) fn finish(mut self, cursor: FrameCursor) {
		self.finished = true;
		if DEPTH.fetch_sub(1, Ordering::Relaxed) != 1 {
			return;
		}

		_ = match cursor {
			FrameCursor::Native(Position { x, y }, style) => {
				writef!(TTY.writer(), "{style}{}{ShowCursor}{EndSyncUpdate}", MoveTo(x, y))
			}
			FrameCursor::Graphical(Position { x, y }) => {
				// Kitty's native trail can still fade after hiding the caret. Keep its
				// target at the rendered cursor instead of the last cell the TUI painted.
				writef!(TTY.writer(), "{HideCursor}{}{EndSyncUpdate}", MoveTo(x, y))
			}
			FrameCursor::Hidden => writef!(TTY.writer(), "{EndSyncUpdate}"),
		};
	}
}

impl Drop for SyncGuard {
	fn drop(&mut self) {
		if self.finished {
			return;
		}

		if DEPTH.fetch_sub(1, Ordering::Relaxed) == 1 {
			writef!(TTY.writer(), "{EndSyncUpdate}").ok();
		}
	}
}
