use std::{
	fs,
	path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;
use yazi_fs::Xdg;

#[derive(Default, Deserialize)]
struct Config {
	#[serde(default)]
	server: Vec<Server>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct Server {
	pub extensions: Vec<String>,
	pub language_id: String,
	pub command: Vec<String>,
	#[serde(default)]
	pub root_markers: Vec<String>,
	pub initialization_options: Option<Value>,
	pub settings: Option<Value>,
}

impl Server {
	pub(super) fn discover(path: &Path) -> Result<Option<(PathBuf, Self)>> {
		let config_path = Xdg::config_dir().join("completion.toml");
		let config: Config = match fs::read_to_string(&config_path) {
			Ok(text) => {
				toml::from_str(&text).with_context(|| format!("Reading {}", config_path.display()))?
			}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
			Err(error) => return Err(error.into()),
		};
		let extension = path.extension().and_then(|s| s.to_str()).unwrap_or_default();
		let server = config.server.into_iter().find(|s| s.extensions.iter().any(|e| e == extension));
		let server = server.or_else(|| {
			let language_id = match extension {
				"c" | "h" => "c",
				"cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
				_ => return None,
			};
			Some(Self {
				extensions: Vec::new(),
				language_id: language_id.into(),
				command: vec![
					"clangd".into(),
					"--background-index=0".into(),
					"--clang-tidy=0".into(),
					"--header-insertion=never".into(),
					"--limit-results=80".into(),
					"-j=2".into(),
					"--pch-storage=memory".into(),
					"--log=error".into(),
				],
				root_markers: vec![
					"compile_commands.json".into(),
					"compile_flags.txt".into(),
					".git".into(),
				],
				initialization_options: Some(serde_json::json!({"clangdFileStatus": true})),
				settings: None,
			})
		});
		let Some(server) = server else { return Ok(None) };
		ensure!(
			server.command.first().is_some_and(|s| !s.is_empty()),
			"Empty completion server command"
		);
		ensure!(!server.language_id.is_empty(), "Empty completion language_id");
		let parent = path.parent().context("File has no parent directory")?;
		let root = server
			.root_markers
			.iter()
			.find_map(|marker| parent.ancestors().find(|dir| dir.join(marker).exists()))
			.unwrap_or(parent)
			.to_owned();
		Ok(Some((root, server)))
	}
}
