use crate::{
    PANEL_START,
    model::{MarqueePhase, MarqueeRuntime, Track},
};
use ab_glyph::{Font, FontArc, Glyph, GlyphId, PxScale, ScaleFont, point};
use cantus_shared::{GLYPH_ATLAS_SIZE, GlyphInstance, MAX_GLYPH_INSTANCES, pack_u16x2};
use glam::{Vec2, vec2};
use std::collections::HashMap;
use wgpu::{
    Device, Extent3d, Queue, Texture, TextureDescriptor, TextureDimension, TextureFormat,
    TextureUsages, TextureView, TextureViewDescriptor,
};

const FONT_SIZE: f32 = 16.0;
const FONT_SIZE_SMALL: f32 = 14.0;
const TEXT_PADDING: f32 = 12.0;
const ART_GAP: f32 = 8.0;
const MIN_TITLE_WIDTH_WITH_ART: f32 = 80.0;
const MIN_METADATA_TRACK_WIDTH: f32 = 24.0;
const MARQUEE_SPEED: f32 = 8.0;
const MARQUEE_PAUSE: f32 = 1.5;
const NO_CLIP_LEFT: f32 = -1_000_000.0;
const NO_CLIP_RIGHT: f32 = 1_000_000.0;

/// Size of the glyph atlas texture (square, in pixels).
const ATLAS_PADDING: u32 = 1;
const SCALE_STEPS: f32 = 4.0;

#[derive(Clone, Copy)]
struct AtlasEntry {
    pos: [u32; 2],
    size: [u32; 2],
    bearing: [i32; 2],
}

pub struct TextRenderer {
    panel_height: f32,
    font: FontArc,
    /// Glyph atlas texture.
    atlas: Texture,
    /// Packed glyph data keyed by glyph ID, size, and subpixel phase.
    atlas_cache: HashMap<(GlyphId, u16), AtlasEntry>,
    /// Current write cursor in the atlas (x, y, `row_height`).
    atlas_cursor: (u32, u32, u32),
    /// Queued glyph instances for the current frame.
    pub glyphs: Vec<GlyphInstance>,
}

impl TextRenderer {
    pub fn new(device: &Device, panel_height: f32) -> Self {
        let font =
            FontArc::try_from_slice(include_bytes!("../../../assets/NotoSans-Bold.ttf")).unwrap();

        let atlas = device.create_texture(&TextureDescriptor {
            label: Some("Glyph Atlas"),
            size: Extent3d {
                width: GLYPH_ATLAS_SIZE,
                height: GLYPH_ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::R8Unorm,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[],
        });
        Self {
            panel_height,
            font,
            atlas,
            atlas_cache: HashMap::new(),
            atlas_cursor: (0, 0, 0),
            glyphs: Vec::with_capacity(MAX_GLYPH_INSTANCES),
        }
    }

    pub fn atlas_view(&self) -> TextureView {
        self.atlas.create_view(&TextureViewDescriptor::default())
    }

    fn rasterize_glyph(&mut self, queue: &Queue, key: (GlyphId, u16)) -> Option<AtlasEntry> {
        if let Some(&entry) = self.atlas_cache.get(&key) {
            return Some(entry);
        }

        let scale = PxScale::from(f32::from(key.1) / SCALE_STEPS);
        let glyph = Glyph {
            id: key.0,
            scale,
            position: point(0.0, 0.0),
        };
        let outlined = self.font.as_scaled(scale).outline_glyph(glyph)?;
        let bounds = outlined.px_bounds();
        let width = bounds.width() as u32;
        let height = bounds.height() as u32;

        if width == 0 || height == 0 {
            return None;
        }

        // Leave a transparent texel around glyphs so linear filtering cannot
        // sample coverage from a neighbouring atlas entry.
        // Simple row-based packing; if it doesn't fit, start a new row.
        let (mut cx, mut cy, mut row_h) = self.atlas_cursor;
        if cx + width + ATLAS_PADDING * 2 > GLYPH_ATLAS_SIZE {
            cy += row_h;
            cx = 0;
            row_h = 0;
        }
        if cy + height + ATLAS_PADDING * 2 > GLYPH_ATLAS_SIZE {
            return None;
        }
        let gx = cx + ATLAS_PADDING;
        let gy = cy + ATLAS_PADDING;
        let row_h = row_h.max(height + ATLAS_PADDING * 2);
        self.atlas_cursor = (cx + width + ATLAS_PADDING * 2, cy, row_h);

        let mut buffer = vec![0u8; (width * height) as usize];
        outlined.draw(|x, y, c| {
            buffer[y as usize * width as usize + x as usize] = (c * 255.0).round() as u8;
        });

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.atlas,
                mip_level: 0,
                aspect: wgpu::TextureAspect::All,
                origin: wgpu::Origin3d { x: gx, y: gy, z: 0 },
            },
            &buffer,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: Some(height),
            },
            Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let entry = AtlasEntry {
            pos: [gx, gy],
            size: [width, height],
            bearing: [bounds.min.x as i32, bounds.min.y as i32],
        };
        self.atlas_cache.insert(key, entry);
        Some(entry)
    }

    pub fn shows_album_art(track_width: f32, panel_height: f32, show_details: bool) -> bool {
        !show_details
            || track_width - panel_height - TEXT_PADDING - ART_GAP >= MIN_TITLE_WIDTH_WITH_ART
    }

    pub fn render(
        &mut self,
        queue: &Queue,
        track: &mut Track,
        alpha: f32,
        show_album_art: bool,
        dt: f32,
        render_scale: f32,
    ) {
        let text_padding = text_padding(track.runtime.width);
        let text_start_left = track.runtime.start_x + text_padding;
        let trailing_space = if show_album_art {
            self.panel_height + ART_GAP
        } else {
            text_padding
        };
        let text_start_right = track.runtime.end_x() - trailing_space;
        let available_width = text_start_right - text_start_left;

        if available_width <= 0.0 {
            track.runtime.reset_marquee();
            return;
        }

        let alpha = alpha.clamp(0.0, 1.0);

        let without_suffix = track
            .name
            .split_once(" -")
            .map_or(track.name.as_str(), |(prefix, _)| prefix);
        let song_name = without_suffix
            .split_once('(')
            .map_or(without_suffix, |(prefix, _)| prefix)
            .trim();
        let song_name = if song_name.is_empty() {
            track.name.trim()
        } else {
            song_name
        };
        let title_only = track.runtime.width < MIN_METADATA_TRACK_WIDTH;
        let title_height = if title_only { 0.45 } else { 0.26 };
        let top_y = PANEL_START + (self.panel_height * title_height).floor();
        let bottom_y = PANEL_START + (self.panel_height * 0.57).floor();

        let measured_width = measure_text(&self.font, song_name, FONT_SIZE);

        let title_overflow = (measured_width - available_width).max(0.0);
        let (x, align, clip) = if title_overflow > 0.0 {
            let offset = update_marquee(&mut track.runtime.marquee, title_overflow, dt);
            let clip_left = if offset > 0.5 {
                text_start_left
            } else {
                NO_CLIP_LEFT
            };
            (
                text_start_left - offset,
                Align::Left,
                Some((
                    clip_left,
                    text_start_right,
                    edge_fade_width(available_width),
                )),
            )
        } else {
            track.runtime.reset_marquee();
            (text_start_right, Align::Right, None)
        };

        let seconds_until_start = (track.runtime.start_ms / 1000.0).abs();
        let time_text = if seconds_until_start >= 60.0 {
            let seconds = seconds_until_start as u32;
            format!("{}m{}s", seconds / 60, seconds % 60)
        } else {
            format!("{}s", seconds_until_start.round())
        };

        let bottom_merged = format!("{time_text}\u{2004}•\u{2004}{}", track.artist.name);
        let measured_bottom_width = measure_text(&self.font, &bottom_merged, FONT_SIZE_SMALL);
        let bottom_ratio = available_width / measured_bottom_width;
        let split_widths = (bottom_ratio > 1.0 && track.is_current()).then(|| {
            (
                measure_text(&self.font, &time_text, FONT_SIZE_SMALL),
                measure_text(&self.font, &track.artist.name, FONT_SIZE_SMALL),
            )
        });

        let mut queue_text = |text, width, origin, size, align, clip| {
            self.queue_glyphs(
                queue,
                text,
                width,
                origin,
                size,
                align,
                alpha,
                clip,
                render_scale,
            );
        };
        queue_text(
            song_name,
            measured_width,
            vec2(x, top_y),
            FONT_SIZE,
            align,
            clip,
        );

        if title_only {
            return;
        }

        if let Some((time_width, artist_width)) = split_widths {
            queue_text(
                &time_text,
                time_width,
                vec2(text_start_left, bottom_y),
                FONT_SIZE_SMALL,
                Align::Left,
                None,
            );
            queue_text(
                &track.artist.name,
                artist_width,
                vec2(text_start_right, bottom_y),
                FONT_SIZE_SMALL,
                Align::Right,
                None,
            );
        } else {
            let (x, align) = if bottom_ratio >= 1.0 {
                (text_start_right, Align::Right)
            } else {
                (text_start_left, Align::Left)
            };
            let size = FONT_SIZE_SMALL * bottom_ratio.clamp(0.8, 1.0);
            queue_text(
                &bottom_merged,
                measured_bottom_width * size / FONT_SIZE_SMALL,
                vec2(x, bottom_y),
                size,
                align,
                (bottom_ratio < 1.0).then_some((
                    NO_CLIP_LEFT,
                    text_start_right,
                    edge_fade_width(available_width),
                )),
            );
        }
    }

    fn queue_glyphs(
        &mut self,
        queue: &Queue,
        text: &str,
        total_width: f32,
        origin: Vec2,
        px_size: f32,
        align: Align,
        alpha: f32,
        clip: Option<(f32, f32, f32)>,
        render_scale: f32,
    ) {
        let scaled_font = self.font.as_scaled(px_size);
        let baseline_offset = scaled_font.ascent().midpoint(scaled_font.descent());

        let caret = match align {
            Align::Left => origin.x,
            Align::Right => origin.x - total_width,
        };

        let scale_quarters = (FONT_SIZE * render_scale * SCALE_STEPS)
            .round()
            .max(SCALE_STEPS) as u16;
        let glyph_scale = px_size / (FONT_SIZE * render_scale);
        let (clip_left, clip_right, fade_width) =
            clip.unwrap_or((NO_CLIP_LEFT, NO_CLIP_RIGHT, 1.0));
        let baseline_y = origin.y + baseline_offset;

        let font = self.font.clone();
        for (glyph_id, glyph_x, _) in layout_glyphs(&font, text, px_size, caret) {
            if self.glyphs.len() == MAX_GLYPH_INSTANCES {
                break;
            }
            let key = (glyph_id, scale_quarters);
            let Some(glyph) = self.rasterize_glyph(queue, key) else {
                continue;
            };
            self.glyphs.push(GlyphInstance {
                pos: vec2(
                    glyph_x + glyph.bearing[0] as f32 * glyph_scale,
                    baseline_y + glyph.bearing[1] as f32 * glyph_scale,
                ),
                size: vec2(
                    glyph.size[0] as f32 * glyph_scale,
                    glyph.size[1] as f32 * glyph_scale,
                ),
                atlas: [
                    pack_u16x2(glyph.pos),
                    pack_u16x2([glyph.pos[0] + glyph.size[0], glyph.pos[1] + glyph.size[1]]),
                ],
                clip_left,
                clip_right,
                alpha,
                fade_width,
            });
        }
    }
}

fn update_marquee(runtime: &mut MarqueeRuntime, extent: f32, dt: f32) -> f32 {
    runtime.elapsed += dt.clamp(0.0, 0.1);

    match runtime.phase {
        MarqueePhase::StartPause => {
            runtime.start = extent;
            if runtime.elapsed >= MARQUEE_PAUSE {
                runtime.phase = MarqueePhase::Forward;
                runtime.elapsed = 0.0;
            }
            0.0
        }
        MarqueePhase::Forward => {
            let progress = (runtime.elapsed / travel_duration(runtime.start)).min(1.0);
            let remaining = runtime.start * (1.0 - ease_in_out(progress));
            let offset = (extent - remaining).clamp(0.0, extent);
            if progress >= 1.0 {
                runtime.phase = MarqueePhase::EndPause;
                runtime.elapsed = 0.0;
            }
            offset
        }
        MarqueePhase::EndPause => {
            let offset = extent;
            if runtime.elapsed >= MARQUEE_PAUSE {
                runtime.phase = MarqueePhase::Backward;
                runtime.elapsed = 0.0;
                runtime.start = offset;
            }
            offset
        }
        MarqueePhase::Backward => {
            let progress = (runtime.elapsed / travel_duration(runtime.start)).min(1.0);
            let offset = runtime.start * (1.0 - ease_in_out(progress));
            if progress >= 1.0 {
                runtime.phase = MarqueePhase::StartPause;
                runtime.elapsed = 0.0;
                runtime.start = extent;
            }
            offset.min(extent)
        }
    }
}

fn travel_duration(distance: f32) -> f32 {
    // Cubic easing peaks at 1.5x its average velocity, so scale the
    // duration to keep the requested marquee speed as the actual maximum.
    (distance * 1.5 / MARQUEE_SPEED).max(0.001)
}

fn ease_in_out(progress: f32) -> f32 {
    progress * progress * (3.0 - 2.0 * progress)
}

fn edge_fade_width(available_width: f32) -> f32 {
    (available_width * 0.15).clamp(1.5, 5.0)
}

fn text_padding(track_width: f32) -> f32 {
    (track_width * 0.2).clamp(1.0, TEXT_PADDING)
}

#[derive(Copy, Clone)]
enum Align {
    Left,
    Right,
}

fn measure_text(font: &FontArc, text: &str, px_size: f32) -> f32 {
    layout_glyphs(font, text, px_size, 0.0)
        .last()
        .map_or(0.0, |(_, _, end)| end)
}

fn layout_glyphs<'a>(
    font: &'a FontArc,
    text: &'a str,
    px_size: f32,
    mut caret: f32,
) -> impl Iterator<Item = (GlyphId, f32, f32)> + 'a {
    let font = font.as_scaled(px_size);
    let mut previous = None;
    text.chars().map(move |c| {
        let glyph_id = font.glyph_id(c);
        if let Some(prev) = previous {
            caret += font.kern(prev, glyph_id);
        }
        let start = caret;
        caret += font.h_advance(glyph_id);
        previous = Some(glyph_id);
        (glyph_id, start, caret)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn album_art_yields_to_the_title_on_narrow_tracks() {
        assert!(TextRenderer::shows_album_art(50.0, 50.0, false));
        assert!(!TextRenderer::shows_album_art(149.0, 50.0, true));
        assert!(TextRenderer::shows_album_art(150.0, 50.0, true));
    }

    #[test]
    fn tiny_tracks_keep_some_title_viewport() {
        let track_width = 8.0;
        let available_width = track_width - text_padding(track_width) * 2.0;

        assert!(available_width >= 4.0);
    }

    #[test]
    fn marquee_uses_a_gentle_easing_curve() {
        let distance = 48.0;
        let mut runtime = MarqueeRuntime {
            phase: MarqueePhase::Forward,
            elapsed: travel_duration(distance) * 0.25,
            start: distance,
        };

        let offset = update_marquee(&mut runtime, distance, 0.0);

        assert!(offset < distance * 0.25);
    }

    #[test]
    fn marquee_speed_stays_below_the_limit() {
        let distance = 48.0;
        let mut runtime = MarqueeRuntime {
            phase: MarqueePhase::Forward,
            start: distance,
            ..Default::default()
        };
        let mut previous_offset = 0.0;

        for _ in 0..1_000 {
            let offset = update_marquee(&mut runtime, distance, 0.01);
            assert!((offset - previous_offset).abs() <= MARQUEE_SPEED * 0.01 + 0.001);
            previous_offset = offset;
        }
    }

    #[test]
    fn marquee_returns_when_the_visible_range_keeps_growing() {
        let mut runtime = MarqueeRuntime::default();
        let mut reached_end = false;
        let mut returned_to_start = false;

        for frame in 0..8_000 {
            let extent = 48.0 + frame as f32 * 0.1;
            let offset = update_marquee(&mut runtime, extent, 0.01);
            if runtime.phase == MarqueePhase::EndPause {
                reached_end = true;
                assert!((offset - extent).abs() < f32::EPSILON);
            }
            returned_to_start |= reached_end
                && runtime.phase == MarqueePhase::StartPause
                && offset.abs() < f32::EPSILON;
        }

        assert!(reached_end);
        assert!(returned_to_start);
    }
}
