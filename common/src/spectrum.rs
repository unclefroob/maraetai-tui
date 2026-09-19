//! Wire-format constants for the spectrum visualizer — shared between the
//! daemon's FFT analyzer (which produces `u8` bar levels) and the TUI's
//! renderer (which turns them into terminal rows), so the two can't drift
//! apart on what a level actually means.

/// Number of frequency bars.
pub const BARS: usize = 16;
/// How many terminal rows a bar's level should be able to fill — the TUI
/// renders each bar across this many rows using 8 sub-levels per row (the
/// `▁▂▃▄▅▆▇█` block characters), for real vertical resolution instead of a
/// single flattened row. Matches the height of the album art beside it in
/// the now-playing panel (title + gauge + this + state = 10 rows).
pub const ROWS: usize = 7;
/// Highest level a bar can report: `ROWS` rows of 8 sub-levels each, minus
/// one (level 0 — silence — is also a valid value).
pub const MAX_LEVEL: u8 = (ROWS * 8 - 1) as u8;
