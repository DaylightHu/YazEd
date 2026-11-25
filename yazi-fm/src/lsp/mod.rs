use std::{
	collections::HashMap,
	fs,
	io::{BufRead, BufReader},
	path::{Path, PathBuf},
	sync::{Arc, Mutex},
	time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;
use tokio::time::timeout;
use url::Url;
use yazi_core::tab::{DefinitionTarget, PositionEncoding, ViewSource};
use yazi_fs::Xdg;
use yazi_shared::url::UrlLike;

mod session;

use session::{ResponseTimeout, RpcError, Session};

type Sessions = HashMap<(PathBuf, String), Arc<tokio::sync::Mutex<Option<Session>>>>;

#[derive(Clone, Default)]
pub(super) struct Lsp {
	sessions: Arc<Mutex<Sessions>>,
}

#[derive(Default, Deserialize)]
struct Config {
	#[serde(default)]
	server: Vec<Server>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Server {
	extensions: Vec<String>,
	language_id: String,
	command: Vec<String>,
	#[serde(default)]
	root_markers: Vec<String>,
	root: Option<PathBuf>,
	initialization_options: Option<Value>,
	settings: Option<Value>,
}

impl Lsp {
	pub(super) async fn definition(&self, source: ViewSource) -> Result<Vec<DefinitionTarget>> {
		let path = source.url.as_local().context("Definitions require a local file")?.to_owned();
		let (path, root, config) = tokio::task::spawn_blocking(move || {
			let path = fs::canonicalize(&path).unwrap_or(path);
			let (root, config) = Server::discover(&path)?;
			Ok::<_, anyhow::Error>((path, root, config))
		})
		.await??;

		let key = (root.clone(), config.language_id.clone());
		let slot = self.sessions.lock().unwrap().entry(key).or_default().clone();
		let response = {
			let mut slot = slot.lock().await;
			if slot.as_ref().is_some_and(|s| !s.matches(&config)) {
				*slot = None;
			}
			if slot.is_none() {
				*slot = Some(
					timeout(Duration::from_secs(25), Session::start(&root, config))
						.await
						.context("Starting the language server timed out")??,
				);
			}
			match slot.as_mut().unwrap().definition(&path, &source).await {
				Ok(response) => response,
				Err(error) => {
					if !error.is::<RpcError>() && !error.is::<ResponseTimeout>() {
						*slot = None;
					}
					return Err(error);
				}
			}
		};

		tokio::task::spawn_blocking(move || make_targets(response.0, response.1, &path, &source))
			.await?
	}
}

impl Server {
	fn discover(path: &Path) -> Result<(PathBuf, Self)> {
		let file = Xdg::config_dir().join("lsp.toml");
		let config: Config = match fs::read_to_string(&file) {
			Ok(text) => toml::from_str(&text).with_context(|| format!("Reading {}", file.display()))?,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
			Err(e) => return Err(e).with_context(|| format!("Reading {}", file.display())),
		};
		let extension = path
			.extension()
			.and_then(|s| s.to_str())
			.context("This file has no language server mapping")?;
		let server = config
			.server
			.into_iter()
			.filter(|s| s.extensions.iter().any(|ext| ext == extension))
			.map(|mut server| {
				if let Some(root) = &mut server.root
					&& root.is_absolute()
					&& let Ok(canonical) = fs::canonicalize(&root)
				{
					*root = canonical;
				}
				server
			})
			.filter(|s| s.root.as_ref().is_none_or(|root| root.is_absolute() && path.starts_with(root)))
			.max_by_key(|s| s.root.as_ref().map_or(0, |root| root.components().count()));
		let server = match server {
			Some(server) => server,
			None if extension == "rs" => Self {
				extensions: vec!["rs".into()],
				language_id: "rust".into(),
				command: vec!["rust-analyzer".into()],
				root_markers: vec!["Cargo.lock".into(), "Cargo.toml".into(), ".git".into()],
				root: None,
				initialization_options: Some(serde_json::json!({
					"checkOnSave": false,
					"cachePriming": { "enable": false },
					"cargo": { "buildScripts": { "enable": false } },
					"procMacro": { "enable": false },
				})),
				settings: None,
			},
			None => bail!("Configure a language server for .{extension} in {}", file.display()),
		};
		ensure!(
			server.command.first().is_some_and(|s| !s.is_empty()),
			"Language server command is empty"
		);
		ensure!(!server.language_id.is_empty(), "Language server language_id is empty");
		let parent = path.parent().context("The file has no parent directory")?;
		let root = server
			.root
			.clone()
			.or_else(|| {
				let markers = if server.root_markers.is_empty() {
					&[".git".to_owned()][..]
				} else {
					&server.root_markers[..]
				};
				markers
					.iter()
					.find_map(|marker| parent.ancestors().find(|dir| dir.join(marker).exists()))
					.map(Path::to_owned)
			})
			.unwrap_or_else(|| parent.to_owned());
		Ok((root, server))
	}
}

fn make_targets(
	value: Value,
	encoding: PositionEncoding,
	path: &Path,
	source: &ViewSource,
) -> Result<Vec<DefinitionTarget>> {
	let locations = match &value {
		Value::Null => return Ok(Vec::new()),
		Value::Array(items) => items.as_slice(),
		_ => std::slice::from_ref(&value),
	};
	let mut targets: Vec<DefinitionTarget> = Vec::new();
	for location in locations {
		let uri = location.get("targetUri").or_else(|| location.get("uri")).and_then(Value::as_str);
		let range = location.get("targetSelectionRange").or_else(|| location.get("range"));
		let Some(target_path) =
			uri.and_then(|s| Url::parse(s).ok()).and_then(|u| u.to_file_path().ok())
		else {
			continue;
		};
		let (Some(line), Some(column)) = (
			range.and_then(|r| r.pointer("/start/line")).and_then(Value::as_u64),
			range.and_then(|r| r.pointer("/start/character")).and_then(Value::as_u64),
		) else {
			continue;
		};
		let (Ok(line), Ok(column)) = (usize::try_from(line), usize::try_from(column)) else { continue };
		let same_file =
			target_path == path || fs::canonicalize(&target_path).is_ok_and(|target| target == path);
		let url = if same_file { source.url.clone() } else { target_path.as_path().into() };
		if targets.iter().any(|t| t.url == url && t.line == line && t.column == column) {
			continue;
		}
		let first_line = line.saturating_sub(2);
		let lines = if same_file {
			(0..10)
				.map_while(|offset| first_line.checked_add(offset).and_then(|index| source.line(index)))
				.map(|line| snippet(line.chars()))
				.collect()
		} else {
			read_snippet(&target_path, first_line).unwrap_or_default()
		};
		targets.push(DefinitionTarget { url, line, column, encoding, first_line, lines });
	}
	if !locations.is_empty() && targets.is_empty() {
		bail!("The language server did not return a local source location");
	}
	Ok(targets)
}

fn read_snippet(path: &Path, first_line: usize) -> Result<Vec<String>> {
	let file = crate::View::open_text_file(path)?;
	let mut reader = BufReader::new(file);
	for _ in 0..first_line {
		if reader.skip_until(b'\n')? == 0 {
			return Ok(Vec::new());
		}
	}
	let mut lines = Vec::new();
	let mut line = Vec::new();
	for _ in 0..10 {
		line.clear();
		if reader.read_until(b'\n', &mut line)? == 0 {
			break;
		}
		lines.push(snippet(String::from_utf8_lossy(&line).chars()));
	}
	Ok(lines)
}

fn snippet(chars: impl Iterator<Item = char>) -> String {
	let mut text = String::new();
	for ch in chars.take(512).take_while(|&c| c != '\r' && c != '\n') {
		if ch == '\t' {
			text.push_str("    ");
		} else if !ch.is_control() {
			text.push(ch);
		}
	}
	text
}
