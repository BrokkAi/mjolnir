/// Convert a ratatui buffer to text rows without its right-edge padding.
pub fn buffer_lines(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
    let lines = (buffer.area.y..buffer.area.bottom())
        .map(|y| {
            (buffer.area.x..buffer.area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end_matches(' ')
                .to_owned()
        })
        .collect::<Vec<_>>();
    #[cfg(test)]
    let lines = lines.into_iter().map(normalize_clocked_activity).collect();
    lines
}

#[cfg(test)]
fn normalize_clocked_activity(line: String) -> String {
    if !line.contains("🎙︎") && !line.contains("Reviewing") {
        return line;
    }

    let characters = line.chars().collect::<Vec<_>>();
    let mut normalized = String::new();
    let mut index = 0;
    while index < characters.len() {
        if matches!(characters[index], '·' | '∙' | '•' | '●') {
            let start = index;
            while index < characters.len() && matches!(characters[index], '·' | '∙' | '•' | '●')
            {
                index += 1;
            }
            let run_len = index - start;
            if run_len > 1 {
                normalized.push('●');
                for _ in 1..run_len {
                    normalized.push('·');
                }
            } else {
                normalized.push('·');
            }
        } else {
            normalized.push(characters[index]);
            index += 1;
        }
    }
    normalized
}
