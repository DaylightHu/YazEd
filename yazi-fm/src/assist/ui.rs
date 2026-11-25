use ratatui_core::{buffer::Buffer, layout::Rect, style::Modifier};
use yazi_config::THEME;
use yazi_core::tab::View;
use yazi_macro::render;
use yazi_term::event::{KeyCode, KeyEvent, Modifiers};

use super::Assist;

impl Assist {
	pub(crate) fn key(view: &mut View, key: &KeyEvent) -> bool {
		if !view.insert || view.saving {
			return false;
		}
		if key.code == KeyCode::Null
			|| (key.modifiers == Modifiers::CTRL && key.code == KeyCode::Char(' '))
		{
			view.assist_request();
			return true;
		}
		let Some(assist) = &mut view.assist else { return false };
		if key.modifiers.is_literal()
			&& key.code == KeyCode::Escape
			&& (!assist.items.is_empty() || assist.signature.is_some())
		{
			view.assist_dismiss();
			render!();
			return true;
		}
		if assist.items.is_empty() {
			return false;
		}
		let control = key.modifiers == Modifiers::CTRL;
		let next = (control && key.code == KeyCode::Char('n'))
			|| (key.modifiers.is_literal() && key.code == KeyCode::Down);
		let previous = (control && key.code == KeyCode::Char('p'))
			|| (key.modifiers.is_literal() && key.code == KeyCode::Up);
		if next || previous {
			assist.selected = if previous {
				(assist.selected + assist.items.len() - 1) % assist.items.len()
			} else {
				(assist.selected + 1) % assist.items.len()
			};
			render!();
			return true;
		}
		if key.modifiers.is_literal() && matches!(key.code, KeyCode::Tab | KeyCode::Enter) {
			let accepted = view.assist_accept();
			if accepted {
				view.reveal(
					yazi_config::LAYOUT.get().view.width as usize,
					yazi_config::LAYOUT.get().view.height.saturating_sub(1) as usize,
				);
			}
			render!();
			return true;
		}
		false
	}

	pub(crate) fn render(view: &View, area: Rect, buf: &mut Buffer) {
		if !view.focused || !view.insert || area.width < 4 || area.height < 3 {
			return;
		}
		let Some(assist) = &view.assist else { return };
		let Some(cursor) = view.screen_cursor(area) else { return };
		let normal = THEME.cmp.inactive.get().0;
		let active = THEME.cmp.active.get().0;
		let signature_style = THEME.status.progress_label.get().0;
		let bottom = area.bottom() - 1;
		let below = bottom.saturating_sub(cursor.y + 1);
		let above = cursor.y.saturating_sub(area.y);
		let down = below >= 5 || below >= above;
		let available = if down { below } else { above };
		let rows = (assist.items.len() as u16).min(8).min(available);
		let menu_width = assist
			.items
			.iter()
			.map(|i| i.label.chars().count() as u16 + 3)
			.max()
			.unwrap_or(0)
			.max(24)
			.min(72)
			.min(area.width);
		let menu_x = cursor.x.min(area.right().saturating_sub(menu_width));
		let menu_y = if down { cursor.y + 1 } else { cursor.y.saturating_sub(rows) };
		let start = assist.selected.saturating_sub(rows.saturating_sub(1) as usize);
		for (row, item) in assist.items.iter().enumerate().skip(start).take(rows as usize) {
			let y = menu_y + (row - start) as u16;
			let style = if row == assist.selected { active } else { normal };
			buf.set_stringn(menu_x, y, " ".repeat(menu_width as usize), menu_width as usize, style);
			buf.set_stringn(menu_x + 1, y, &item.label, menu_width.saturating_sub(2) as usize, style);
		}
		if let Some(signature) = &assist.signature {
			let y = if down {
				cursor.y.checked_sub(1).filter(|y| *y >= area.y)
			} else {
				(cursor.y + 1 < bottom).then_some(cursor.y + 1)
			};
			if let Some(y) = y {
				let width = area.width.min(100);
				let x = cursor.x.min(area.right().saturating_sub(width));
				buf.set_stringn(x, y, " ".repeat(width as usize), width as usize, normal);
				buf.set_stringn(x + 1, y, &signature.label, width.saturating_sub(2) as usize, normal);
				if let Some(parameter) = &signature.parameter {
					// Ask the buffer for display width so wide Unicode characters align correctly.
					let prefix = &signature.label[..parameter.start];
					let parameter = &signature.label[parameter.clone()];
					let (px, _) = buf.set_stringn(x + 1, y, prefix, width.saturating_sub(2) as usize, normal);
					buf.set_stringn(
						px,
						y,
						parameter,
						(x + width - 1).saturating_sub(px) as usize,
						signature_style.add_modifier(Modifier::BOLD),
					);
				}
			}
		}
	}
}
