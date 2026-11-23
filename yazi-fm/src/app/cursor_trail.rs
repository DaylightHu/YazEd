use std::{env, io::Write, time::{Duration, Instant}};

use base64::{Engine, engine::general_purpose::STANDARD};
use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
use ratatui_core::layout::{Position, Rect};
use yazi_config::LAYOUT;
use yazi_core::Core;
use yazi_emulator::{Brand, Dimension, EMULATOR};
use yazi_shared::{Layer, id::Id};
use yazi_tty::{TTY, sequence::{MoveTo, RestoreCursorPos, SaveCursorPos, SetCursorStyle}};

const DURATION: Duration = Duration::from_millis(120);
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const OPACITY: [u8; 8] = [18, 28, 40, 54, 72, 90, 110, 132];

#[derive(Clone, Copy, PartialEq, Eq)]
struct Scene {
	tab:    Id,
	files:  Rect,
	editor: Rect,
	cell:   (u32, u32),
}

impl Scene {
	fn target(self, x: u16, y: u16, width: u32) -> PixelRect {
		PixelRect { x: u32::from(x) * self.cell.0, y: u32::from(y) * self.cell.1, width }
	}
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PixelRect {
	x:     u32,
	y:     u32,
	width: u32,
}

struct Motion {
	from:    PixelRect,
	to:      PixelRect,
	started: Instant,
}

impl Motion {
	fn progress(&self, now: Instant) -> f32 {
		(now.duration_since(self.started).as_secs_f32() / DURATION.as_secs_f32()).min(1.0)
	}

	fn rect(&self, progress: f32) -> PixelRect {
		let eased = 1.0 - (1.0 - progress).powi(3);
		let interpolate =
			|a: u32, b: u32| ((b as f32 - a as f32).mul_add(eased, a as f32)).round() as u32;
		PixelRect {
			x:     interpolate(self.from.x, self.to.x),
			y:     interpolate(self.from.y, self.to.y),
			width: interpolate(self.from.width, self.to.width),
		}
	}
}

pub(super) struct CursorTrail {
	enabled:  bool,
	image_id: u32,
	target:   Option<(Scene, PixelRect)>,
	motion:   Option<Motion>,
	deadline: Option<Instant>,
	texture:  Option<((u32, u32), Vec<u8>)>,
	uploaded: bool,
}

impl CursorTrail {
	pub(super) fn new() -> Self {
		Self {
			enabled:  env::var_os("YAZED_CURSOR_TRAIL").is_some_and(|v| v == "1"),
			// The preview adapter uses the low 24 bits of the process ID.
			image_id: std::process::id() | 0x8000_0000,
			target:   None,
			motion:   None,
			deadline: None,
			texture:  None,
			uploaded: false,
		}
	}

	pub(super) fn deadline(&self) -> Option<Instant> { self.deadline }

	pub(super) fn reset(&mut self) {
		self.target = None;
		self.stop();
	}

	pub(super) fn track(
		&mut self,
		core: &Core,
		cursor: Option<(Position, SetCursorStyle)>,
		now: Instant,
	) {
		if !self.enabled {
			return;
		}

		let next = Self::focus(core, cursor);
		let prev = std::mem::replace(&mut self.target, next);
		match (prev, next) {
			(Some((old_scene, old)), Some((scene, target))) if old_scene == scene => {
				if old == target {
					return;
				}
				let from = self.motion.as_ref().map_or(old, |motion| motion.rect(motion.progress(now)));
				self.motion = Some(Motion { from, to: target, started: now });
				self.draw(now);
			}
			_ => self.stop(),
		}
	}

	fn focus(core: &Core, cursor: Option<(Position, SetCursorStyle)>) -> Option<(Scene, PixelRect)> {
		if EMULATOR.brand.get() != Brand::Kitty || !EMULATOR.kgp.get() || EMULATOR.mux.get().is_some() {
			return None;
		}

		let tab = core.active();
		tab.view.as_ref()?;
		let layout = LAYOUT.get();
		if layout.current.is_empty() || layout.view.is_empty() {
			return None;
		}

		let (w, h) = Dimension::cell_size()?;
		let (w, h) = (w.floor() as u32, h.floor() as u32);
		if w == 0 || h == 0 {
			return None;
		}

		let scene =
			Scene { tab: tab.id, files: layout.current, editor: layout.view, cell: (w, h) };
		let rect = match core.layer() {
			Layer::Mgr => {
				tab.current.hovered()?;
				let row = tab.current.cursor.checked_sub(tab.current.offset)?;
				if row >= usize::from(scene.files.height) {
					return None;
				}
				scene.target(scene.files.x, scene.files.y + row as u16, u32::from(scene.files.width) * w)
			}
			Layer::View => {
				let (pos, shape) = cursor?;
				if !scene.editor.contains(pos) {
					return None;
				}
				let width = if shape == SetCursorStyle::SteadyBar { w.div_ceil(6) } else { w };
				scene.target(pos.x, pos.y, width)
			}
			_ => return None,
		};
		Some((scene, rect))
	}

	pub(super) fn draw(&mut self, now: Instant) {
		let (Some((scene, _)), Some(motion)) = (self.target, &self.motion) else {
			return;
		};

		let progress = motion.progress(now);
		if progress >= 1.0 {
			self.stop();
			return;
		}

		let rect = motion.rect(progress);
		let deadline = (now + FRAME_INTERVAL).min(motion.started + DURATION);
		let (w, h) = scene.cell;
		let band = ((1.0 - progress) * (OPACITY.len() - 1) as f32).round() as u32 * h;
		if !self.upload_texture(u32::from(scene.files.width) * w, h) {
			self.stop();
			return;
		}

		let id = self.image_id;
		let mut out = TTY.lockout();
		let _ = write!(
			out,
			"{SaveCursorPos}{}\x1b_Ga=p,q=2,i={id},p=1,C=1,z=-1,X={},Y={},y={band},w={},h={h};\x1b\\{RestoreCursorPos}",
			MoveTo((rect.x / w) as u16, (rect.y / h) as u16),
			rect.x % w,
			rect.y % h,
			rect.width,
		);
		self.deadline = Some(deadline);
	}

	fn upload_texture(&mut self, width: u32, height: u32) -> bool {
		if self.uploaded {
			return true;
		}

		let size = (width, height);
		if self.texture.as_ref().is_none_or(|(cached, _)| *cached != size) {
			// One texture holds the same solid color at different opacities. Each frame
			// crops a strip to the animated width, so Kitty can reuse the GPU texture.
			let rows = height * OPACITY.len() as u32;
			let Some(pixels) = width.checked_mul(rows).filter(|&pixels| pixels <= 1_000_000) else {
				return false;
			};
			let mut rgba = vec![0; pixels as usize * 4];
			for (band, alpha) in rgba.chunks_exact_mut((width * height) as usize * 4).zip(OPACITY) {
				for pixel in band.chunks_exact_mut(4) {
					pixel.copy_from_slice(&[120, 157, 235, alpha]);
				}
			}
			let mut png = Vec::new();
			if PngEncoder::new(&mut png)
				.write_image(&rgba, width, rows, ExtendedColorType::Rgba8)
				.is_err()
			{
				return false;
			}

			let encoded = STANDARD.encode(png);
			let mut bytes = Vec::new();
			let mut chunks = encoded.as_bytes().chunks(4096).peekable();
			if let Some(chunk) = chunks.next() {
				_ = write!(
					bytes,
					"\x1b_Ga=t,f=100,q=2,i={},m={};",
					self.image_id,
					u8::from(chunks.peek().is_some())
				);
				bytes.extend_from_slice(chunk);
				bytes.extend_from_slice(b"\x1b\\");
			}
			while let Some(chunk) = chunks.next() {
				_ = write!(bytes, "\x1b_Gm={};", u8::from(chunks.peek().is_some()));
				bytes.extend_from_slice(chunk);
				bytes.extend_from_slice(b"\x1b\\");
			}
			self.texture = Some((size, bytes));
		}

		self.uploaded =
			self.texture.as_ref().is_some_and(|(_, bytes)| TTY.writer().write_all(bytes).is_ok());
		self.uploaded
	}

	fn stop(&mut self) {
		self.motion = None;
		self.deadline = None;
		if std::mem::take(&mut self.uploaded) {
			let _ = write!(TTY.writer(), "\x1b_Ga=d,q=2,d=I,i={};\x1b\\", self.image_id);
		}
	}
}
