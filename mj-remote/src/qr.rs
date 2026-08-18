//! Terminal QR rendering shared by the remote-viewer login banner and the
//! ACP elicitation URL modal. Encodes arbitrary text into a half-block ASCII
//! QR (two module rows per text row) framed by the spec-required quiet zone.

use anyhow::{Context, Result};
use qrcode::QrCode;
use qrcode::types::{Color, EcLevel};

/// Render `data` as a half-block QR code string. Each line is terminated by
/// `\n`; callers that need individual rows can split on it. The four-module
/// quiet zone on every side keeps the code scannable against any background.
pub fn render_qr(data: &str) -> Result<String> {
    const QUIET_ZONE_MODULES: usize = 4;

    let qr = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::L)
        .context("encode QR code")?;
    let mut output = String::new();
    let qr_width = qr.width();
    let total_width = qr_width + QUIET_ZONE_MODULES * 2;
    let total_height = qr_width + QUIET_ZONE_MODULES * 2;

    for y in (0..total_height).step_by(2) {
        for x in 0..total_width {
            let top = qr_module_is_dark(&qr, x, y, QUIET_ZONE_MODULES);
            let bottom = qr_module_is_dark(&qr, x, y + 1, QUIET_ZONE_MODULES);
            let ch = match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            };
            output.push(ch);
        }
        output.push('\n');
    }
    Ok(output)
}

fn qr_module_is_dark(qr: &QrCode, x: usize, y: usize, quiet_zone: usize) -> bool {
    let Some(qr_x) = x.checked_sub(quiet_zone) else {
        return false;
    };
    let Some(qr_y) = y.checked_sub(quiet_zone) else {
        return false;
    };
    qr_x < qr.width() && qr_y < qr.width() && qr[(qr_x, qr_y)] == Color::Dark
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_qr_produces_visible_blocks() {
        let rendered = render_qr("https://localhost:11921/#token=test").expect("qr");
        assert!(rendered.contains('█') || rendered.contains('▀') || rendered.contains('▄'));
        assert!(rendered.contains('\n'));
    }

    #[test]
    fn render_qr_includes_quiet_zone() {
        let rendered = render_qr("https://localhost:11921/auth/login?code=123456").expect("qr");
        let lines = rendered.lines().collect::<Vec<_>>();

        assert!(lines.len() > 4);
        assert!(lines[0].chars().all(|ch| ch == ' '));
        assert!(lines[1].chars().all(|ch| ch == ' '));
        for line in &lines {
            assert!(line.starts_with("    "));
            assert!(line.ends_with("    "));
        }
    }
}
