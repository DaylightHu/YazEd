use std::{fmt, path::Path, process::Stdio, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use tokio::{
	io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
	process::{Child, ChildStdin, ChildStdout, Command},
	sync::{Mutex, mpsc, watch},
	task::JoinHandle,
};
use url::Url;
use yazi_core::tab::{PositionEncoding, ViewSource};
use yazi_shared::id::Id;

use super::Server;

type Writer = Arc<Mutex<ChildStdin>>;

pub(super) struct Session {
	_child: Child,
	reader: JoinHandle<()>,
	writer: Writer,
	responses: mpsc::Receiver<Result<Value>>,
	status: watch::Receiver<Option<bool>>,
	rust_analyzer: bool,
	config: Server,
	next_id: u64,
	encoding: PositionEncoding,
	sync: SyncKind,
	open_close: bool,
	document: Option<Document>,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum SyncKind {
	#[default]
	None,
	Full,
	Incremental,
}

#[derive(Debug)]
pub(super) struct RpcError {
	code: i64,
	message: String,
}

impl fmt::Display for RpcError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.message)
	}
}

impl std::error::Error for RpcError {}

#[derive(Debug)]
pub(super) struct ResponseTimeout;

impl fmt::Display for ResponseTimeout {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("Waiting for the language server timed out; try the lookup again")
	}
}

impl std::error::Error for ResponseTimeout {}

struct Document {
	uri: String,
	identity: Id,
	revision: usize,
	version: u64,
	end: Value,
}

impl Drop for Session {
	fn drop(&mut self) {
		self.reader.abort();
	}
}

impl Session {
	pub(super) async fn start(root: &Path, config: Server) -> Result<Self> {
		let root_uri =
			Url::from_directory_path(root).map_err(|_| anyhow::anyhow!("Invalid workspace path"))?;
		let folders = json!([{ "uri": root_uri.as_str(), "name": root.file_name().unwrap_or_default().to_string_lossy() }]);
		let mut child = Command::new(&config.command[0])
			.args(&config.command[1..])
			.current_dir(root)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::null())
			.kill_on_drop(true)
			.spawn()
			.with_context(|| format!("Starting {}", config.command[0]))?;
		let writer = Arc::new(Mutex::new(child.stdin.take().unwrap()));
		let (send, responses) = mpsc::channel(8);
		let (set_status, status) = watch::channel(None);
		let reader = tokio::spawn(receive(
			child.stdout.take().unwrap(),
			writer.clone(),
			send,
			set_status,
			folders.clone(),
			config.settings.clone(),
		));
		let mut session = Self {
			_child: child,
			reader,
			writer,
			responses,
			status,
			rust_analyzer: false,
			config,
			next_id: 0,
			encoding: PositionEncoding::default(),
			sync: SyncKind::default(),
			open_close: false,
			document: None,
		};
		let result = session
			.request(
				"initialize",
				json!({
					"processId": std::process::id(), "clientInfo": { "name": "YazEd" },
					"rootUri": root_uri.as_str(), "workspaceFolders": folders,
					"initializationOptions": session.config.initialization_options,
					"capabilities": {
						"general": { "positionEncodings": ["utf-16", "utf-8", "utf-32"] },
						"experimental": { "serverStatusNotification": true },
						"textDocument": { "definition": { "linkSupport": true } },
						"workspace": { "configuration": true, "workspaceFolders": true },
					},
				}),
			)
			.await?;
		session.rust_analyzer =
			result.pointer("/serverInfo/name").and_then(Value::as_str) == Some("rust-analyzer");
		let caps = result.get("capabilities").context("Missing language server capabilities")?;
		ensure!(
			caps.get("definitionProvider").is_some_and(|v| v == &Value::Bool(true) || v.is_object()),
			"The language server does not support definition lookup"
		);
		let encoding =
			caps.get("positionEncoding").or_else(|| result.get("offsetEncoding")).and_then(Value::as_str);
		session.encoding = match encoding {
			Some("utf-8") => PositionEncoding::Utf8,
			Some("utf-32") => PositionEncoding::Utf32,
			Some("utf-16") | None => PositionEncoding::Utf16,
			Some(encoding) => bail!("Unsupported language server position encoding: {encoding}"),
		};
		if let Some(sync) = caps.get("textDocumentSync") {
			session.sync = match sync.as_u64().or_else(|| sync.get("change").and_then(Value::as_u64)) {
				Some(1) => SyncKind::Full,
				Some(2) => SyncKind::Incremental,
				_ => SyncKind::None,
			};
			session.open_close =
				sync.get("openClose").and_then(Value::as_bool).unwrap_or(session.sync != SyncKind::None);
		}
		session.notify("initialized", json!({})).await?;
		Ok(session)
	}

	pub(super) fn matches(&self, config: &Server) -> bool {
		!self.responses.is_closed()
			&& self.config.command == config.command
			&& self.config.settings == config.settings
			&& self.config.initialization_options == config.initialization_options
	}

	pub(super) async fn prepare_document(&mut self, path: &Path, source: &ViewSource) -> Result<String> {
		let uri = Url::from_file_path(path).map_err(|_| anyhow::anyhow!("Invalid source path"))?.to_string();
		self.sync_document(&uri, source).await?;
		Ok(uri)
	}

	pub(super) async fn definition(
		&mut self,
		path: &Path,
		source: &ViewSource,
	) -> Result<(Value, PositionEncoding)> {
		let uri = self.prepare_document(path, source).await?;
		let (line, character) = source.cursor_position(self.encoding);
		let params = json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": character } });
		let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
		loop {
			let status = *self.status.borrow();
			let result = self.request("textDocument/definition", params.clone()).await;
			let unfinished =
				result.as_ref().is_ok_and(|v| v.is_null() || v.as_array().is_some_and(Vec::is_empty))
					|| result
						.as_ref()
						.err()
						.and_then(|e| e.downcast_ref::<RpcError>())
						.is_some_and(|e| matches!(e.code, -32801 | -32802));
			if !unfinished
				|| status == Some(true)
				|| (!self.rust_analyzer && status.is_none())
				|| tokio::time::Instant::now() >= deadline
			{
				return Ok((result?, self.encoding));
			}
			// The requested symbol can be available before the rest of the workspace is ready.
			if matches!(
				tokio::time::timeout(
					Duration::from_millis(500),
					self.status.wait_for(|ready| *ready == Some(true))
				)
				.await,
				Ok(Err(_))
			) {
				return Ok((result?, self.encoding));
			}
		}
	}

	async fn sync_document(&mut self, uri: &str, source: &ViewSource) -> Result<()> {
		if !self.open_close && self.sync == SyncKind::None {
			return Ok(());
		}
		if self.document.as_ref().is_some_and(|doc| doc.uri != uri) {
			let doc = self.document.take().unwrap();
			if self.open_close {
				self.notify("textDocument/didClose", json!({ "textDocument": { "uri": doc.uri } })).await?;
			}
		}
		if let Some(doc) = &self.document
			&& doc.identity == source.identity
			&& doc.revision == source.revision
		{
			return Ok(());
		}
		let mut version = self.document.as_ref().map_or(1, |doc| doc.version + 1);
		let text = source.text.to_string();
		match &self.document {
			None if self.open_close => self.open(uri, version, &text).await?,
			Some(doc) if self.sync == SyncKind::None && self.open_close => {
				self.notify("textDocument/didClose", json!({ "textDocument": { "uri": doc.uri } })).await?;
				version = 1;
				self.open(uri, version, &text).await?;
			}
			previous => {
				let change = if self.sync == SyncKind::Incremental {
					let end = previous
						.as_ref()
						.context("The language server uses incremental changes without opening documents")?
						.end
						.clone();
					json!({ "range": { "start": { "line": 0, "character": 0 }, "end": end }, "text": text })
				} else {
					json!({ "text": text })
				};
				self
					.notify(
						"textDocument/didChange",
						json!({
							"textDocument": { "uri": uri, "version": version }, "contentChanges": [change],
						}),
					)
					.await?;
			}
		}
		let line = source.text.len_lines() - 1;
		let character: usize = source.text.line(line).chars().map(|c| self.encoding.units(c)).sum();
		self.document = Some(Document {
			uri: uri.into(),
			identity: source.identity,
			revision: source.revision,
			version,
			end: json!({ "line": line, "character": character }),
		});
		Ok(())
	}

	async fn open(&self, uri: &str, version: u64, text: &str) -> Result<()> {
		self
			.notify(
				"textDocument/didOpen",
				json!({ "textDocument": {
			"uri": uri, "languageId": self.config.language_id, "version": version, "text": text,
		} }),
			)
			.await
	}

	async fn notify(&self, method: &str, params: Value) -> Result<()> {
		write(&self.writer, json!({"jsonrpc": "2.0", "method": method, "params": params})).await
	}

	async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
		self.next_id += 1;
		let id = self.next_id;
		write(&self.writer, json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
			.await?;
		let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
		loop {
			let mut message = tokio::time::timeout_at(deadline, self.responses.recv())
				.await
				.map_err(|_| ResponseTimeout)?
				.context("Language server exited")??;
			if message.get("id").and_then(Value::as_u64) != Some(id) {
				continue;
			}
			if let Some(error) = message.get("error") {
				let code = error.get("code").and_then(Value::as_i64).unwrap_or_default();
				let message = error.get("message").and_then(Value::as_str).unwrap_or("Request failed");
				return Err(RpcError { code, message: format!("{method}: {message}") }.into());
			}
			return Ok(message["result"].take());
		}
	}
}

async fn receive(
	stdout: ChildStdout,
	writer: Writer,
	send: mpsc::Sender<Result<Value>>,
	set_status: watch::Sender<Option<bool>>,
	folders: Value,
	settings: Option<Value>,
) {
	let mut reader = BufReader::new(stdout);
	loop {
		let message = match read(&mut reader).await {
			Ok(message) => message,
			Err(e) => {
				let _ = send.send(Err(e)).await;
				break;
			}
		};
		if message.get("method").and_then(Value::as_str) == Some("experimental/serverStatus") {
			set_status.send_replace(message.pointer("/params/quiescent").and_then(Value::as_bool));
		}
		if let Some(id) = message.get("id") {
			if let Some(method) = message.get("method").and_then(Value::as_str) {
				let result = match method {
					"workspace/workspaceFolders" => Some(folders.clone()),
					"workspace/configuration" => Some(Value::Array(
						message
							.pointer("/params/items")
							.and_then(Value::as_array)
							.into_iter()
							.flatten()
							.map(|item| setting(settings.as_ref(), item.get("section").and_then(Value::as_str)))
							.collect(),
					)),
					"window/showMessageRequest" | "window/workDoneProgress/create" => Some(Value::Null),
					_ => None,
				};
				let reply = match result {
					Some(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
					None => {
						json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not supported" } })
					}
				};
				if let Err(e) = write(&writer, reply).await {
					let _ = send.send(Err(e)).await;
					break;
				}
			} else if send.send(Ok(message)).await.is_err() {
				break;
			}
		}
	}
}

fn setting(settings: Option<&Value>, section: Option<&str>) -> Value {
	let Some(value) = settings else { return Value::Null };
	let Some(section) = section.filter(|s| !s.is_empty()) else { return value.clone() };
	value
		.get(section)
		.or_else(|| section.split('.').try_fold(value, |v, name| v.get(name)))
		.cloned()
		.unwrap_or_default()
}

async fn read(reader: &mut BufReader<ChildStdout>) -> Result<Value> {
	let mut length = None;
	let mut header = String::new();
	loop {
		ensure!(
			(&mut *reader).take(8193).read_line(&mut header).await? != 0,
			"Language server closed its output"
		);
		ensure!(header.ends_with('\n') && header.len() <= 8192, "Invalid language server header");
		if header == "\r\n" || header == "\n" {
			break;
		}
		if let Some((name, value)) = header.split_once(':')
			&& name.eq_ignore_ascii_case("Content-Length")
		{
			length = Some(value.trim().parse::<usize>()?);
		}
		header.clear();
	}
	let length = length.context("Missing language server Content-Length")?;
	ensure!(length <= 64 * 1024 * 1024, "Language server message is too large");
	let mut bytes = vec![0; length];
	reader.read_exact(&mut bytes).await?;
	Ok(serde_json::from_slice(&bytes)?)
}

async fn write(writer: &Writer, message: Value) -> Result<()> {
	let bytes = serde_json::to_vec(&message)?;
	tokio::time::timeout(Duration::from_secs(5), async {
		let mut writer = writer.lock().await;
		writer.write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes()).await?;
		writer.write_all(&bytes).await?;
		writer.flush().await
	})
	.await
	.context("Writing to the language server timed out")??;
	Ok(())
}
