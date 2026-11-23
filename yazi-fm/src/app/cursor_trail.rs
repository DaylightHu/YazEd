use std::{env, io::Write, time::{Duration, Instant}};

use base64::{Engine, engine::general_purpose::STANDARD};
use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
use ratatui_core::layout::{Position, Rect};
use yazi_config::LAYOUT;
use yazi_core::Core;
use yazi_emulator::{Dimension, EMULATOR};
use yazi_shared::{Layer, id::Id};
use yazi_tty::{TTY, sequence::{MoveTo, RestoreCursorPos, SaveCursorPos}};

const FRAME_INTERVAL: Duration = Duration::from_millis(10);
const SPRING_RATE: f32 = 40.0;
const TRAIL_SEGMENTS: usize = 3;
const TRAIL_LIFETIME: Duration = Duration::from_millis(180);
const TRAIL_FOLLOW_RATE: f32 = 20.0;
const OPACITY: [u8; TRAIL_SEGMENTS + 1] = [104, 65, 32, 144];

#[derive(Clone, Copy, Eq, PartialEq)]
struct Scene {
	tab:    Id,
	files:  Rect,
	editor: Rect,
	cell:   (u32, u32),
}

impl Scene {
	fn target(self, x: u16, y: u16, width: u32) -> PixelRect {
		PixelRect {
			x: u32::from(x) * self.cell.0,
			y: u32::from(y) * self.cell.1,
			width,
			height: self.cell.1,
		}
	}

	fn texture_width(self) -> u32 { u32::from(self.files.width.max(3)) * self.cell.0 }

	fn texture_band(self) -> u32 { 2 * self.cell.1 }

	fn extent(self) -> (u32, u32) {
		(
			u32::from(self.files.right().max(self.editor.right())) * self.cell.0,
			u32::from(self.files.bottom().max(self.editor.bottom())) * self.cell.1,
		)
	}

	fn clip(self, rect: PixelRect) -> PixelRect {
		let (right, bottom) = self.extent();
		let x = rect.x.min(right.saturating_sub(1));
		PixelRect {
			x,
			y: rect.y.min(bottom.saturating_sub(rect.height)),
			width: rect.width.clamp(1, self.texture_width().min(right.saturating_sub(x).max(1))),
			height: rect.height,
		}
	}

	fn trail_rect(self, x: i64, y: i64, width: u32, height: u32) -> Option<PixelRect> {
		let (right, bottom) = self.extent();
		let top = match (self.files.is_empty(), self.editor.is_empty()) {
			(true, _) => self.editor.y,
			(_, true) => self.files.y,
			_ => self.files.y.min(self.editor.y),
		};
		let left = x.max(0);
		let top = y.max(i64::from(top) * i64::from(self.cell.1));
		let width = (x + i64::from(width)).min(i64::from(right)) - left;
		let height = (y + i64::from(height)).min(i64::from(bottom)) - top;
		if width <= 0 || height <= 0 {
			return None;
		}
		Some(PixelRect {
			x:      left as u32,
			y:      top as u32,
			width:  width as u32,
			height: height as u32,
		})
	}
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct PixelRect {
	x:      u32,
	y:      u32,
	width:  u32,
	height: u32,
}

impl PixelRect {
	fn center(self) -> [f32; 2] {
		[self.x as f32 + self.width as f32 * 0.5, self.y as f32 + self.height as f32 * 0.5]
	}
}

struct Glide {
	position:   [f32; 3],
	velocity:   [f32; 3],
	last_frame: Instant,
}

impl Glide {
	fn new(rect: PixelRect, now: Instant) -> Self {
		Self {
			position:   [rect.x as _, rect.y as _, rect.width as _],
			velocity:   [0.0; 3],
			last_frame: now,
		}
	}

	fn advance(&mut self, target: PixelRect, now: Instant) -> PixelRect {
		let elapsed = now.duration_since(self.last_frame).as_secs_f32().min(1.0);
		self.last_frame = now;

		// A critically damped spring keeps both position and speed through a retarget.
		let decay = (-SPRING_RATE * elapsed).exp();
		for ((position, velocity), goal) in self.position.iter_mut().zip(&mut self.velocity).zip([
			target.x as f32,
			target.y as f32,
			target.width as f32,
		]) {
			let distance = *position - goal;
			let slope = *velocity + SPRING_RATE * distance;
			let offset = (distance + slope * elapsed) * decay;
			*position = goal + offset;
			*velocity = (slope - SPRING_RATE * (distance + slope * elapsed)) * decay;
			if offset.abs() < 0.5 && velocity.abs() < 16.0 {
				*position = goal;
				*velocity = 0.0;
			}
		}

		let [x, y, width] = self.position.map(|value| value.round() as u32);
		PixelRect { x, y, width, height: target.height }
	}

	fn settled(&self, target: PixelRect) -> bool {
		self.velocity == [0.0; 3]
			&& self.position == [target.x as f32, target.y as f32, target.width as f32]
	}
}

struct Trail {
	center:     [f32; 2],
	updated_at: Instant,
	moved_at:   Instant,
}

impl Trail {
	fn new(head: PixelRect, now: Instant) -> Self {
		Self { center: head.center(), updated_at: now, moved_at: now }
	}

	fn advance(
		&mut self,
		scene: Scene,
		head: PixelRect,
		moved: bool,
		now: Instant,
	) -> [Option<PixelRect>; TRAIL_SEGMENTS] {
		if moved {
			self.moved_at = now;
		}
		let decay = (-TRAIL_FOLLOW_RATE * now.duration_since(self.updated_at).as_secs_f32()).exp();
		self.updated_at = now;
		let center = head.center();
		let offset: [f32; 2] = std::array::from_fn(|i| {
			let offset = (self.center[i] - center[i]) * decay;
			self.center[i] = center[i] + offset;
			offset
		});
		if now.duration_since(self.moved_at) >= TRAIL_LIFETIME {
			return [None; TRAIL_SEGMENTS];
		}

		// Build a tapered wake along the dominant direction. Each strip borders
		// the next one without painting over the head.
		let horizontal = offset[0].abs() >= offset[1].abs();
		let (along, across, edge, size, cross_center, breadth, limit) = if horizontal {
			(offset[0], offset[1], head.x, head.width, center[1], head.height, 8 * scene.cell.0)
		} else {
			(offset[1], offset[0], head.y, head.height, center[0], head.width, 5 * scene.cell.1)
		};
		let across = across.clamp(-(breadth as f32) * 0.75, breadth as f32 * 0.75);
		let length = along.abs().round().min(limit as f32) as u32;
		std::array::from_fn(|i| {
			let start = length * i as u32 / TRAIL_SEGMENTS as u32;
			let end = length * (i + 1) as u32 / TRAIL_SEGMENTS as u32;
			if start == end {
				return None;
			}
			let taper = i as f32 / TRAIL_SEGMENTS as f32;
			let width = (breadth as f32 * (1.0 - taper * 0.60)).round().max(1.0) as u32;
			let cross = (cross_center + across * taper - width as f32 * 0.5).round() as i64;
			let along = if along < 0.0 {
				i64::from(edge) - i64::from(end)
			} else {
				i64::from(edge) + i64::from(size) + i64::from(start)
			};
			if horizontal {
				scene.trail_rect(along, cross, end - start, width)
			} else {
				scene.trail_rect(cross, along, width, end - start)
			}
		})
	}
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct TextureKey {
	width:  u32,
	height: u32,
	color:  [u8; 3],
}

pub(super) struct CursorTrail {
	enabled:  bool,
	image_id: u32,
	target:   Option<(Scene, PixelRect)>,
	shown:    Option<PixelRect>,
	trail:    Option<Trail>,
	tails:    [Option<PixelRect>; TRAIL_SEGMENTS],
	glide:    Option<Glide>,
	deadline: Option<Instant>,
	texture:  Option<(TextureKey, Vec<u8>)>,
	uploaded: bool,
}

impl CursorTrail {
	pub(super) fn new() -> Self {
		Self {
			enabled:  env::var_os("YAZED_CURSOR_TRAIL").is_none_or(|v| v != "0"),
			// The preview adapter uses the low 24 bits of the process ID.
			image_id: std::process::id() | 0x8000_0000,
			target:   None,
			shown:    None,
			trail:    None,
			tails:    [None; TRAIL_SEGMENTS],
			glide:    None,
			deadline: None,
			texture:  None,
			uploaded: false,
		}
	}

	pub(super) fn deadline(&self) -> Option<Instant> { self.deadline }

	pub(super) fn position(&self) -> Option<Position> {
		let (scene, _) = self.target?;
		let head = self.shown?;
		Some(Position { x: (head.x / scene.cell.0) as u16, y: (head.y / scene.cell.1) as u16 })
	}

	pub(super) fn reset(&mut self) {
		self.target = None;
		self.shown = None;
		self.trail = None;
		self.tails = [None; TRAIL_SEGMENTS];
		self.glide = None;
		self.deadline = None;
		if std::mem::take(&mut self.uploaded) {
			let _ = write!(TTY.writer(), "\x1b_Ga=d,q=2,d=I,i={};\x1b\\", self.image_id);
		}
	}

	pub(super) fn track(&mut self, core: &Core, now: Instant) -> bool {
		let Some((scene, rect)) = self.focus(core) else {
			self.reset();
			return false;
		};
		let color = EMULATOR.cursor_color.get().map_or_else(
			|| if EMULATOR.light() == Some(true) { [41, 78, 143] } else { [120, 157, 235] },
			|rgb| rgb.map(|channel| (channel >> 8) as u8),
		);
		let key = TextureKey { width: scene.texture_width(), height: scene.texture_band(), color };
		if self.texture.as_ref().is_none_or(|(previous, _)| *previous != key) {
			self.reset();
			let Some(encoded) = self.encode_texture(key) else {
				return false;
			};
			self.texture = Some((key, encoded));
		} else if self.target.is_some_and(|(previous, _)| previous != scene) {
			self.reset();
		}

		if let Some((_, old)) = self.target.replace((scene, rect))
			&& old != rect
			&& self.glide.is_none()
		{
			self.glide = Some(Glide::new(old, now));
			self.trail.get_or_insert_with(|| Trail::new(old, now));
			self.deadline = Some(now + FRAME_INTERVAL);
		}
		true
	}

	fn focus(&self, core: &Core) -> Option<(Scene, PixelRect)> {
		if !self.enabled || !EMULATOR.supports_graphical_cursor() {
			return None;
		}

		let tab = core.active();
		let layout = LAYOUT.get();
		let layer = core.layer();
		if !matches!(layer, Layer::Mgr | Layer::View) {
			return None;
		}

		let (w, h) = Dimension::cell_size()?;
		let (w, h) = (w.floor() as u32, h.floor() as u32);
		if w == 0 || h == 0 {
			return None;
		}

		let scene =
			Scene { tab: tab.id, files: layout.current, editor: layout.view, cell: (w, h) };
		let rect = match layer {
			Layer::Mgr => {
				tab.current.hovered()?;
				let row = tab.current.cursor.checked_sub(tab.current.offset)?;
				if scene.files.is_empty() || row >= usize::from(scene.files.height) {
					return None;
				}
				scene.target(scene.files.x, scene.files.y + row as u16, u32::from(scene.files.width) * w)
			}
			Layer::View => {
				let view = tab.view.as_ref()?;
				let pos = view.screen_cursor(scene.editor)?;
				if !scene.editor.contains(pos) {
					return None;
				}
				let width = if view.insert {
					w.div_ceil(6)
				} else {
					let cells = view.cursor_cells();
					if cells <= usize::from(scene.editor.right() - pos.x) {
						(cells as u32 * w).min(scene.texture_width())
					} else {
						w
					}
				};
				scene.target(pos.x, pos.y, width)
			}
			_ => return None,
		};
		Some((scene, rect))
	}

	pub(super) fn draw(&mut self, now: Instant) {
		let Some((scene, target)) = self.target else { return };
		if self.shown.is_some() && self.deadline.is_none_or(|deadline| now < deadline) {
			return;
		}

		let mut head = target;
		if let Some(glide) = &mut self.glide {
			head = scene.clip(glide.advance(target, now));
			if glide.settled(target) {
				self.glide = None;
			}
		}
		let moved = self.shown.is_some_and(|old| old != head);
		let tails = self
			.trail
			.as_mut()
			.map_or([None; TRAIL_SEGMENTS], |trail| trail.advance(scene, head, moved, now));
		if self.glide.is_none() && tails.iter().all(Option::is_none) {
			self.trail = None;
		}
		self.deadline = (self.glide.is_some() || self.trail.is_some()).then_some(now + FRAME_INTERVAL);

		if self.shown == Some(head) && self.tails == tails {
			return;
		}
		let mut out = TTY.lockout();
		if !self.uploaded {
			let Some((_, texture)) = &self.texture else { return };
			if out.write_all(texture).is_err() {
				return;
			}
			self.uploaded = true;
		}

		let _ = write!(out, "{SaveCursorPos}");
		for (index, (&old, &new)) in self.tails.iter().zip(&tails).enumerate() {
			if old == new {
				continue;
			}
			let placement = 2 + index as u32;
			if let Some(rect) = new {
				self.place(&mut *out, scene, rect, placement, index as u32, -2 - index as i32);
			} else {
				let _ = write!(out, "\x1b_Ga=d,q=2,d=i,i={},p={placement};\x1b\\", self.image_id);
			}
		}
		if self.shown != Some(head) {
			self.place(&mut *out, scene, head, 1, TRAIL_SEGMENTS as u32, -1);
		}
		let _ = write!(out, "{RestoreCursorPos}");
		self.shown = Some(head);
		self.tails = tails;
	}

	fn place(
		&self,
		out: &mut impl Write,
		scene: Scene,
		rect: PixelRect,
		placement: u32,
		band: u32,
		z: i32,
	) {
		let (w, h) = scene.cell;
		let source_y = band * scene.texture_band();
		let _ = write!(
			out,
			"{}\x1b_Ga=p,q=2,i={},p={placement},C=1,z={z},X={},Y={},y={source_y},w={},h={};\x1b\\",
			MoveTo((rect.x / w) as u16, (rect.y / h) as u16),
			self.image_id,
			rect.x % w,
			rect.y % h,
			rect.width,
			rect.height,
		);
	}

	fn encode_texture(&self, key: TextureKey) -> Option<Vec<u8>> {
		// Both panes share one texture. Kitty crops each placement to the current width.
		let rows = key.height * OPACITY.len() as u32;
		let pixels = key.width.checked_mul(rows).filter(|&pixels| pixels <= 4_000_000)?;
		let mut rgba = vec![0; pixels as usize * 4];
		let [r, g, b] = key.color;
		for (band, alpha) in rgba.chunks_exact_mut((key.width * key.height) as usize * 4).zip(OPACITY) {
			for pixel in band.chunks_exact_mut(4) {
				pixel.copy_from_slice(&[r, g, b, alpha]);
			}
		}
		let mut png = Vec::new();
		PngEncoder::new(&mut png).write_image(&rgba, key.width, rows, ExtendedColorType::Rgba8).ok()?;

		let encoded = STANDARD.encode(png);
		let mut bytes = Vec::new();
		let chunks = encoded.as_bytes().chunks(4096);
		let count = chunks.len();
		for (index, chunk) in chunks.enumerate() {
			let more = u8::from(index + 1 < count);
			if index == 0 {
				_ = write!(bytes, "\x1b_Ga=t,f=100,q=2,i={},m={more};", self.image_id);
			} else {
				_ = write!(bytes, "\x1b_Gm={more};");
			}
			bytes.extend_from_slice(chunk);
			bytes.extend_from_slice(b"\x1b\\");
		}
		Some(bytes)
	}
}
