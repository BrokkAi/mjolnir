//! SVG rendering for terminal UI documentation images.

use std::fmt::Write as _;

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

const CELL_WIDTH: u16 = 9;
const CELL_HEIGHT: u16 = 18;
const PADDING: u16 = 14;
const TERMINAL_BACKGROUND: &str = "#0f1214";

pub(crate) fn buffer_svg(buffer: &Buffer, title: &str, description: &str) -> String {
    let width = buffer.area.width * CELL_WIDTH + PADDING * 2;
    let height = buffer.area.height * CELL_HEIGHT + PADDING * 2;
    let mut svg = String::new();
    writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img" aria-labelledby="title description">"#
    )
    .unwrap();
    writeln!(svg, "  <title id=\"title\">{}</title>", xml_escape(title)).unwrap();
    writeln!(
        svg,
        "  <desc id=\"description\">{}</desc>",
        xml_escape(description)
    )
    .unwrap();
    writeln!(
        svg,
        "  <rect width=\"100%\" height=\"100%\" rx=\"8\" fill=\"{TERMINAL_BACKGROUND}\"/>"
    )
    .unwrap();
    writeln!(
        svg,
        "  <g font-family=\"Source Code Pro, JetBrains Mono, Menlo, Consolas, monospace\" font-size=\"15\" font-variant-ligatures=\"none\">"
    )
    .unwrap();

    for y in buffer.area.y..buffer.area.bottom() {
        let background_at = |x| {
            let cell = &buffer[(x, y)];
            if cell.modifier.contains(Modifier::REVERSED) {
                cell.fg
            } else {
                cell.bg
            }
        };
        let mut start = buffer.area.x;
        while start < buffer.area.right() {
            let background = background_at(start);
            let mut end = start + 1;
            while end < buffer.area.right() && background_at(end) == background {
                end += 1;
            }
            if background != Color::Reset {
                writeln!(svg,
                    "    <rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{CELL_HEIGHT}\" fill=\"{}\"/>",
                    PADDING + (start - buffer.area.x) * CELL_WIDTH,
                    PADDING + (y - buffer.area.y) * CELL_HEIGHT,
                    (end - start) * CELL_WIDTH,
                    color_hex(background, TERMINAL_BACKGROUND),
                ).unwrap();
            }
            start = end;
        }
        for x in buffer.area.x..buffer.area.right() {
            let cell = &buffer[(x, y)];
            let reversed = cell.modifier.contains(Modifier::REVERSED);
            let foreground = if reversed { cell.bg } else { cell.fg };
            let draw_x = PADDING + (x - buffer.area.x) * CELL_WIDTH;
            let draw_y = PADDING + (y - buffer.area.y) * CELL_HEIGHT;

            let symbol = cell.symbol();
            if symbol.trim().is_empty() {
                continue;
            }
            let mut attributes = String::new();
            if cell.modifier.contains(Modifier::BOLD) {
                attributes.push_str(" font-weight=\"700\"");
            }
            if cell.modifier.contains(Modifier::DIM) {
                attributes.push_str(" opacity=\"0.62\"");
            }
            if cell.modifier.contains(Modifier::ITALIC) {
                attributes.push_str(" font-style=\"italic\"");
            }
            if cell.modifier.contains(Modifier::UNDERLINED) {
                attributes.push_str(" text-decoration=\"underline\"");
            }
            writeln!(
                svg,
                "    <text x=\"{draw_x}\" y=\"{}\" fill=\"{}\"{attributes}>{}</text>",
                draw_y + 14,
                color_hex(foreground, "#efede8"),
                xml_escape(symbol)
            )
            .unwrap();
        }
    }
    svg.push_str("  </g>\n</svg>\n");
    svg
}

// Captures reproduce the terminal's rendered colors exactly, including the
// standard indexed color cube; this encoder does not choose live UI colors.
#[allow(clippy::disallowed_methods)]
fn color_hex(color: Color, fallback: &str) -> String {
    match color {
        Color::Rgb(red, green, blue) => return format!("#{red:02x}{green:02x}{blue:02x}"),
        Color::Indexed(index) => {
            return match index {
                0..=15 => color_hex(
                    [
                        Color::Black,
                        Color::Red,
                        Color::Green,
                        Color::Yellow,
                        Color::Blue,
                        Color::Magenta,
                        Color::Cyan,
                        Color::Gray,
                        Color::DarkGray,
                        Color::LightRed,
                        Color::LightGreen,
                        Color::LightYellow,
                        Color::LightBlue,
                        Color::LightMagenta,
                        Color::LightCyan,
                        Color::White,
                    ][usize::from(index)],
                    fallback,
                ),
                16..=231 => {
                    let cube = index - 16;
                    let level = |value: u8| if value == 0 { 0 } else { 55 + value * 40 };
                    color_hex(
                        Color::Rgb(level(cube / 36), level((cube / 6) % 6), level(cube % 6)),
                        fallback,
                    )
                }
                _ => {
                    let gray = 8 + (index - 232) * 10;
                    color_hex(Color::Rgb(gray, gray, gray), fallback)
                }
            };
        }
        Color::Reset => fallback,
        Color::Black => "#09070e",
        Color::Red => "#ff6b6b",
        Color::Green => "#64d98b",
        Color::Yellow => "#f2c94c",
        Color::Blue => "#69a7ff",
        Color::Magenta => "#c792ff",
        Color::Cyan => "#70d7e8",
        Color::Gray => "#b8adc9",
        Color::DarkGray => "#71677f",
        Color::LightRed => "#ff9292",
        Color::LightGreen => "#8ee8aa",
        Color::LightYellow => "#ffe184",
        Color::LightBlue => "#9bc4ff",
        Color::LightMagenta => "#d9b5ff",
        Color::LightCyan => "#a5ecf5",
        Color::White => "#f4f0fa",
    }
    .to_owned()
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
