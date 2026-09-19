//! Album art for the now-playing panel: fetches cover art over HTTP,
//! decodes it, and hands it to `ratatui-image`, which renders it as a real
//! image via whatever graphics protocol the terminal actually supports
//! (Kitty, Sixel, iTerm2) — degrading to colored half-block characters when
//! none of those are available, the same fallback ladder terminal image
//! viewers like `chafa`/`viu` use.

use image::DynamicImage;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui_image::picker::Picker;
use tokio::sync::mpsc;

/// Album art is rendered into a fixed-size column so the now-playing
/// panel's layout never shifts between "no art" and "art loaded". `WIDTH ==
/// 2 * HEIGHT` because a terminal cell is roughly twice as tall as it is
/// wide, so this comes out close to square — matching a real album cover's
/// aspect ratio. Only really governs the *placeholder*'s size and the panel
/// layout math; a real graphics protocol resizes to fit this cell area
/// using the terminal's actual font metrics, and half-block fallback fills
/// it exactly by construction.
pub const WIDTH: u16 = 20;
pub const HEIGHT: u16 = 10;

/// Builds a `Picker` for the current terminal: real pixel-accurate font
/// metrics and graphics-protocol detection when the terminal reports them
/// (via a termios ioctl, then a bounded ~1s terminal query — see
/// `ratatui-image::picker::Picker::guess_protocol`), degrading to a safe
/// guess — which still renders correctly via the half-block fallback —
/// when it can't.
///
/// Must be called after entering the alternate screen but before reading
/// any terminal input events (`ratatui-image`'s own requirement — its
/// protocol query briefly takes over raw stdin reads).
pub fn make_picker() -> Picker {
    let mut picker = Picker::from_termios().unwrap_or_else(|_| Picker::new((8, 16)));
    picker.guess_protocol();
    picker
}

/// One fetch's result: the URL it was fetched for (so a reply for a track
/// the user has since skipped past can be discarded by the caller) and the
/// decoded image, ready for `Picker::new_resize_protocol`.
pub struct Fetched {
    pub url: String,
    pub image: DynamicImage,
}

/// Spawns a background fetch+decode of `url`, sending the result on `tx`.
/// Never blocks the caller and never surfaces an error to it — a failed
/// fetch or an undecodable image just means no `Fetched` ever arrives, so
/// the placeholder keeps showing instead of a jarring toast for what's
/// ultimately a cosmetic feature.
pub fn spawn_fetch(url: String, tx: mpsc::UnboundedSender<Fetched>) {
    tokio::spawn(async move {
        if let Some(image) = fetch_and_decode(&url).await {
            let _ = tx.send(Fetched { url, image });
        }
    });
}

async fn fetch_and_decode(url: &str) -> Option<DynamicImage> {
    let bytes = reqwest::get(url).await.ok()?.bytes().await.ok()?;
    Some(center_crop_to_square(image::load_from_memory(&bytes).ok()?))
}

/// Crops the longer dimension down to match the shorter one, centered —
/// album art is almost always already square, but when it isn't this avoids
/// a surprising letterbox. Doing this crop *before* handing the image to
/// `ratatui-image` matters: its own `Resize::Crop` clips straight pixels out
/// of the original at the target's raw pixel size with no scaling step
/// first, so on a real (large) cover it zooms into an unrecognizable corner
/// rather than showing a scaled-down crop. Pre-cropping to the right aspect
/// ourselves and then asking it to `Fit` (scale, not clip) is what actually
/// produces a normal-looking thumbnail.
fn center_crop_to_square(img: DynamicImage) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    let side = w.min(h);
    let x = (w - side) / 2;
    let y = (h - side) / 2;
    img.crop_imm(x, y, side, side)
}

/// A same-size placeholder for when there's no art yet (no track loaded,
/// still fetching, or the fetch/decode failed) — a plain dashed box with a
/// centered note glyph, sized to match `WIDTH`x`HEIGHT` so the info column
/// beside it never jumps sideways when real art loads in.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crops_a_wide_image_to_a_centered_square() {
        let img = DynamicImage::new_rgb8(100, 60);
        let cropped = center_crop_to_square(img);
        assert_eq!((cropped.width(), cropped.height()), (60, 60));
    }

    #[test]
    fn crops_a_tall_image_to_a_centered_square() {
        let img = DynamicImage::new_rgb8(60, 100);
        let cropped = center_crop_to_square(img);
        assert_eq!((cropped.width(), cropped.height()), (60, 60));
    }

    #[test]
    fn leaves_an_already_square_image_unchanged() {
        let img = DynamicImage::new_rgb8(80, 80);
        let cropped = center_crop_to_square(img);
        assert_eq!((cropped.width(), cropped.height()), (80, 80));
    }
}
