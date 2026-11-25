use std::{path::PathBuf, time::Duration};

use tokio::{
	sync::watch,
	task::JoinHandle,
	time::{sleep, timeout},
};
use yazi_core::{
	Core,
	tab::{AssistItem, AssistKey, AssistSignature, AssistSource},
};
use yazi_macro::{emit, impl_data_any, relay};

mod config;
mod model;
mod rpc;
mod session;
mod ui;

use config::Server;
use session::Session;

#[derive(Clone)]
pub(super) struct Output {
	pub key: AssistKey,
	pub items: Vec<AssistItem>,
	pub signature: Option<AssistSignature>,
	pub status: Option<String>,
}

impl_data_any!(Output);

pub(crate) struct Assist {
	enabled: bool,
	worker: Option<(watch::Sender<Option<AssistSource>>, JoinHandle<()>)>,
	last: Option<AssistKey>,
}

impl Assist {
	pub(crate) fn new() -> Self {
		Self {
			enabled: std::env::var_os("YAZED_COMPLETION").is_none_or(|v| v != "0"),
			worker: None,
			last: None,
		}
	}

	pub(crate) fn sync(&mut self, core: &mut Core) {
		if !self.enabled {
			return;
		}
		let mut source = core.active_mut().view.as_mut().and_then(|view| view.assist_source());
		let key = source.as_ref().map(|s| s.key);
		if key == self.last {
			return;
		}
		if let Some(source) = &mut source {
			source.manual = self.last.is_some_and(|old| {
				old.identity == source.key.identity && old.request != source.key.request
			});
		}
		self.last = key;
		if let Some(view) = core.active_mut().view.as_mut() {
			view.assist_clear();
		}
		if source.is_some() && self.worker.is_none() {
			let (send, receive) = watch::channel(None);
			self.worker = Some((send, tokio::spawn(run(receive))));
		}
		if let Some((send, _)) = &self.worker {
			send.send_replace(source);
		}
	}
}

impl Drop for Assist {
	fn drop(&mut self) {
		if let Some((_, task)) = &self.worker {
			task.abort();
		}
	}
}

async fn run(mut receive: watch::Receiver<Option<AssistSource>>) {
	let mut session: Option<Session> = None;
	let mut path = PathBuf::new();
	let mut discovered = None;
	let mut failed_request = None;
	let mut pending = false;
	loop {
		if !pending {
			match timeout(Duration::from_secs(60), receive.changed()).await {
				Ok(Ok(())) => {}
				Ok(Err(_)) => break,
				Err(_) => {
					session = None;
					continue;
				}
			}
		}
		pending = false;
		// The watch channel holds one snapshot, so rapid typing cannot grow a queue.
		loop {
			if receive.borrow().is_none() {
				break;
			}
			tokio::select! {
				_ = sleep(Duration::from_millis(120)) => break,
				changed = receive.changed() => if changed.is_err() { return; },
			}
		}
		let Some(mut source) = receive.borrow_and_update().clone() else { continue };
		if source.path != path {
			path = source.path.clone();
			failed_request = None;
			let file = path.clone();
			match tokio::task::spawn_blocking(move || Server::discover(&file)).await {
				Ok(Ok(config)) => discovered = config,
				result => {
					publish(error(&source, format!("Completion configuration: {result:?}")));
					discovered = None;
				}
			}
		}
		let Some((root, config)) = &discovered else { continue };
		if failed_request == Some(source.key.request) {
			continue;
		}
		if session.as_mut().is_some_and(|s| s.root != *root || s.config != *config || !s.rpc.alive()) {
			session = None;
		}
		if session.is_none() {
			publish(Output {
				key: source.key,
				items: Vec::new(),
				signature: None,
				status: Some(format!("Completion · starting {}", config.command[0])),
			});
			match Session::start(root.clone(), config.clone()).await {
				Ok(ready) => session = Some(ready),
				Err(failure) => {
					publish(error(&source, format!("{failure:#}")));
					failed_request = Some(source.key.request);
					continue;
				}
			}
			// Initialization finishes once while the latest text continues replacing the snapshot.
			let latest = receive.borrow_and_update().clone();
			if let Some(latest) = latest.filter(|s| s.path == source.path) {
				source = latest;
			} else {
				pending = true;
				continue;
			}
		}
		let ready = session.as_mut().unwrap();
		tokio::select! {
			changed = receive.changed() => {
				if changed.is_err() { break; }
				pending = true;
			}
			result = ready.query(source.clone()) => {
				match result {
					Ok(output) => publish(output),
					Err(failure) => {
						publish(error(&source, format!("{failure:#}")));
						if !ready.rpc.alive() {
							session = None;
							failed_request = Some(source.key.request);
						}
					}
				}
			}
		}
	}
}

fn publish(output: Output) {
	emit!(Call(relay!(app:view_assist_ready).with_any("output", output)));
}

fn error(source: &AssistSource, status: String) -> Output {
	Output {
		key: source.key,
		items: Vec::new(),
		signature: None,
		status: Some(model::clean(&status, 300)),
	}
}
