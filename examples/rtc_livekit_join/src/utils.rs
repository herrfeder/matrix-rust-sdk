#[cfg(all(feature = "v4l2", target_os = "linux"))]
use std::sync::{Arc, Mutex};

#[cfg(all(feature = "v4l2", target_os = "linux"))]
use matrix_sdk_rtc_livekit::livekit::webrtc::prelude::VideoResolution;
#[cfg(all(feature = "v4l2", target_os = "linux"))]
use tokio::io::{AsyncBufReadExt, BufReader};

#[cfg(all(feature = "v4l2", target_os = "linux"))]
#[derive(Default)]
pub(crate) struct TextOverlayState {
    text: String,
    phase: u8,
    last_tick: Option<std::time::Instant>,
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
impl TextOverlayState {
    pub(crate) fn tick_and_draw(
        &mut self,
        resolution: &VideoResolution,
        dst_y: &mut [u8],
        stride_y: u32,
        dst_u: &mut [u8],
        stride_u: u32,
        dst_v: &mut [u8],
        stride_v: u32,
    ) {
        let now = std::time::Instant::now();
        let advance = self
            .last_tick
            .is_none_or(|last| now.duration_since(last) >= std::time::Duration::from_millis(120));
        if advance {
            self.phase = self.phase.wrapping_add(1);
            self.last_tick = Some(now);
        }

        draw_big_shiny_text(
            &self.text, self.phase, resolution, dst_y, stride_y, dst_u, stride_u, dst_v, stride_v,
        );
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn glyph_5x7(c: char) -> [u8; 7] {
    match c.to_ascii_uppercase() {
        'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        'C' => [0b01111, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b01111],
        'D' => [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
        'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        'G' => [0b01111, 0b10000, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110],
        'H' => [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'I' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b11111],
        'J' => [0b11111, 0b00010, 0b00010, 0b00010, 0b10010, 0b10010, 0b01100],
        'K' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        'L' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        'N' => [0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001],
        'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'Q' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b10101, 0b01010],
        'X' => [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        'Y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        'Z' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b10000, 0b11110, 0b00001, 0b00001, 0b11110],
        '6' => [0b01110, 0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110],
        '!' => [0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00000, 0b00100],
        '?' => [0b01110, 0b10001, 0b00001, 0b00110, 0b00100, 0b00000, 0b00100],
        '.' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00110, 0b00110],
        ',' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00110, 0b00110, 0b00100],
        '-' => [0b00000, 0b00000, 0b00000, 0b01110, 0b00000, 0b00000, 0b00000],
        ':' => [0b00000, 0b00110, 0b00110, 0b00000, 0b00110, 0b00110, 0b00000],
        '/' => [0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b00000, 0b00000],
        ' ' => [0; 7],
        _ => [0b11111, 0b10001, 0b00100, 0b00100, 0b00100, 0b10001, 0b11111],
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn draw_big_shiny_text(
    text: &str,
    phase: u8,
    resolution: &VideoResolution,
    dst_y: &mut [u8],
    stride_y: u32,
    dst_u: &mut [u8],
    stride_u: u32,
    dst_v: &mut [u8],
    stride_v: u32,
) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }

    let width = resolution.width as usize;
    let height = resolution.height as usize;
    let stride_y = stride_y as usize;

    let scale = 6usize;
    let char_w = 5 * scale;
    let char_h = 7 * scale;
    let spacing = scale + 2;
    let max_chars = ((width.saturating_sub(20)) / (char_w + spacing)).max(1);
    let visible: Vec<char> = text.chars().take(max_chars).collect();
    let total_w = visible.len().saturating_mul(char_w + spacing).saturating_sub(spacing);
    let x0 = (width.saturating_sub(total_w)) / 2;
    let y0 = (height.saturating_sub(char_h)) / 2;

    let pad = 12usize;
    fill_rect_i420(
        dst_y,
        stride_y,
        dst_u,
        stride_u,
        dst_v,
        stride_v,
        width,
        height,
        x0.saturating_sub(pad),
        y0.saturating_sub(pad),
        total_w + pad * 2,
        char_h + pad * 2,
        18,
        128,
        128,
    );

    for (i, ch) in visible.iter().enumerate() {
        let glyph = glyph_5x7(*ch);
        let gx = x0 + i * (char_w + spacing);

        for (row, bits) in glyph.iter().copied().enumerate() {
            for col in 0..5 {
                if (bits >> (4 - col)) & 1 == 1 {
                    let px = gx + col * scale;
                    let py = y0 + row * scale;
                    let shimmer = (((phase as usize + i + row + col) % 6) * 6) as u8;
                    let bright = 190u8.saturating_add(shimmer);
                    let rainbow_index = (phase as usize + i + row + col) % 7;
                    let (u, v) = match rainbow_index {
                        0 => (90, 240),
                        1 => (54, 34),
                        2 => (34, 163),
                        3 => (16, 146),
                        4 => (166, 16),
                        5 => (202, 222),
                        _ => (240, 110),
                    };
                    fill_rect_i420(
                        dst_y, stride_y, dst_u, stride_u, dst_v, stride_v, width, height, px, py,
                        scale, scale, bright, u, v,
                    );

                    if px + scale < width && py + scale < height {
                        fill_rect_i420(
                            dst_y,
                            stride_y,
                            dst_u,
                            stride_u,
                            dst_v,
                            stride_v,
                            width,
                            height,
                            px + scale / 2,
                            py + scale / 2,
                            scale / 2,
                            scale / 2,
                            235,
                            128,
                            128,
                        );
                    }
                }
            }
        }
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn fill_rect_i420(
    dst_y: &mut [u8],
    stride_y: usize,
    dst_u: &mut [u8],
    stride_u: u32,
    dst_v: &mut [u8],
    stride_v: u32,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    y_value: u8,
    u_value: u8,
    v_value: u8,
) {
    let x2 = (x + w).min(width);
    let y2 = (y + h).min(height);

    for row in y..y2 {
        let start = row * stride_y + x;
        let end = row * stride_y + x2;
        if start < dst_y.len() && end <= dst_y.len() && start < end {
            dst_y[start..end].fill(y_value);
        }
    }

    let stride_u = stride_u as usize;
    let stride_v = stride_v as usize;
    let uv_x = x / 2;
    let uv_x2 = x2.div_ceil(2);
    let uv_y = y / 2;
    let uv_y2 = y2.div_ceil(2);

    for row in uv_y..uv_y2 {
        let start = row * stride_u + uv_x;
        let end = row * stride_u + uv_x2;
        if start < dst_u.len() && end <= dst_u.len() && start < end {
            dst_u[start..end].fill(u_value);
        }
    }

    for row in uv_y..uv_y2 {
        let start = row * stride_v + uv_x;
        let end = row * stride_v + uv_x2;
        if start < dst_v.len() && end <= dst_v.len() && start < end {
            dst_v[start..end].fill(v_value);
        }
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
pub(crate) fn spawn_stdin_text_task(
    text_overlay: Arc<Mutex<TextOverlayState>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let stdin = BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(mut overlay) = text_overlay.lock() {
                overlay.text = line;
            }
        }
    })
}
