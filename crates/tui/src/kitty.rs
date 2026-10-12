//! kitty graphics protocol rendering (AD-0010) — detect, fit, encode.
//!
//! The pipeline, pi's `terminal-image.js` step for step: env-sniff the
//! terminal, sniff pixel dimensions from headers, re-encode non-PNG
//! data (kitty accepts only PNG, `f=100`), fit the image to a cell
//! rectangle that preserves aspect, and emit the chunked APC sequence.
//! The sequence rides in one buffer cell (`C=1`: kitty must not move
//! the cursor — ratatui owns cursor math), overlaying `rows` empty
//! rows printed once into terminal-owned scrollback. No image
//! registry, no cropping, no delete-on-exit: hi never re-renders
//! history, so the terminal keeps the image the way it keeps text.

use hi_core::image_util::{dimensions, to_png};
use hi_core::tool::ImageBlock;
use std::borrow::Cow;
use std::time::{SystemTime, UNIX_EPOCH};

/// The protocols hi knows about. Exhaustive by house rule — iTerm2's
/// encoder would join here as a new variant, not a string flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// kitty graphics protocol: kitty, Ghostty, WezTerm, Warp.
    Kitty,
}

/// The environment variables detection reads, snapshotted. Pure
/// `detect_from` keeps the decision testable without process-env races.
#[derive(Debug, Default)]
pub struct EnvSnapshot {
    pub term_program: Option<String>,
    pub term: Option<String>,
    pub tmux: bool,
    pub kitty_window_id: bool,
    pub ghostty_resources_dir: bool,
    pub wezterm_pane: bool,
    pub warp_session: bool,
    /// `HI_IMAGE_PROTOCOL` — the env's last word over detection.
    pub image_protocol: Option<String>,
}

impl EnvSnapshot {
    pub fn capture() -> Self {
        let var = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
        EnvSnapshot {
            term_program: var("TERM_PROGRAM").map(|v| v.to_lowercase()),
            term: var("TERM").map(|v| v.to_lowercase()),
            tmux: var("TMUX").is_some()
                || var("TERM").is_some_and(|t| t.to_lowercase().starts_with("tmux")),
            kitty_window_id: var("KITTY_WINDOW_ID").is_some(),
            ghostty_resources_dir: var("GHOSTTY_RESOURCES_DIR").is_some(),
            wezterm_pane: var("WEZTERM_PANE").is_some(),
            warp_session: var("WARP_SESSION_ID").is_some()
                || var("WARP_TERMINAL_SESSION_UUID").is_some(),
            image_protocol: var("HI_IMAGE_PROTOCOL").map(|v| v.to_lowercase()),
        }
    }
}

/// pi's exact matrix, scoped to the kitty protocol. tmux and screen
/// are excluded wholesale: graphics passthrough there is unreliable
/// enough that pi declines too. Unknown terminals get the text note.
pub fn detect_from(env: &EnvSnapshot) -> Option<Protocol> {
    let program = env.term_program.as_deref();
    let detected = if env.tmux {
        // pi declines under tmux/screen wholesale: graphics
        // passthrough there is unreliable enough to never risk.
        None
    } else if env.kitty_window_id
        || program == Some("kitty")
        // Ghostty speaks the kitty protocol.
        || env.ghostty_resources_dir
        || program == Some("ghostty")
        || env.term.as_deref().is_some_and(|t| t.contains("ghostty"))
        // So do WezTerm and Warp.
        || env.wezterm_pane
        || program == Some("wezterm")
        || env.warp_session
        || program == Some("warpterminal")
    {
        Some(Protocol::Kitty)
    } else {
        None
    };
    match env.image_protocol.as_deref() {
        Some("kitty") => Some(Protocol::Kitty),
        Some("none" | "0") => None,
        _ => detected,
    }
}

/// Read the real environment once per print batch.
pub fn detect() -> Option<Protocol> {
    detect_from(&EnvSnapshot::capture())
}

/// Placement rectangle in cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellSize {
    pub columns: u16,
    pub rows: u16,
}

/// Fit an image into a cell rectangle, preserving aspect — pi's
/// `calculateImageCellSize` with its distortion chooser: cell-ceiling
/// rounding distorts, and the neighboring cell count is sometimes the
/// truer fit.
pub fn fit(dims: (u32, u32), max_cols: u16, max_rows: u16, cell: (f64, f64)) -> CellSize {
    let img_w = dims.0.max(1) as f64;
    let img_h = dims.1.max(1) as f64;
    let cell_w = cell.0.max(1.0);
    let cell_h = cell.1.max(1.0);
    let max_w = max_cols.max(1) as f64;
    let max_h = max_rows.max(1) as f64;
    let width_scale = max_w * cell_w / img_w;
    let height_scale = max_h * cell_h / img_h;
    let scale = width_scale.min(height_scale);
    let mut columns = ((img_w * scale / cell_w).ceil().max(1.0).min(max_w)) as u16;
    let mut rows = ((img_h * scale / cell_h).ceil().max(1.0).min(max_h)) as u16;
    if width_scale <= height_scale {
        let ideal_rows = columns as f64 * cell_w * img_h / (img_w * cell_h);
        rows = less_distorted(rows, ideal_rows);
    } else {
        let ideal_columns = rows as f64 * cell_h * img_w / (img_h * cell_w);
        columns = less_distorted(columns, ideal_columns);
    }
    CellSize { columns, rows }
}

/// pi's `chooseLessDistortedCellCount`: between the ceiled count and
/// one less, take whichever ratio is closer to the ideal.
fn less_distorted(upper: u16, ideal: f64) -> u16 {
    if upper <= 1 {
        return upper;
    }
    let distortion = |n: u16| (n as f64 / ideal).max(ideal / n as f64);
    if distortion(upper - 1) < distortion(upper) {
        upper - 1
    } else {
        upper
    }
}

/// Cell size in pixels — from the OS when the terminal reports real
/// window pixels, pi's 9×18 fallback otherwise.
pub fn cell_pixels() -> (f64, f64) {
    if let Ok(size) = ratatui::crossterm::terminal::window_size()
        && size.width > 0
        && size.height > 0
        && size.columns > 0
        && size.rows > 0
    {
        return (
            size.width as f64 / size.columns as f64,
            size.height as f64 / size.rows as f64,
        );
    }
    (9.0, 18.0)
}

/// Chunk size for base64 payloads — the protocol maximum (4096 bytes).
const CHUNK: usize = 4096;

/// Encode a transmit-and-display command for PNG `base64` data. The
/// spec: chunks no larger than 4096 bytes, `m=1` on all but the last
/// (`m=0`), and only the first chunk carries control keys. `C=1` is
/// not optional here: by default kitty moves the cursor right by
/// `columns` and down by `rows` after placing, and ratatui's backend
/// skips cursor moves between contiguous cells — the next row's text
/// would land where kitty left the cursor. `C=1` keeps the cursor
/// untouched; the `rows` buffer rows the sequence overlays provide the
/// screen space instead.
pub fn encode_kitty(base64: &str, columns: u16, rows: u16, image_id: u32) -> String {
    let params = format!("a=T,f=100,q=2,C=1,c={columns},r={rows},i={image_id}");
    if base64.len() <= CHUNK {
        return format!("\x1b_G{params};{base64}\x1b\\");
    }
    // base64 is ASCII: byte offsets are char offsets.
    let mut out =
        String::with_capacity(base64.len() + (base64.len() / CHUNK + 1) * (params.len() + 16));
    let mut offset = 0;
    let mut first = true;
    while offset < base64.len() {
        let end = (offset + CHUNK).min(base64.len());
        let last = end == base64.len();
        let flag = if first {
            first = false;
            format!("{params},m=1")
        } else if last {
            "m=0".to_string()
        } else {
            "m=1".to_string()
        };
        out.push_str(&format!("\x1b_G{flag};{}\x1b\\", &base64[offset..end]));
        offset = end;
    }
    out
}

/// Render caps from the ruling: the image never spans more than this
/// fraction of the screen.
pub const MAX_WIDTH_CELLS: u16 = 80;
pub const MAX_HEIGHT_CELLS: u16 = 24;

/// The full pipeline for one attached image: decode, sniff, convert to
/// PNG, fit, encode. `None` anywhere falls back to the text note — a
/// corrupt attachment renders as words, never as nothing.
pub fn render(image: &ImageBlock, max_cols: u16) -> Option<(String, u16)> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .ok()?;
    let dims = dimensions(&bytes)?;
    let png = to_png(&bytes)?;
    // A borrowed PNG is byte-identical to the input: reuse its base64
    // instead of re-encoding megabytes.
    let base64 = match png {
        Cow::Borrowed(_) => image.data.clone(),
        Cow::Owned(owned) => base64::engine::general_purpose::STANDARD.encode(owned),
    };
    let size = fit(
        dims,
        max_cols.min(MAX_WIDTH_CELLS),
        MAX_HEIGHT_CELLS,
        cell_pixels(),
    );
    Some((
        encode_kitty(&base64, size.columns, size.rows, next_image_id()),
        size.rows,
    ))
}

/// Distinct-ish 32-bit image IDs (the protocol range is [1, 2^32)):
/// xorshift seeded from the clock, pi-random in spirit. Images render
/// sequentially on the UI thread, so no synchronization beyond the
/// static.
fn next_image_id() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static STATE: AtomicU32 = AtomicU32::new(0);
    let mut state = STATE.load(Ordering::Relaxed);
    if state == 0 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(1);
        state = nanos | 1;
    }
    state ^= state << 13;
    state ^= state >> 17;
    state ^= state << 5;
    STATE.store(state, Ordering::Relaxed);
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_chunk_omits_the_m_key() {
        let seq = encode_kitty("aGVsbG8", 10, 4, 7);
        assert_eq!(seq, "\x1b_Ga=T,f=100,q=2,C=1,c=10,r=4,i=7;aGVsbG8\x1b\\");
    }

    #[test]
    fn oversized_payloads_chunk_at_4096() {
        // Exactly 2×CHUNK: two chunks, m=1 then m=0 — a zero-size
        // final chunk never happens.
        let payload = "A".repeat(8192);
        let seq = encode_kitty(&payload, 80, 24, 9);
        let parts: Vec<&str> = seq.split("\x1b\\").filter(|p| !p.is_empty()).collect();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].starts_with("\x1b_Ga=T,f=100,q=2,C=1,c=80,r=24,i=9,m=1;"));
        assert_eq!(parts[0].matches('A').count(), 4096);
        assert!(parts[1].starts_with("\x1b_Gm=0;"));
        assert_eq!(parts[1].matches('A').count(), 4096);
    }

    #[test]
    fn only_the_first_chunk_carries_control_keys() {
        let payload = "B".repeat(4097); // 4096 + 1: two chunks
        let seq = encode_kitty(&payload, 5, 2, 3);
        let parts: Vec<&str> = seq.split("\x1b\\").filter(|p| !p.is_empty()).collect();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].contains("i=3,m=1;"));
        assert_eq!(parts[1], "\x1b_Gm=0;B");
    }

    #[test]
    fn detection_follows_pis_matrix() {
        // Positive identifications.
        for env in [
            EnvSnapshot {
                term_program: Some("kitty".into()),
                ..Default::default()
            },
            EnvSnapshot {
                kitty_window_id: true,
                ..Default::default()
            },
            EnvSnapshot {
                term_program: Some("ghostty".into()),
                ..Default::default()
            },
            EnvSnapshot {
                ghostty_resources_dir: true,
                term_program: Some("app.exe".into()),
                ..Default::default()
            },
            EnvSnapshot {
                term: Some("xterm-ghostty".into()),
                ..Default::default()
            },
            EnvSnapshot {
                wezterm_pane: true,
                ..Default::default()
            },
            EnvSnapshot {
                warp_session: true,
                ..Default::default()
            },
        ] {
            assert_eq!(detect_from(&env), Some(Protocol::Kitty), "{env:?}");
        }
        // The skeptics: tmux first, then unknowns.
        assert_eq!(
            detect_from(&EnvSnapshot {
                kitty_window_id: true,
                tmux: true,
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            detect_from(&EnvSnapshot {
                term_program: Some("alacritty".into()),
                ..Default::default()
            }),
            None
        );
        assert_eq!(detect_from(&EnvSnapshot::default()), None);
        // The override is the env's last word, both directions.
        assert_eq!(
            detect_from(&EnvSnapshot {
                term_program: Some("alacritty".into()),
                image_protocol: Some("kitty".into()),
                ..Default::default()
            }),
            Some(Protocol::Kitty)
        );
        assert_eq!(
            detect_from(&EnvSnapshot {
                term_program: Some("kitty".into()),
                image_protocol: Some("none".into()),
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            detect_from(&EnvSnapshot {
                term_program: Some("kitty".into()),
                image_protocol: Some("0".into()),
                ..Default::default()
            }),
            None
        );
    }

    #[test]
    fn landscape_images_cap_at_width() {
        // 1600×900 px in 8×16 px cells, 100×100 cell budget: width binds.
        let size = fit((1600, 900), 100, 100, (8.0, 16.0));
        assert!(size.columns <= 100 && size.rows <= 100);
        assert_eq!(size.columns, 100);
        // Exact aspect is 28.125 rows; ceil gives 29, and pi's
        // distortion chooser takes the neighboring 28 as the truer fit.
        assert_eq!(size.rows, 28);
    }

    #[test]
    fn portrait_images_cap_at_height() {
        // 1080×1920 in 9×18 cells, 200-wide budget: height binds at 24.
        let size = fit((1080, 1920), 200, 24, (9.0, 18.0));
        assert_eq!(size.rows, 24);
        assert!(size.columns <= 200);
        // 1080 * (24*18/1920) / 9 = 27.
        assert_eq!(size.columns, 27);
    }

    #[test]
    fn small_images_upscale_to_the_budget_like_pi() {
        // pi's fit has no downscale-only clamp: a 1×1 px image in an
        // 80×24 cell budget at 9×18 px cells lands at 48×24 — the
        // terminal scales the bitmap to the placement rectangle.
        let size = fit((1, 1), 80, 24, (9.0, 18.0));
        assert_eq!(
            size,
            CellSize {
                columns: 48,
                rows: 24
            }
        );
    }

    #[test]
    fn zero_and_hostile_inputs_do_not_panic() {
        assert_eq!(
            fit((0, 0), 0, 0, (0.0, 0.0)),
            CellSize {
                columns: 1,
                rows: 1
            }
        );
    }

    const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    #[test]
    fn a_small_png_renders_as_a_kitty_sequence() {
        let image = ImageBlock {
            data: TINY_PNG_B64.to_string(),
            mime_type: "image/png".into(),
        };
        let (sequence, rows) = render(&image, 80).expect("valid PNG renders");
        // The 1×1 px fixture upscales to the 48×24 cell budget (9×18 px
        // cells); C=1 keeps the cursor where ratatui put it.
        assert!(sequence.starts_with("\x1b_Ga=T,f=100,q=2,C=1,c=48,r=24,i="));
        assert!(sequence.ends_with(format!("{TINY_PNG_B64}\x1b\\").as_str()));
        assert_eq!(rows, 24);
    }

    #[test]
    fn corrupt_data_falls_back_to_none() {
        use base64::Engine as _;
        let image = ImageBlock {
            data: base64::engine::general_purpose::STANDARD.encode([1, 2, 3, 4]),
            mime_type: "image/png".into(),
        };
        assert_eq!(render(&image, 80), None);
        let undecodable = ImageBlock {
            data: "!!!".into(),
            mime_type: "image/png".into(),
        };
        assert_eq!(render(&undecodable, 80), None);
    }

    #[test]
    fn image_ids_stay_in_the_protocol_range() {
        let first = next_image_id();
        for _ in 0..100 {
            let id = next_image_id();
            assert_ne!(id, 0);
            assert_ne!(id, first, "xorshift must move");
        }
    }
}
