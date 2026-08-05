//! Measures what the terminal can actually render, and what it looks like.
//!
//! Two independent facts drive the adaptive palette:
//!
//! * **How many colors stdout can carry** ([`stdout_color_level`]), read from
//!   the environment. This decides whether a blended background can be sent as
//!   truecolor RGB, has to be snapped to the 256-color cube, or must be dropped
//!   entirely in favour of a foreground-only treatment.
//! * **The terminal's own default foreground and background**
//!   ([`default_colors`]), obtained by asking the terminal directly with OSC 10
//!   and OSC 11. Knowing the real background is what lets a diff row be tinted
//!   *relative to it* instead of against an assumed black or white.
//!
//! Both degrade safely. A terminal that ignores the OSC queries — tmux without
//! passthrough, a CI log, a dumb pipe — yields `None`, and every caller has a
//! modifier-only fallback for that case.

use std::sync::OnceLock;

use ratatui::style::Color;

/// How much color stdout can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdoutColorLevel {
    TrueColor,
    Ansi256,
    Ansi16,
    /// Nothing in the environment claims color support. Treated as the floor:
    /// callers fall back to modifiers rather than risking an unreadable fill.
    Unknown,
}

/// The terminal's own default foreground and background, as reported by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultColors {
    pub fg: (u8, u8, u8),
    pub bg: (u8, u8, u8),
}

static DEFAULT_COLORS: OnceLock<Option<DefaultColors>> = OnceLock::new();
static COLOR_LEVEL: OnceLock<StdoutColorLevel> = OnceLock::new();

/// Record the startup probe's result. Called once, before the TUI starts.
///
/// Later calls are ignored rather than panicking: a second probe (after a
/// suspend/resume, say) should not be able to take down a running session.
pub fn set_default_colors(colors: Option<DefaultColors>) {
    let _ = DEFAULT_COLORS.set(colors);
}

pub fn default_colors() -> Option<DefaultColors> {
    *DEFAULT_COLORS.get_or_init(|| None)
}

pub fn stdout_color_level() -> StdoutColorLevel {
    *COLOR_LEVEL.get_or_init(|| color_level_from_env(|key| std::env::var(key).ok()))
}

/// Resolve the color level from environment variables.
///
/// Split out from [`stdout_color_level`] so it can be tested without mutating
/// the process environment, which races across parallel test threads.
fn color_level_from_env(var: impl Fn(&str) -> Option<String>) -> StdoutColorLevel {
    // An explicit opt-out wins over every capability hint below.
    if var("NO_COLOR").is_some_and(|value| !value.is_empty()) {
        return StdoutColorLevel::Unknown;
    }

    if let Some(colorterm) = var("COLORTERM")
        && (colorterm.eq_ignore_ascii_case("truecolor") || colorterm.eq_ignore_ascii_case("24bit"))
    {
        return StdoutColorLevel::TrueColor;
    }

    // Terminals that are truecolor in practice but do not always export
    // COLORTERM (notably when reached over SSH or through a login shell that
    // scrubs the environment).
    if let Some(program) = var("TERM_PROGRAM")
        && matches!(
            program.as_str(),
            "iTerm.app" | "WezTerm" | "ghostty" | "vscode" | "Hyper" | "rio"
        )
    {
        return StdoutColorLevel::TrueColor;
    }

    let Some(term) = var("TERM") else {
        return StdoutColorLevel::Unknown;
    };
    if term == "dumb" {
        return StdoutColorLevel::Unknown;
    }
    if term.contains("truecolor") || term.contains("direct") {
        return StdoutColorLevel::TrueColor;
    }
    if term.contains("256") {
        return StdoutColorLevel::Ansi256;
    }
    if term.contains("color") || term.starts_with("xterm") || term.starts_with("screen") {
        return StdoutColorLevel::Ansi16;
    }
    StdoutColorLevel::Unknown
}

/// True when `bg` is bright enough that dark text reads better on it.
pub fn is_light(bg: (u8, u8, u8)) -> bool {
    let (r, g, b) = bg;
    // Rec. 601 luma. Good enough for a binary light/dark decision and cheap.
    let luma = 0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b);
    luma > 128.0
}

/// Composite `fg` over `bg` at `alpha`.
///
/// This is the whole trick behind adaptive backgrounds: a diff row tint is
/// defined as a *fraction of the way* from the terminal's real background
/// toward a hue, so it stays subtle on black, on white, and on solarized.
pub fn blend(fg: (u8, u8, u8), bg: (u8, u8, u8), alpha: f32) -> (u8, u8, u8) {
    let alpha = alpha.clamp(0.0, 1.0);
    let mix = |f: u8, b: u8| (f32::from(f) * alpha + f32::from(b) * (1.0 - alpha)).round() as u8;
    (mix(fg.0, bg.0), mix(fg.1, bg.1), mix(fg.2, bg.2))
}

/// Perceptual distance between two colors (CIE76, Euclidean in Lab).
///
/// Used to snap a blended RGB to the closest 256-color cube entry. Naive RGB
/// distance picks visibly wrong neighbours for the desaturated tints we
/// generate, because it treats a unit of blue as it treats a unit of green.
pub fn perceptual_distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
    fn srgb_to_linear(c: u8) -> f32 {
        let c = f32::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    fn to_lab((r, g, b): (u8, u8, u8)) -> (f32, f32, f32) {
        let (r, g, b) = (srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b));
        // sRGB -> XYZ (D65)
        let x = r * 0.4124 + g * 0.3576 + b * 0.1805;
        let y = r * 0.2126 + g * 0.7152 + b * 0.0722;
        let z = r * 0.0193 + g * 0.1192 + b * 0.9505;
        // XYZ -> Lab, normalised against the D65 reference white.
        let f = |t: f32| {
            if t > 0.008856 {
                t.powf(1.0 / 3.0)
            } else {
                7.787 * t + 16.0 / 116.0
            }
        };
        let (fx, fy, fz) = (f(x / 0.95047), f(y), f(z / 1.08883));
        (116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz))
    }

    let (l1, a1, b1) = to_lab(a);
    let (l2, a2, b2) = to_lab(b);
    ((l1 - l2).powi(2) + (a1 - a2).powi(2) + (b1 - b2).powi(2)).sqrt()
}

/// The RGB value of an xterm 256-color index, for indices 16..=255 only.
///
/// Indices 0..=15 are deliberately excluded everywhere in this module: those
/// are exactly the slots a terminal theme redefines, so their "standard" values
/// are fiction. 16..=255 are fixed by the xterm specification and are the only
/// indices whose appearance we can actually predict.
fn xterm_rgb(index: u8) -> (u8, u8, u8) {
    if index >= 232 {
        // 24-step grayscale ramp.
        let level = 8 + (index - 232) * 10;
        return (level, level, level);
    }
    // 6x6x6 color cube. The steps are not evenly spaced; the gap from 0 to 95
    // is wider than the rest.
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let offset = index - 16;
    (
        STEPS[(offset / 36) as usize],
        STEPS[((offset % 36) / 6) as usize],
        STEPS[(offset % 6) as usize],
    )
}

#[allow(clippy::disallowed_methods)]
pub fn rgb_color((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

/// The closest renderable color to `target` at the given color level.
///
/// Returns `None` when the terminal cannot express the target at all, which
/// callers turn into a modifier-only fallback rather than a wrong fill.
#[allow(clippy::disallowed_methods)]
pub fn best_color_for_level(target: (u8, u8, u8), level: StdoutColorLevel) -> Option<Color> {
    match level {
        StdoutColorLevel::TrueColor => Some(rgb_color(target)),
        StdoutColorLevel::Ansi256 => {
            let index = (16u8..=255)
                .min_by(|&a, &b| {
                    perceptual_distance(xterm_rgb(a), target)
                        .total_cmp(&perceptual_distance(xterm_rgb(b), target))
                })
                .unwrap_or(16);
            Some(Color::Indexed(index))
        }
        StdoutColorLevel::Ansi16 | StdoutColorLevel::Unknown => None,
    }
}

/// Parse one OSC color response, e.g. `ESC ] 11 ; rgb:1c1c/1c1c/1c1c BEL`.
///
/// `code` is 10 for foreground, 11 for background. Components may be 1-4 hex
/// digits wide depending on the terminal, and are scaled to 8 bits.
#[cfg(any(unix, test))]
fn parse_osc_color(buffer: &[u8], code: u8) -> Option<(u8, u8, u8)> {
    let text = String::from_utf8_lossy(buffer);
    let marker = format!("]{code};");
    let rest = text.split(&marker).nth(1)?;
    let rest = rest.split("rgb:").nth(1)?;

    let mut components = rest.split('/');
    let mut parse_component = || -> Option<u8> {
        let raw: String = components
            .next()?
            .chars()
            .take_while(char::is_ascii_hexdigit)
            .collect();
        if raw.is_empty() || raw.len() > 4 {
            return None;
        }
        let value = u32::from_str_radix(&raw, 16).ok()?;
        // Scale from the reported width down to 8 bits: a 4-digit `ffff` and a
        // 2-digit `ff` both mean "fully on".
        let max = 16u32.pow(raw.len() as u32) - 1;
        Some(((value * 255 + max / 2) / max) as u8)
    };

    Some((parse_component()?, parse_component()?, parse_component()?))
}

/// True once a complete primary device attributes (DA1) response has arrived.
///
/// DA1 is a CSI sequence ending in `c`, usually `ESC [ ? ... c`. Looking for a
/// bare `c` is not enough because it is also a valid hex digit in OSC color
/// responses; stopping on one leaves the rest of the reply in the terminal's
/// input queue, where it is mistaken for user input when the TUI starts.
#[cfg(any(unix, test))]
fn contains_da1_response(buffer: &[u8]) -> bool {
    let mut offset = 0;
    while offset < buffer.len() {
        let parameter_start = if buffer[offset] == 0x9b {
            offset + 1
        } else if buffer[offset..].starts_with(b"\x1b[") {
            offset + 2
        } else {
            offset += 1;
            continue;
        };

        let mut cursor = parameter_start;
        while buffer
            .get(cursor)
            .is_some_and(|byte| matches!(byte, 0x30..=0x3f))
        {
            cursor += 1;
        }
        while buffer
            .get(cursor)
            .is_some_and(|byte| matches!(byte, 0x20..=0x2f))
        {
            cursor += 1;
        }
        if buffer.get(cursor) == Some(&b'c') {
            return true;
        }
        offset = parameter_start;
    }
    false
}

/// Read replies until the DA1 sentinel lands, the source ends, or time is up.
///
/// Split out of [`imp::probe`] so the part that decides *when the terminal has
/// finished answering* can be tested against a scripted source. That decision
/// is the whole correctness question here — stopping early leaves the rest of
/// the reply in the input queue, where the TUI reads it as user keystrokes —
/// and it is the one part that needs no terminal to exercise.
///
/// `ready` reports whether the budget allows another blocking read; `read`
/// fills a chunk the way [`std::io::Read::read`] does.
#[cfg(any(unix, test))]
fn drain_replies(
    mut read: impl FnMut(&mut [u8]) -> std::io::Result<usize>,
    mut ready: impl FnMut() -> bool,
) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(128);
    let mut chunk = [0u8; 128];
    loop {
        if !ready() {
            break;
        }
        match read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                // Once the complete DA1 reply arrives, anything the terminal
                // meant to say about colors has already been said, because it
                // answers queries in order.
                if contains_da1_response(&buffer) {
                    break;
                }
            }
            // A signal mid-read is not the terminal declining to answer.
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    buffer
}

/// Pick the foreground and background out of a drained reply buffer.
///
/// Both must be present: a terminal that answered one query but not the other
/// tells us nothing safe about the pair, so callers fall back to modifiers.
#[cfg(any(unix, test))]
fn colors_from_replies(buffer: &[u8]) -> Option<DefaultColors> {
    Some(DefaultColors {
        fg: parse_osc_color(buffer, 10)?,
        bg: parse_osc_color(buffer, 11)?,
    })
}

/// Ask the terminal for its default foreground and background.
///
/// Returns `None` whenever the answer would be a guess: not a TTY, the terminal
/// stayed silent, or the platform has no OSC support. Callers must treat `None`
/// as "use modifiers", never as "assume dark".
pub fn probe_default_colors() -> Option<DefaultColors> {
    if std::env::var("MJ_NO_TERMINAL_PROBE").is_ok_and(|value| !value.is_empty()) {
        return None;
    }
    imp::probe().or_else(colorfgbg_fallback)
}

/// Derive approximate defaults from `COLORFGBG`, which rxvt-family terminals and
/// some tmux configurations export as `"<fg>;<bg>"` ANSI indices.
///
/// The indices are theme-dependent so the RGB we return is an approximation;
/// it is only ever used to pick a blend direction and a light/dark branch,
/// where being roughly right beats having nothing.
fn colorfgbg_fallback() -> Option<DefaultColors> {
    colorfgbg_from_value(&std::env::var("COLORFGBG").ok()?)
}

/// Parse a `COLORFGBG` value into approximate defaults.
///
/// Split out from [`colorfgbg_fallback`] for the same reason
/// [`color_level_from_env`] is split out of [`stdout_color_level`]: it can then
/// be tested without mutating the process environment, which races across
/// parallel test threads.
fn colorfgbg_from_value(raw: &str) -> Option<DefaultColors> {
    // Nominal appearance of ANSI 0..=15 in a default xterm scheme.
    const ANSI_16: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];

    let mut parts = raw.split(';');
    let fg = parts.next()?.trim().parse::<usize>().ok()?;
    // Some terminals emit "fg;<something>;bg"; the background is always last.
    let bg = raw.rsplit(';').next()?.trim().parse::<usize>().ok()?;
    Some(DefaultColors {
        fg: *ANSI_16.get(fg)?,
        bg: *ANSI_16.get(bg)?,
    })
}

#[cfg(unix)]
mod imp {
    use std::fs::OpenOptions;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    use super::{DefaultColors, colors_from_replies, drain_replies};

    /// Upper bound on how long startup may block waiting for the terminal.
    ///
    /// The queries are answered in well under a millisecond by terminals that
    /// support them; the budget exists for terminals that never answer at all.
    const PROBE_TIMEOUT: Duration = Duration::from_millis(120);

    pub(super) fn probe() -> Option<DefaultColors> {
        // /dev/tty rather than stdin/stdout so the probe still works when
        // either is redirected, and so it is obviously a no-op when there is
        // no controlling terminal at all.
        let mut tty = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;

        let _raw = RawMode::enable(tty.as_raw_fd())?;

        // The trailing DA1 (`ESC [ c`) is a sentinel. Terminals answer it even
        // when they ignore OSC 10/11, so a terminal without OSC support costs
        // one round trip instead of the full timeout.
        tty.write_all(b"\x1b]10;?\x07\x1b]11;?\x07\x1b[c").ok()?;
        tty.flush().ok()?;

        let fd = tty.as_raw_fd();
        let deadline = Instant::now() + PROBE_TIMEOUT;
        let buffer = drain_replies(
            |chunk| tty.read(chunk),
            || {
                let remaining = deadline.saturating_duration_since(Instant::now());
                !remaining.is_zero() && wait_readable(fd, remaining)
            },
        );

        colors_from_replies(&buffer)
    }

    fn wait_readable(fd: i32, timeout: Duration) -> bool {
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: `poll_fd` is a valid, initialised single-element array and
        // `fd` is owned by the caller for the duration of this call.
        unsafe { libc::poll(&mut poll_fd, 1, millis) > 0 }
    }

    /// Puts the terminal in raw mode and restores the previous settings on drop.
    ///
    /// The probe runs before the TUI owns the terminal, so it must leave the
    /// termios state exactly as it found it — including on the early-return
    /// paths above, which is why this is a guard rather than paired calls.
    struct RawMode {
        fd: i32,
        original: libc::termios,
    }

    impl RawMode {
        fn enable(fd: i32) -> Option<Self> {
            // SAFETY: zeroed termios is a valid initial value for tcgetattr to
            // overwrite, and `fd` refers to an open terminal.
            unsafe {
                let mut original: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(fd, &mut original) != 0 {
                    return None;
                }
                let mut raw = original;
                libc::cfmakeraw(&mut raw);
                if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                    return None;
                }
                Some(Self { fd, original })
            }
        }
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            // SAFETY: `self.original` came from tcgetattr on this same fd.
            unsafe {
                libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
            }
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::DefaultColors;

    /// No OSC probe off Unix; callers fall back to modifier-only styling.
    pub(super) fn probe() -> Option<DefaultColors> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_four_digit_osc_background_response() {
        let response = b"\x1b]11;rgb:1c1c/1c1c/1c1c\x07";
        assert_eq!(parse_osc_color(response, 11), Some((28, 28, 28)));
    }

    #[test]
    fn parses_two_digit_and_saturated_components() {
        // A terminal replying with 2-digit components must scale the same way
        // a 4-digit one does: `ff` and `ffff` both mean fully on.
        assert_eq!(
            parse_osc_color(b"\x1b]10;rgb:ff/00/80\x1b\\", 10),
            Some((255, 0, 128))
        );
        assert_eq!(
            parse_osc_color(b"\x1b]10;rgb:ffff/ffff/ffff\x07", 10),
            Some((255, 255, 255))
        );
    }

    #[test]
    fn picks_the_right_response_when_both_are_in_one_buffer() {
        // The probe issues both queries back to back, so a single read usually
        // contains both replies plus the DA1 sentinel.
        let buffer = b"\x1b]10;rgb:ffff/ffff/ffff\x07\x1b]11;rgb:0000/0000/0000\x07\x1b[?62c";
        assert_eq!(parse_osc_color(buffer, 10), Some((255, 255, 255)));
        assert_eq!(parse_osc_color(buffer, 11), Some((0, 0, 0)));
    }

    #[test]
    fn color_hex_digits_do_not_end_the_probe_before_da1() {
        let colors = b"\x1b]10;rgb:cccc/eeee/ffff\x07\x1b]11;rgb:213d/2743/33e7\x07";
        assert!(!contains_da1_response(colors));

        let mut completed = colors.to_vec();
        completed.extend_from_slice(b"\x1b[?62;4c");
        assert!(contains_da1_response(&completed));
    }

    #[test]
    fn rejects_malformed_responses_instead_of_guessing() {
        assert_eq!(parse_osc_color(b"\x1b[?62c", 11), None);
        assert_eq!(parse_osc_color(b"\x1b]11;rgb:zz/00/00\x07", 11), None);
        assert_eq!(parse_osc_color(b"\x1b]11;rgb:1c1c/1c1c\x07", 11), None);
    }

    #[test]
    fn light_and_dark_backgrounds_are_classified_by_luma() {
        assert!(is_light((255, 255, 255)));
        assert!(is_light((253, 246, 227))); // solarized light
        assert!(!is_light((0, 43, 54))); // solarized dark
        assert!(!is_light((40, 42, 54))); // dracula
    }

    #[test]
    fn blending_moves_toward_the_overlay_proportionally() {
        assert_eq!(blend((255, 255, 255), (0, 0, 0), 0.0), (0, 0, 0));
        assert_eq!(blend((255, 255, 255), (0, 0, 0), 1.0), (255, 255, 255));
        assert_eq!(blend((255, 255, 255), (0, 0, 0), 0.2), (51, 51, 51));
    }

    #[test]
    fn blended_tint_stays_close_to_the_background_it_came_from() {
        // The property that makes adaptive backgrounds work: whatever the
        // terminal background is, a low-alpha tint lands near it rather than
        // near some absolute color we chose.
        for bg in [(0, 0, 0), (255, 255, 255), (0, 43, 54), (253, 246, 227)] {
            let tinted = blend((46, 160, 67), bg, 0.18);
            assert!(
                perceptual_distance(tinted, bg) < 25.0,
                "tint drifted too far from {bg:?}"
            );
        }
    }

    #[test]
    fn best_color_degrades_with_the_terminal_color_level() {
        let target = (51, 51, 51);
        assert_eq!(
            best_color_for_level(target, StdoutColorLevel::TrueColor),
            Some(rgb_color(target))
        );
        // 16-color and unknown terminals get nothing, so callers fall back to
        // modifiers rather than painting a fill that may be unreadable.
        assert_eq!(best_color_for_level(target, StdoutColorLevel::Ansi16), None);
        assert_eq!(
            best_color_for_level(target, StdoutColorLevel::Unknown),
            None
        );
    }

    #[test]
    fn indexed_match_never_returns_a_theme_defined_slot() {
        // Indices 0..=15 are redefined by the user's terminal theme, so a
        // "nearest match" onto one of them is unpredictable in practice.
        for target in [(0, 0, 0), (255, 255, 255), (128, 0, 0), (51, 51, 51)] {
            let Some(Color::Indexed(index)) =
                best_color_for_level(target, StdoutColorLevel::Ansi256)
            else {
                panic!("expected an indexed color for {target:?}");
            };
            assert!(index >= 16, "matched theme-defined slot {index}");
        }
    }

    #[test]
    // Naming the expected index is the entire assertion here; this is the one
    // place an indexed color is the subject rather than a mistake.
    #[allow(clippy::disallowed_methods)]
    fn indexed_match_lands_on_a_perceptually_close_entry() {
        // Pure black and pure white are exactly representable in the cube.
        assert_eq!(
            best_color_for_level((0, 0, 0), StdoutColorLevel::Ansi256),
            Some(Color::Indexed(16))
        );
        assert_eq!(
            best_color_for_level((255, 255, 255), StdoutColorLevel::Ansi256),
            Some(Color::Indexed(231))
        );
    }

    #[test]
    fn color_level_reads_capability_hints_in_priority_order() {
        let env = |pairs: Vec<(&'static str, &'static str)>| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| (*value).to_string())
            }
        };

        assert_eq!(
            color_level_from_env(env(vec![("COLORTERM", "truecolor")])),
            StdoutColorLevel::TrueColor
        );
        assert_eq!(
            color_level_from_env(env(vec![("TERM", "xterm-256color")])),
            StdoutColorLevel::Ansi256
        );
        assert_eq!(
            color_level_from_env(env(vec![("TERM", "xterm")])),
            StdoutColorLevel::Ansi16
        );
        assert_eq!(
            color_level_from_env(env(vec![("TERM", "dumb")])),
            StdoutColorLevel::Unknown
        );
        assert_eq!(color_level_from_env(env(vec![])), StdoutColorLevel::Unknown);
    }

    #[test]
    fn no_color_overrides_every_capability_hint() {
        let level = color_level_from_env(|key| match key {
            "NO_COLOR" => Some("1".to_string()),
            "COLORTERM" => Some("truecolor".to_string()),
            _ => None,
        });
        assert_eq!(level, StdoutColorLevel::Unknown);
    }

    /// Build a `var`-style lookup over a fixed set of pairs.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| {
            owned
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn empty_no_color_does_not_suppress_color() {
        // The convention is that `NO_COLOR` opts out only when it is set to a
        // non-empty value; an empty one must not disable an otherwise
        // truecolor terminal.
        assert_eq!(
            color_level_from_env(env_of(&[("NO_COLOR", ""), ("COLORTERM", "truecolor")])),
            StdoutColorLevel::TrueColor
        );
    }

    #[test]
    fn colorterm_matches_case_insensitively_and_accepts_24bit() {
        assert_eq!(
            color_level_from_env(env_of(&[("COLORTERM", "TrueColor")])),
            StdoutColorLevel::TrueColor
        );
        assert_eq!(
            color_level_from_env(env_of(&[("COLORTERM", "24BIT")])),
            StdoutColorLevel::TrueColor
        );
        // An unrecognised COLORTERM must fall through to the TERM hints rather
        // than being treated as a truecolor claim.
        assert_eq!(
            color_level_from_env(env_of(&[("COLORTERM", "yes"), ("TERM", "xterm-256color")])),
            StdoutColorLevel::Ansi256
        );
    }

    #[test]
    fn known_terminal_programs_are_truecolor_without_colorterm() {
        // These export no COLORTERM over SSH or through a scrubbing login
        // shell, which is the whole reason the allowlist exists.
        for program in ["iTerm.app", "WezTerm", "ghostty", "vscode", "Hyper", "rio"] {
            assert_eq!(
                color_level_from_env(env_of(&[("TERM_PROGRAM", program), ("TERM", "xterm")])),
                StdoutColorLevel::TrueColor,
                "{program} should be treated as truecolor"
            );
        }
    }

    #[test]
    fn unlisted_terminal_program_falls_through_to_term() {
        // Apple Terminal genuinely is not truecolor, so it must not be granted
        // one by the allowlist.
        assert_eq!(
            color_level_from_env(env_of(&[
                ("TERM_PROGRAM", "Apple_Terminal"),
                ("TERM", "xterm-256color")
            ])),
            StdoutColorLevel::Ansi256
        );
    }

    #[test]
    fn term_names_that_advertise_direct_color_are_truecolor() {
        for term in ["xterm-direct", "xterm-truecolor", "iterm2-direct"] {
            assert_eq!(
                color_level_from_env(env_of(&[("TERM", term)])),
                StdoutColorLevel::TrueColor,
                "{term} should be treated as truecolor"
            );
        }
    }

    #[test]
    fn unrecognised_term_names_fall_back_to_unknown() {
        // The floor matters more than a guess: an unknown TERM gets modifiers,
        // not a fill that might be invisible.
        for term in ["vt100", "linux", "ansi", "sun"] {
            assert_eq!(
                color_level_from_env(env_of(&[("TERM", term)])),
                StdoutColorLevel::Unknown,
                "{term} should not be assumed to have color"
            );
        }
    }

    #[test]
    fn screen_and_color_suffixed_terms_get_sixteen_colors() {
        assert_eq!(
            color_level_from_env(env_of(&[("TERM", "screen")])),
            StdoutColorLevel::Ansi16
        );
        assert_eq!(
            color_level_from_env(env_of(&[("TERM", "rxvt-unicode-color")])),
            StdoutColorLevel::Ansi16
        );
        // `screen-256color` contains both "256" and "screen"; the richer hint
        // has to win, so ordering inside the resolver is load-bearing.
        assert_eq!(
            color_level_from_env(env_of(&[("TERM", "screen-256color")])),
            StdoutColorLevel::Ansi256
        );
    }

    #[test]
    fn recording_default_colors_is_idempotent() {
        // A second probe (after a suspend/resume, say) must not panic and must
        // not be able to change what the session already resolved. Asserting
        // against the first observed value rather than a literal keeps this
        // independent of whichever test in the binary touched the OnceLock
        // first.
        let first = default_colors();
        set_default_colors(Some(DefaultColors {
            fg: (1, 2, 3),
            bg: (4, 5, 6),
        }));
        set_default_colors(None);
        assert_eq!(default_colors(), first, "the first recorded value wins");
    }

    #[test]
    fn da1_is_recognised_in_its_single_byte_csi_form() {
        // A terminal may answer with the C1 control byte 0x9b instead of the
        // two-byte `ESC [` sequence.
        assert!(contains_da1_response(b"\x9b?62c"));
        assert!(contains_da1_response(b"\x1b]11;rgb:00/00/00\x07\x9b?1;2c"));
    }

    #[test]
    fn da1_is_recognised_with_intermediate_bytes() {
        // CSI allows 0x20..=0x2f intermediates between the parameters and the
        // final byte; skipping them would miss the terminator.
        assert!(contains_da1_response(b"\x1b[?62;1 c"));
        assert!(contains_da1_response(b"\x1b[?62;1!c"));
    }

    #[test]
    fn non_da1_csi_sequences_are_skipped_rather_than_ending_the_scan() {
        // An SGR reset ahead of the real answer must not stop the search, or
        // the rest of the reply is left in the input queue and shows up as
        // stray keystrokes once the TUI starts.
        assert!(contains_da1_response(b"\x1b[0m\x1b[?62c"));
        assert!(!contains_da1_response(b"\x1b[0m"));
        assert!(!contains_da1_response(b"\x1b[2J\x1b[H"));
    }

    #[test]
    fn incomplete_or_empty_buffers_are_not_mistaken_for_da1() {
        assert!(!contains_da1_response(b""));
        assert!(!contains_da1_response(b"\x1b"));
        assert!(!contains_da1_response(b"\x1b["));
        assert!(!contains_da1_response(b"\x1b[?62"));
        // A bare `c` is a hex digit in an OSC color reply, not a terminator.
        assert!(!contains_da1_response(b"\x1b]11;rgb:cccc/cccc/cccc\x07"));
    }

    #[test]
    fn osc_components_scale_from_one_and_three_digit_widths() {
        // Terminals are free to answer at any width from 1 to 4 hex digits.
        assert_eq!(
            parse_osc_color(b"\x1b]11;rgb:f/0/8\x07", 11),
            Some((255, 0, 136))
        );
        assert_eq!(
            parse_osc_color(b"\x1b]11;rgb:fff/000/800\x07", 11),
            Some((255, 0, 128))
        );
    }

    #[test]
    fn osc_parse_rejects_the_wrong_code_and_missing_prefix() {
        // Asking for the background must not accidentally read the foreground
        // reply that shares the buffer.
        assert_eq!(parse_osc_color(b"\x1b]10;rgb:ff/ff/ff\x07", 11), None);
        // Some terminals answer `#rrggbb`; we do not claim to parse that.
        assert_eq!(parse_osc_color(b"\x1b]11;#1c1c1c\x07", 11), None);
        // Wider than 4 hex digits is out of spec and must not be truncated
        // into a plausible-looking color.
        assert_eq!(parse_osc_color(b"\x1b]11;rgb:fffff/00/00\x07", 11), None);
        assert_eq!(parse_osc_color(b"", 11), None);
    }

    #[test]
    fn colorfgbg_maps_ansi_indices_to_nominal_rgb() {
        // rxvt and tmux export "<fg>;<bg>"; light-on-dark is the common case.
        assert_eq!(
            colorfgbg_from_value("15;0"),
            Some(DefaultColors {
                fg: (255, 255, 255),
                bg: (0, 0, 0)
            })
        );
        assert_eq!(
            colorfgbg_from_value("0;15"),
            Some(DefaultColors {
                fg: (0, 0, 0),
                bg: (255, 255, 255)
            })
        );
    }

    #[test]
    fn colorfgbg_takes_the_background_from_the_last_field() {
        // Some terminals emit a three-field "fg;<something>;bg" form, where
        // reading the second field would pick up the wrong color entirely.
        assert_eq!(
            colorfgbg_from_value("15;default;0"),
            Some(DefaultColors {
                fg: (255, 255, 255),
                bg: (0, 0, 0)
            })
        );
        assert_eq!(
            colorfgbg_from_value(" 7 ; 0 "),
            Some(DefaultColors {
                fg: (229, 229, 229),
                bg: (0, 0, 0)
            })
        );
    }

    #[test]
    fn colorfgbg_rejects_values_it_cannot_trust() {
        // Anything unparseable yields None so the caller uses modifiers rather
        // than tinting against a background that may not exist.
        assert_eq!(colorfgbg_from_value(""), None);
        assert_eq!(colorfgbg_from_value("default;default"), None);
        // Out of the 16-entry table: an index we have no nominal RGB for.
        assert_eq!(colorfgbg_from_value("15;16"), None);
        assert_eq!(colorfgbg_from_value("-1;0"), None);
        assert_eq!(colorfgbg_from_value("15;"), None);
    }

    #[test]
    fn colorfgbg_without_a_separator_reads_one_field_as_both() {
        // A separator-less value makes the first and last field the same, so
        // foreground and background come back identical. Documented rather
        // than rejected: no terminal emits this, and a matching pair is inert
        // downstream — the blend has nowhere to move and the light/dark branch
        // still resolves.
        assert_eq!(
            colorfgbg_from_value("15"),
            Some(DefaultColors {
                fg: (255, 255, 255),
                bg: (255, 255, 255)
            })
        );
    }

    #[test]
    fn colorfgbg_classification_matches_the_light_dark_branch() {
        // The only two things this fallback feeds are a blend direction and a
        // light/dark decision, so that is what is worth asserting.
        let dark = colorfgbg_from_value("15;0").expect("parses");
        let light = colorfgbg_from_value("0;15").expect("parses");
        assert!(!is_light(dark.bg));
        assert!(is_light(light.bg));
    }

    #[test]
    fn xterm_cube_and_grayscale_ramp_match_the_specification() {
        assert_eq!(xterm_rgb(16), (0, 0, 0));
        assert_eq!(xterm_rgb(231), (255, 255, 255));
        assert_eq!(xterm_rgb(196), (255, 0, 0));
        // The grayscale ramp starts at 8 and steps by 10.
        assert_eq!(xterm_rgb(232), (8, 8, 8));
        assert_eq!(xterm_rgb(255), (238, 238, 238));
    }

    #[test]
    fn perceptual_distance_is_zero_for_identical_colors_and_symmetric() {
        assert_eq!(perceptual_distance((18, 24, 30), (18, 24, 30)), 0.0);
        let forward = perceptual_distance((0, 0, 0), (255, 255, 255));
        let backward = perceptual_distance((255, 255, 255), (0, 0, 0));
        assert!((forward - backward).abs() < f32::EPSILON);
        // Black to white is the largest lightness gap there is.
        assert!(forward > perceptual_distance((0, 0, 0), (128, 128, 128)));
    }

    #[test]
    fn blend_clamps_alpha_outside_the_unit_range() {
        assert_eq!(blend((255, 0, 0), (0, 0, 255), -1.0), (0, 0, 255));
        assert_eq!(blend((255, 0, 0), (0, 0, 255), 2.0), (255, 0, 0));
    }

    /// A `read`-shaped closure that replays a fixed script, then reports EOF.
    fn scripted_reads(
        steps: Vec<std::io::Result<&'static [u8]>>,
    ) -> impl FnMut(&mut [u8]) -> std::io::Result<usize> {
        let mut steps = steps.into_iter();
        move |chunk: &mut [u8]| match steps.next() {
            Some(Ok(bytes)) => {
                chunk[..bytes.len()].copy_from_slice(bytes);
                Ok(bytes.len())
            }
            Some(Err(err)) => Err(err),
            None => Ok(0),
        }
    }

    const OSC_REPLIES: &[u8] = b"\x1b]10;rgb:ffff/ffff/ffff\x07\x1b]11;rgb:0000/0000/0000\x07";

    #[test]
    fn draining_stops_as_soon_as_the_da1_sentinel_arrives() {
        // The read after the sentinel must never happen: anything past it is
        // the user's own typing, and consuming it swallows a keystroke.
        let mut reads = 0;
        let buffer = drain_replies(
            |chunk| {
                reads += 1;
                let bytes: &[u8] = match reads {
                    1 => OSC_REPLIES,
                    2 => b"\x1b[?62;4c",
                    _ => panic!("read past the DA1 sentinel"),
                };
                chunk[..bytes.len()].copy_from_slice(bytes);
                Ok(bytes.len())
            },
            || true,
        );
        assert_eq!(reads, 2);
        assert_eq!(
            colors_from_replies(&buffer),
            Some(DefaultColors {
                fg: (255, 255, 255),
                bg: (0, 0, 0)
            })
        );
    }

    #[test]
    fn draining_reassembles_a_reply_split_across_reads() {
        // Terminals are under no obligation to deliver a reply in one chunk,
        // and a byte-at-a-time split is the worst realistic case.
        let mut pending: Vec<&'static [u8]> = vec![
            b"\x1b]10;rgb:1111/2222/3333\x07",
            b"\x1b]11;rgb:4444/5555/6666\x07",
            b"\x1b[",
            b"?62",
            b"c",
        ];
        pending.reverse();
        let buffer = drain_replies(
            |chunk| match pending.pop() {
                Some(bytes) => {
                    chunk[..bytes.len()].copy_from_slice(bytes);
                    Ok(bytes.len())
                }
                None => Ok(0),
            },
            || true,
        );
        assert!(pending.is_empty(), "should have consumed the whole script");
        assert_eq!(
            colors_from_replies(&buffer),
            Some(DefaultColors {
                fg: (17, 34, 51),
                bg: (68, 85, 102)
            })
        );
    }

    #[test]
    fn draining_retries_after_an_interrupted_read() {
        // A signal landing mid-probe is not the terminal declining to answer,
        // so EINTR has to be retried rather than treated as the end.
        let buffer = drain_replies(
            scripted_reads(vec![
                Err(std::io::Error::from(std::io::ErrorKind::Interrupted)),
                Ok(OSC_REPLIES),
                Err(std::io::Error::from(std::io::ErrorKind::Interrupted)),
                Ok(b"\x1b[?62c"),
            ]),
            || true,
        );
        assert!(colors_from_replies(&buffer).is_some());
    }

    #[test]
    fn draining_gives_up_on_a_real_read_error_but_keeps_what_it_has() {
        let buffer = drain_replies(
            scripted_reads(vec![
                Ok(OSC_REPLIES),
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
            ]),
            || true,
        );
        // Both colors did arrive before the failure, so the partial read is
        // still usable — the error ends the wait, it does not void the answer.
        assert_eq!(
            colors_from_replies(&buffer),
            Some(DefaultColors {
                fg: (255, 255, 255),
                bg: (0, 0, 0)
            })
        );
    }

    #[test]
    fn draining_ends_at_end_of_input_without_a_sentinel() {
        // A terminal that answers the color queries but ignores DA1 still has
        // to terminate the loop when the source runs dry.
        let buffer = drain_replies(scripted_reads(vec![Ok(OSC_REPLIES)]), || true);
        assert!(colors_from_replies(&buffer).is_some());
    }

    #[test]
    fn draining_returns_nothing_when_the_budget_is_already_spent() {
        // The timeout path: a terminal that never answers must cost the
        // budget and nothing more, and must not yield a guessed color.
        let mut reads = 0;
        let buffer = drain_replies(
            |_chunk| {
                reads += 1;
                Ok(0)
            },
            || false,
        );
        assert_eq!(reads, 0, "must not read once the budget is gone");
        assert!(buffer.is_empty());
        assert_eq!(colors_from_replies(&buffer), None);
    }

    #[test]
    fn draining_stops_when_the_budget_runs_out_partway_through() {
        // Readiness is re-checked every iteration, so a terminal that dribbles
        // out a reply cannot hold the probe open past its deadline.
        let mut allowed = 2;
        let buffer = drain_replies(
            scripted_reads(vec![
                Ok(b"\x1b]10;rgb:ffff/ffff/ffff\x07"),
                Ok(b"\x1b]11;rgb:0000/"),
                Ok(b"0000/0000\x07\x1b[?62c"),
            ]),
            || {
                allowed -= 1;
                allowed >= 0
            },
        );
        // Only the first two chunks got through, so the background reply is
        // still truncated and the pair must be refused rather than guessed.
        assert_eq!(colors_from_replies(&buffer), None);
    }

    #[test]
    fn a_half_answered_probe_is_refused_rather_than_half_guessed() {
        // Assuming a background when only the foreground came back is exactly
        // the "assume dark" failure the module is built to avoid.
        assert_eq!(colors_from_replies(b"\x1b]10;rgb:ffff/ffff/ffff\x07"), None);
        assert_eq!(colors_from_replies(b"\x1b]11;rgb:0000/0000/0000\x07"), None);
        assert_eq!(colors_from_replies(b""), None);
        assert_eq!(colors_from_replies(b"\x1b[?62c"), None);
    }

    #[test]
    fn is_light_sits_on_the_rec601_midpoint() {
        // Mid grey lands just above the threshold; one step down does not.
        assert!(is_light((129, 129, 129)));
        assert!(!is_light((128, 128, 128)));
        // Luma is not a plain average: full green reads light, full blue dark.
        assert!(is_light((0, 255, 0)));
        assert!(!is_light((0, 0, 255)));
    }
}
