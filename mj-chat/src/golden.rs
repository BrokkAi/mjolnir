/// Convert a ratatui buffer to text rows without its right-edge padding.
pub fn buffer_lines(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
    (buffer.area.y..buffer.area.bottom())
        .map(|y| {
            (buffer.area.x..buffer.area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end_matches(' ')
                .to_owned()
        })
        .collect()
}
