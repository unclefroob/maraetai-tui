//! Album art rendering for the now-playing panel: fetches cover art over
//! HTTP, decodes it, and turns it into colored half-block terminal text —
//! two vertical pixels per terminal row (the upper-half-block glyph's own
//! color is the top pixel, the cell background is the bottom pixel). This
//! needs no special terminal support (sixel/kitty graphics protocols,
//! iTerm2's inline images) the way a "real" terminal image viewer would,
//! at the cost of chunkier resolution — a reasonable trade for a TUI that
//! has to keep working over SSH/tmux in whatever terminal the user has.

use image::DynamicImage;
use image::imageops::FilterType;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use tokio::sync::mpsc;

/// Rendered art is always exactly `WIDTH` columns by `HEIGHT` rows, so
/// swapping between "no art yet" (placeholder) and "art loaded" never
/// shifts the now-playing panel's layout. `WIDTH == 2 * HEIGHT` because a
/// terminal cell is roughly twice as tall as it is wide, and half-block
/// rendering already halves that back down (2 pixel-rows per cell) — so
/// this comes out close to square, matching a real album cover's aspect.
pub const WIDTH: u16 = 20;
pub const HEIGHT: u16 = 10;

/// One fetch's result: the URL it was fetched for (so a reply that arrives
/// after the user has already skipped to a different track — and this
/// track's art is no longer wanted — can be discarded by the caller) and
/// the rendered lines.
pub struct Fetched {
    pub url: String,
    pub lines: Vec<Line<'static>>,
}

/// Spawns a background fetch+decode+render of `url`, sending the result on
/// `tx`. Never blocks the caller and never surfaces an error to it — a
/// failed fetch or an undecodable image just means no `Fetched` ever
/// arrives, so the placeholder keeps showing instead of a jarring toast for
/// what's ultimately a cosmetic feature.
pub fn spawn_fetch(url: String, tx: mpsc::UnboundedSender<Fetched>) {
    tokio::spawn(async move {
        if let Some(lines) = fetch_and_render(&url).await {
            let _ = tx.send(Fetched { url, lines });
        }
    });
}

async fn fetch_and_render(url: &str) -> Option<Vec<Line<'static>>> {
    let bytes = reqwest::get(url).await.ok()?.bytes().await.ok()?;
    let img = image::load_from_memory(&bytes).ok()?;
    Some(render(&img))
}

/// Renders `img` into exactly `WIDTH` x `HEIGHT` terminal cells.
pub fn render(img: &DynamicImage) -> Vec<Line<'static>> {
    let resized = img
        .resize_exact(WIDTH as u32, HEIGHT as u32 * 2, FilterType::Triangle)
        .to_rgb8();
    (0..HEIGHT)
        .map(|row| {
            let spans: Vec<Span<'static>> = (0..WIDTH)
                .map(|col| {
                    let top = resized.get_pixel(col as u32, row as u32 * 2);
                    let bottom = resized.get_pixel(col as u32, row as u32 * 2 + 1);
                    Span::styled(
                        "\u{2580}", // upper half block
                        Style::default()
                            .fg(Color::Rgb(top[0], top[1], top[2]))
                            .bg(Color::Rgb(bottom[0], bottom[1], bottom[2])),
                    )
                })
                .collect();
            Line::from(spans)
        })
        .collect()
}

/// A same-size placeholder for when there's no art yet (no track loaded,
/// still fetching, or the fetch/decode failed) — a plain dashed box with a
/// centered note glyph, sized to exactly match `render()`'s output so the
/// info column beside it never jumps sideways when real art does load in.
pub fn placeholder() -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let inner_width = (WIDTH as usize).saturating_sub(2);
    (0..HEIGHT)
        .map(|row| {
            let text = if row == 0 {
                format!("\u{256d}{}\u{256e}", "\u{2500}".repeat(inner_width))
            } else if row == HEIGHT - 1 {
                format!("\u{2570}{}\u{256f}", "\u{2500}".repeat(inner_width))
            } else if row == HEIGHT / 2 {
                let left_pad = inner_width.saturating_sub(1) / 2;
                let right_pad = inner_width.saturating_sub(left_pad + 1);
                format!("\u{2502}{}\u{266a}{}\u{2502}", " ".repeat(left_pad), " ".repeat(right_pad))
            } else {
                format!("\u{2502}{}\u{2502}", " ".repeat(inner_width))
            };
            Line::from(Span::styled(text, dim))
        })
        .collect()
}
