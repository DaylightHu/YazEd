use std::{future::pending, sync::atomic::Ordering, time::{Duration, Instant}};

use anyhow::Result;
use tokio::{select, time::sleep_until};
use yazi_actor::act;
use yazi_core::{Core, Ctx};
use yazi_macro::{render, succ};
use yazi_shared::{data::Data, event::{Event, EventRx, NEED_RENDER}};
use yazi_term::event::Event as TermEvent;
use yazi_tui::Raterm;

use super::{SyncGuard, cursor_trail::CursorTrail};
use crate::Dispatcher;

pub(crate) struct App {
	pub(crate) core: Core,

	pub(super) need_render:     u8,
	pub(super) clear_on_render: bool,
	pub(crate) last_render:     Instant,
	pub(super) next_render:     Option<Instant>,
	pub(super) cursor_trail:    CursorTrail,
}

impl App {
	fn make(term: Raterm) -> Self {
		Self {
			core: Core::make(term),

			need_render:     0,
			clear_on_render: false,
			last_render:     Instant::now(),
			next_render:     None,
			cursor_trail:    CursorTrail::new(),
		}
	}

	pub(crate) async fn serve(mut rx: EventRx) -> Result<()> {
		let term = Raterm::start()?;

		let mut app = Self::make(term);
		app.bootstrap()?;

		loop {
			select! {
				_ = wait_until(app.next_render) => {
					app.render()?;
				}
				_ = wait_until(app.cursor_trail.deadline()) => {
					let guard = SyncGuard::enter();
					app.cursor_trail.draw(Instant::now());
					guard.finish(None);
				}
				result = app.drain(&mut rx) => if !result? {
					break;
				}
			}
		}
		app.cursor_trail.reset();
		Ok(())
	}

	fn bootstrap(&mut self) -> Result<Data> {
		let cx = &mut Ctx::active(&mut self.core);
		act!(app:bootstrap, cx)?;
		act!(app:reflow, cx, crate::Root::reflow as fn(_) -> _)?;
		succ!(render!())
	}

	async fn drain(&mut self, rx: &mut EventRx) -> Result<bool> {
		let Some(event) = rx.recv().await else {
			return Ok(false);
		};

		self.dispatch(event)?;
		while let Ok(e) = rx.try_recv() {
			self.dispatch(e)?;
		}

		Ok(true)
	}

	fn dispatch(&mut self, event: Event) -> Result<()> {
		if self.core.graphical_cursor && matches!(event, Event::Term(TermEvent::Resize(_))) {
			self.cursor_trail.reset();
			render!();
		}
		let viewing = self.core.active().view.is_some();
		let tab_bar = self.core.mgr.tabs.len() > 1;
		Dispatcher::new(self).dispatch(event);

		if self.core.term.is_none() {
			self.cursor_trail.reset();
		}
		let now_viewing = self.core.active().view.is_some();
		if viewing != now_viewing || (now_viewing && tab_bar != (self.core.mgr.tabs.len() > 1)) {
			let cx = &mut Ctx::active(&mut self.core);
			act!(app:resize, cx, crate::Root::reflow as fn(_) -> _)?;
		}
		if viewing != now_viewing {
			self.clear_on_render = true;
			render!();
		}

		self.need_render = NEED_RENDER.load(Ordering::Relaxed);
		if self.need_render == 0 {
			return Ok(());
		}

		let deadline = self.last_render + Duration::from_millis(10);
		if deadline > Instant::now() {
			self.next_render = Some(deadline);
		} else {
			self.render()?;
		}

		Ok(())
	}
}

async fn wait_until(deadline: Option<Instant>) {
	match deadline {
		Some(deadline) => sleep_until(deadline.into()).await,
		None => pending().await,
	}
}
