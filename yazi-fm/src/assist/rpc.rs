use std::{
	collections::HashMap,
	process::Stdio,
	sync::{
		Arc, Mutex,
		atomic::{AtomicU64, Ordering},
	},
	time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use tokio::{
	io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
	process::{Child, Command},
	sync::{mpsc, oneshot, watch},
	task::JoinHandle,
	time::timeout,
};

use super::config::Server;

type Responses = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

pub(super) struct Rpc {
	pub status: watch::Receiver<Option<(String, String)>>,
	status_send: watch::Sender<Option<(String, String)>>,
	child: Child,
	send: mpsc::Sender<Value>,
	responses: Responses,
	next: AtomicU64,
	tasks: Vec<JoinHandle<()>>,
	errors: Arc<Mutex<String>>,
}

impl Rpc {
	pub(super) fn start(config: &Server, root: &std::path::Path) -> Result<Self> {
		let mut child = Command::new(&config.command[0])
			.args(&config.command[1..])
			.current_dir(root)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.kill_on_drop(true)
			.spawn()
			.with_context(|| {
				format!("Starting {}. Install it or configure completion.toml", config.command[0])
			})?;
		let (send, mut outgoing) = mpsc::channel::<Value>(8);
		let (status_send, status) = watch::channel(None);
		let responses: Responses = Default::default();
		let errors: Arc<Mutex<String>> = Default::default();
		let mut stdin = child.stdin.take().unwrap();
		let writer = tokio::spawn(async move {
			while let Some(value) = outgoing.recv().await {
				let body = value.to_string();
				let header = format!("Content-Length: {}\r\n\r\n", body.len());
				if stdin.write_all(header.as_bytes()).await.is_err()
					|| stdin.write_all(body.as_bytes()).await.is_err()
					|| stdin.flush().await.is_err()
				{
					break;
				}
			}
		});
		let mut stderr = child.stderr.take().unwrap();
		let log = errors.clone();
		let error_reader = tokio::spawn(async move {
			let mut bytes = [0; 2048];
			while let Ok(n @ 1..) = stderr.read(&mut bytes).await {
				let mut log = log.lock().unwrap();
				if log.len() > 4096 {
					log.clear();
				}
				log.push_str(&String::from_utf8_lossy(&bytes[..n]));
			}
		});
		let stdout = child.stdout.take().unwrap();
		let file_status = status_send.clone();
		let (pending, tx, settings) = (responses.clone(), send.clone(), config.settings.clone());
		let reader = tokio::spawn(async move {
			let mut input = BufReader::new(stdout);
			while let Ok(value) = receive(&mut input).await {
				if value["method"] == "textDocument/clangd.fileStatus" {
					if let (Some(uri), Some(state)) =
						(value["params"]["uri"].as_str(), value["params"]["state"].as_str())
					{
						file_status.send_replace(Some((uri.into(), state.into())));
					}
				}
				if value.get("method").is_some() {
					if let Some(id) = value.get("id") {
						let result = match value["method"].as_str() {
							Some("workspace/configuration") => {
								let values: Vec<_> = value["params"]["items"]
									.as_array()
									.into_iter()
									.flatten()
									.map(|item| {
										configuration(settings.as_ref(), item["section"].as_str().unwrap_or(""))
									})
									.collect();
								json!(values)
							}
							Some("workspace/workspaceFolders") => Value::Null,
							Some("workspace/applyEdit") => json!({"applied": false}),
							_ => Value::Null,
						};
						if tx.send(json!({"jsonrpc": "2.0", "id": id, "result": result})).await.is_err() {
							break;
						}
					}
				} else if let Some(id) = value["id"].as_u64() {
					if let Some(reply) = pending.lock().unwrap().remove(&id) {
						let _ = reply.send(value);
					}
				}
			}
			pending.lock().unwrap().clear();
		});
		Ok(Self {
			status,
			status_send,
			child,
			send,
			responses,
			next: AtomicU64::new(1),
			tasks: vec![writer, reader, error_reader],
			errors,
		})
	}

	pub(super) async fn notify(&self, method: &str, params: Value) -> Result<()> {
		self
			.send
			.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
			.await
			.context("Completion server closed its input")
	}

	pub(super) async fn request(&self, method: &str, params: Value) -> Result<Value> {
		let id = self.next.fetch_add(1, Ordering::Relaxed);
		let (send, receive) = oneshot::channel();
		self.responses.lock().unwrap().insert(id, send);
		let _cancel = scopeguard::guard(id, |id| {
			if self.responses.lock().unwrap().remove(&id).is_some() {
				let _ = self
					.send
					.try_send(json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": id}}));
			}
		});
		self.send.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
		let response = timeout(Duration::from_secs(8), receive)
			.await
			.context("Completion server response timed out")?
			.with_context(|| {
				format!("Completion server disconnected. {}", self.errors.lock().unwrap().trim())
			})?;
		if let Some(error) = response.get("error") {
			bail!("{method}: {error}");
		}
		Ok(response.get("result").cloned().unwrap_or_default())
	}

	pub(super) fn alive(&mut self) -> bool {
		matches!(self.child.try_wait(), Ok(None)) && !self.tasks[1].is_finished()
	}

	pub(super) fn reset_status(&self) {
		self.status_send.send_replace(None);
	}
}

impl Drop for Rpc {
	fn drop(&mut self) {
		for task in &self.tasks {
			task.abort();
		}
		let _ = self.child.start_kill();
	}
}

async fn receive(input: &mut BufReader<tokio::process::ChildStdout>) -> Result<Value> {
	let mut length = None;
	let mut header_bytes = 0;
	loop {
		let mut header = Vec::new();
		let n = (&mut *input).take(8193).read_until(b'\n', &mut header).await?;
		ensure!(n > 0 && n <= 8192, "Invalid completion server header");
		header_bytes += n;
		ensure!(header_bytes <= 8192, "Completion server headers are too large");
		let header = std::str::from_utf8(&header)?.trim();
		if header.is_empty() {
			break;
		}
		if let Some((name, value)) = header.split_once(':') {
			if name.eq_ignore_ascii_case("Content-Length") {
				length = Some(value.trim().parse::<usize>()?);
			}
		}
	}
	let length = length.context("Missing Content-Length")?;
	ensure!(length <= 16 * 1024 * 1024, "Completion response exceeds 16 MiB");
	let mut bytes = vec![0; length];
	input.read_exact(&mut bytes).await?;
	Ok(serde_json::from_slice(&bytes)?)
}

fn configuration(settings: Option<&Value>, section: &str) -> Value {
	let Some(mut value) = settings else { return Value::Null };
	if section.is_empty() {
		return value.clone();
	}
	for part in section.split('.') {
		value = &value[part];
	}
	value.clone()
}
