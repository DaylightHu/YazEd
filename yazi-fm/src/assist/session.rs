use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use url::Url;
use yazi_core::tab::AssistSource;

use super::{
	Output,
	config::Server,
	model::{self, Encoding},
	rpc::Rpc,
};

pub(super) struct Session {
	pub rpc: Rpc,
	pub root: PathBuf,
	pub config: Server,
	encoding: Encoding,
	incremental: bool,
	completion: bool,
	signatures: bool,
	document: Option<(String, AssistSource, i64)>,
	needs_preamble: bool,
	completion_ready: bool,
}

impl Session {
	pub(super) async fn start(root: PathBuf, config: Server) -> Result<Self> {
		let rpc = Rpc::start(&config, &root)?;
		let uri = file_uri(&root)?;
		let result = rpc.request("initialize", json!({
			"processId": std::process::id(), "clientInfo": {"name": "YazEd"},
			"rootUri": uri, "workspaceFolders": [{"uri": uri, "name": root.file_name().unwrap_or_default().to_string_lossy()}],
			"initializationOptions": config.initialization_options,
			"capabilities": {
				"general": {"positionEncodings": ["utf-8", "utf-16", "utf-32"]},
				"workspace": {"configuration": true},
				"textDocument": {
					"completion": {"completionItem": {"snippetSupport": false, "insertReplaceSupport": true}},
					"signatureHelp": {"signatureInformation": {"parameterInformation": {"labelOffsetSupport": true}, "activeParameterSupport": true}}
				}
			}
		})).await?;
		let caps = &result["capabilities"];
		let encoding = match caps["positionEncoding"].as_str().unwrap_or("utf-16") {
			"utf-8" => Encoding::Utf8,
			"utf-32" => Encoding::Utf32,
			_ => Encoding::Utf16,
		};
		let sync = &caps["textDocumentSync"];
		let incremental = sync.as_u64().or_else(|| sync["change"].as_u64()) == Some(2);
		let completion = caps["completionProvider"].is_object() || caps["completionProvider"] == true;
		let signatures =
			caps["signatureHelpProvider"].is_object() || caps["signatureHelpProvider"] == true;
		rpc.notify("initialized", json!({})).await?;
		if let Some(settings) = &config.settings {
			rpc.notify("workspace/didChangeConfiguration", json!({"settings": settings})).await?;
		}
		Ok(Self {
			rpc,
			root,
			config,
			encoding,
			incremental,
			completion,
			signatures,
			document: None,
			needs_preamble: false,
			completion_ready: false,
		})
	}

	pub(super) async fn query(&mut self, source: AssistSource) -> Result<Output> {
		self.sync(&source).await?;
		let cursor = source.key.cursor;
		let completion = self.completion && model::wants_completion(&source);
		let call = self.signatures && model::in_call(&source);
		let uri = file_uri(&source.path)?;
		if self.needs_preamble {
			// The first clangd completion can otherwise omit all included-header symbols.
			super::publish(Output {
				key: source.key,
				items: Vec::new(),
				signature: None,
				status: Some("Completion · preparing headers".into()),
			});
			tokio::time::timeout(
				std::time::Duration::from_secs(8),
				self.rpc.status.wait_for(|status| {
					status.as_ref().is_some_and(|(file, state)| file == &uri && state == "idle")
				}),
			)
			.await
			.context("Preparing completion headers timed out")??;
			self.needs_preamble = false;
		}
		let params =
			json!({"textDocument": {"uri": uri}, "position": self.encoding.position(&source, cursor)});
		// Both requests can run concurrently on the server.
		let complete = async {
			if completion {
				let mut result = self.rpc.request("textDocument/completion", params.clone()).await?;
				// Header symbols can reach clangd's index just after the first AST becomes idle.
				for delay in [20, 40, 80, 160] {
					let empty =
						result.as_array().or_else(|| result["items"].as_array()).is_none_or(Vec::is_empty);
					if self.completion_ready || !empty {
						break;
					}
					tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
					result = self.rpc.request("textDocument/completion", params.clone()).await?;
				}
				Ok::<_, anyhow::Error>(result)
			} else {
				Ok(Value::Null)
			}
		};
		let help = async {
			if call {
				self.rpc.request("textDocument/signatureHelp", params.clone()).await
			} else {
				Ok(Value::Null)
			}
		};
		let (items, signature) = tokio::join!(complete, help);
		if completion {
			self.completion_ready = true;
		}
		let status = items
			.as_ref()
			.err()
			.or_else(|| signature.as_ref().err())
			.map(|error| model::clean(&format!("{error:#}"), 300));
		let items = items.unwrap_or_default();
		let signature = signature.unwrap_or_default();
		let encoding = self.encoding;
		tokio::task::spawn_blocking(move || {
			Ok(Output {
				key: source.key,
				items: model::items(items, &source, encoding),
				signature: model::signature(signature),
				status,
			})
		})
		.await?
	}

	async fn sync(&mut self, source: &AssistSource) -> Result<()> {
		let uri = file_uri(&source.path)?;
		if let Some((old_uri, old_source, _)) = &self.document {
			if old_uri != &uri || old_source.key.identity != source.key.identity {
				self.rpc.notify("textDocument/didClose", json!({"textDocument": {"uri": old_uri}})).await?;
				self.document = None;
			}
		}
		let Some((_, old, version)) = &self.document else {
			self.completion_ready = false;
			self.needs_preamble = self
				.config
				.initialization_options
				.as_ref()
				.is_some_and(|options| options["clangdFileStatus"] == true);
			self.rpc.reset_status();
			let text = source.text.to_string();
			self
				.rpc
				.notify(
					"textDocument/didOpen",
					json!({"textDocument": {
						"uri": uri, "languageId": self.config.language_id, "version": 1, "text": text
					}}),
				)
				.await?;
			self.document = Some((uri, source.clone(), 1));
			return Ok(());
		};
		if old.key.revision == source.key.revision {
			return Ok(());
		}
		let version = version + 1;
		let (old, new, incremental, encoding) =
			(old.clone(), source.clone(), self.incremental, self.encoding);
		let change = tokio::task::spawn_blocking(move || {
			if !incremental {
				return json!({"text": new.text.to_string()});
			}
			let before = old.text.to_string();
			let after = new.text.to_string();
			let mut start = before.bytes().zip(after.bytes()).take_while(|(a, b)| a == b).count();
			while !before.is_char_boundary(start) || !after.is_char_boundary(start) {
				start -= 1;
			}
			let mut suffix = before[start..]
				.bytes()
				.rev()
				.zip(after[start..].bytes().rev())
				.take_while(|(a, b)| a == b)
				.count();
			while !before.is_char_boundary(before.len() - suffix)
				|| !after.is_char_boundary(after.len() - suffix)
			{
				suffix -= 1;
			}
			let range = json!({"start": encoding.position(&old, old.text.byte_to_char(start)),
				"end": encoding.position(&old, old.text.byte_to_char(before.len() - suffix))});
			json!({"range": range, "text": &after[start..after.len() - suffix]})
		})
		.await?;
		self
			.rpc
			.notify(
				"textDocument/didChange",
				json!({"textDocument": {"uri": uri, "version": version}, "contentChanges": [change]}),
			)
			.await?;
		self.document = Some((uri, source.clone(), version));
		Ok(())
	}
}

fn file_uri(path: &Path) -> Result<String> {
	Url::from_file_path(path)
		.map(String::from)
		.ok()
		.context("Completion requires an absolute local path")
}
