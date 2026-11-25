use anyhow::Result;
use ratatui_core::{
	buffer::Buffer,
	layout::Rect,
	style::{Modifier, Style},
};
use yazi_config::THEME;
use yazi_core::{
	Ctx,
	tab::{DefinitionTarget, View as DocumentView, ViewDefinition, ViewReturn},
};
use yazi_macro::{emit, impl_data_any, relay, render, succ};
use yazi_scheduler::NotifyProxy;
use yazi_shared::{Layer, data::Data, event::ActionCow, id::Id, url::UrlLike};
use yazi_term::event::{KeyCode, KeyEvent};

use crate::lsp::Lsp;

use super::View;

#[derive(Clone)]
struct Lookup {
	request: Id,
	identity: Id,
	revision: usize,
	cursor: usize,
	result: Result<Vec<DefinitionTarget>, String>,
}

#[derive(Clone)]
struct Opened {
	request: Id,
	view: Option<DocumentView>,
}

impl_data_any!(Lookup);
impl_data_any!(Opened);

impl View {
	pub(super) fn definition(cx: &mut Ctx, lsp: &mut Option<Lsp>) -> Result<Data> {
		let tab = cx.tab_mut();
		let Some(view) = tab
			.view
			.as_mut()
			.filter(|v| v.focused && !v.insert && !v.saving && v.visual_mode().is_none())
		else {
			succ!()
		};
		let request = Id::unique();
		let source = view.definition_source();
		view.definition_request = Some(request);
		let scope = tab.scope.clone();
		let lsp = lsp.get_or_insert_with(Lsp::default).clone();
		tokio::spawn(async move {
			let (identity, revision, cursor) = (source.identity, source.revision, source.cursor);
			let result = lsp.definition(source).await.map_err(|error| format!("{error:#}"));
			if !scope.is_cancelled() {
				let lookup = Lookup { request, identity, revision, cursor, result };
				emit!(Call(relay!(view:definition_ready).with_scope(scope).with_any("lookup", lookup)));
			}
		});
		succ!(render!());
	}

	pub(super) fn definition_ready(cx: &mut Ctx, mut action: ActionCow) -> Result<Data> {
		let Some(lookup): Option<Lookup> = action.take_any("lookup") else { succ!() };
		let active = cx.tab == cx.mgr.tabs.cursor;
		let editing = cx.core.layer() == Layer::View;
		let Some(view) = &mut cx.tab_mut().view else { succ!() };
		if view.definition_request != Some(lookup.request) {
			succ!();
		}
		view.definition_request = None;
		if !active
			|| !editing
			|| !view.focused
			|| view.insert
			|| view.saving
			|| view.visual_mode().is_some()
			|| !view.at_definition_origin(lookup.identity, lookup.revision, lookup.cursor)
		{
			succ!(render!(active));
		}
		match lookup.result {
			Ok(targets) if targets.is_empty() => {
				NotifyProxy::push_warn("No definition", "The language server returned no location.")
			}
			Ok(targets) => {
				view.definition = Some(Box::new(ViewDefinition {
					request: lookup.request,
					targets,
					selected: 0,
					opening: false,
				}))
			}
			Err(error) => NotifyProxy::push_warn("Definition lookup failed", error),
		}
		succ!(render!());
	}

	pub(crate) fn definition_key(cx: &mut Ctx, key: &KeyEvent) {
		if !key.modifiers.is_literal() {
			return;
		}
		let tab = cx.tab_mut();
		let Some(view) = &mut tab.view else { return };
		if key.code == KeyCode::Escape {
			view.definition = None;
			render!();
			return;
		}
		let Some(preview) = view.definition.as_mut().filter(|preview| !preview.opening) else { return };
		match key.code {
			KeyCode::Down | KeyCode::Char('j') => {
				preview.selected = (preview.selected + 1).min(preview.targets.len() - 1);
			}
			KeyCode::Up | KeyCode::Char('k') => preview.selected = preview.selected.saturating_sub(1),
			KeyCode::Enter => {
				let target = &preview.targets[preview.selected];
				if target.url == view.url {
					let target = target.clone();
					view.definition = None;
					let position = view.definition_position(&target);
					tab.view_returns.push(ViewReturn::Position(position));
					Self::redraw(view, true);
					return;
				}
				if tab
					.view_returns
					.iter()
					.any(|entry| matches!(entry, ViewReturn::Document(view) if view.url == target.url))
				{
					NotifyProxy::push_warn(
						"Definition already open",
						"Close this file with :q to return to the earlier buffer.",
					);
					return;
				}
				preview.opening = true;
				let target = target.clone();
				let request = preview.request;
				let scope = tab.scope.clone();
				tokio::spawn(async move {
					let worker_scope = scope.clone();
					let view = tokio::task::spawn_blocking(move || {
						let mut view = Self::load(target.url.clone(), &worker_scope)?;
						view.definition_position(&target);
						Some(view)
					})
					.await
					.ok()
					.flatten();
					if !scope.is_cancelled() {
						emit!(Call(
							relay!(view:definition_opened)
								.with_scope(scope)
								.with_any("opened", Opened { request, view })
						));
					}
				});
			}
			_ => return,
		}
		render!();
	}

	pub(super) fn definition_opened(cx: &mut Ctx, mut action: ActionCow) -> Result<Data> {
		let Some(opened): Option<Opened> = action.take_any("opened") else { succ!() };
		let tab = cx.tab_mut();
		let Some(view) = &mut tab.view else { succ!() };
		let Some(preview) =
			view.definition.as_mut().filter(|p| p.request == opened.request && p.opening)
		else {
			succ!()
		};
		if let Some(mut target) = opened.view {
			view.definition = None;
			let previous = std::mem::replace(view, {
				let area = yazi_config::LAYOUT.get().view;
				target.reveal(area.width as usize, area.height.saturating_sub(1) as usize);
				target
			});
			tab.view_returns.push(ViewReturn::Document(Box::new(previous)));
		} else {
			preview.opening = false;
			NotifyProxy::push_warn(
				"Cannot open definition",
				"The source file could not be loaded as text.",
			);
		}
		succ!(render!());
	}

	pub(super) fn render_definition(preview: &ViewDefinition, area: Rect, buf: &mut Buffer) {
		for y in area.top()..area.bottom() {
			for x in area.left()..area.right() {
				buf[(x, y)].reset();
			}
		}
		let target = &preview.targets[preview.selected];
		let title = format!(
			" Definition {}/{}  {}:{}",
			preview.selected + 1,
			preview.targets.len(),
			target.url.name().map(|name| name.to_string_lossy()).unwrap_or_default(),
			target.line.saturating_add(1)
		);
		let selected = THEME.input.selected.get().0;
		let muted = Style::default().add_modifier(Modifier::DIM);
		let width = usize::from(area.width);
		buf.set_stringn(area.x, area.y, title, width, THEME.mode.normal_main.get().0);
		let mut body = area.y.saturating_add(1)..area.bottom().saturating_sub(1);
		if let Some(row) = body.next() {
			buf.set_stringn(area.x, row, format!(" {}", target.url.display()), width, muted);
		}
		if preview.targets.len() > 1 && body.len() >= 7 {
			let count = preview.targets.len().min(3);
			let first = preview.selected.saturating_sub(1).min(preview.targets.len() - count);
			for (offset, candidate) in preview.targets[first..first + count].iter().enumerate() {
				let active = first + offset == preview.selected;
				let marker = if active { ">" } else { " " };
				let style = if active { selected } else { muted };
				let name = candidate.url.name().map(|name| name.to_string_lossy()).unwrap_or_default();
				let label = format!(" {marker} {name}:{}", candidate.line.saturating_add(1));
				buf.set_stringn(area.x, body.next().unwrap(), label, width, style);
			}
		}
		for (row, (index, line)) in body.clone().zip(target.lines.iter().enumerate()) {
			let index = target.first_line + index;
			let style = if index == target.line { selected } else { Style::default() };
			buf.set_stringn(
				area.x,
				row,
				format!(" {:>5}  {line}", index.saturating_add(1)),
				width,
				style,
			);
		}
		if target.lines.is_empty() && !body.is_empty() {
			buf.set_stringn(area.x, body.start, " Source preview unavailable", width, muted);
		}
		if area.height > 1 {
			let footer = if preview.opening {
				" Opening definition…  Esc cancel"
			} else if preview.targets.len() > 1 {
				" j/k select  Enter open  Esc close"
			} else {
				" Enter open  Esc close"
			};
			buf.set_stringn(area.x, area.bottom() - 1, footer, width, muted);
		}
	}
}
