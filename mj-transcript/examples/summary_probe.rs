//! Offline diagnostic: render a canonical snapshot through the production summary.
use std::io::Read;
fn main() -> anyhow::Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let snapshot: mj_core::archive::CanonicalSessionSnapshot = serde_json::from_str(&input)?;
    let summary = mj_transcript::summary::TranscriptSummary::from_snapshot(&snapshot);
    println!("{}", serde_json::to_string(&summary.render(48 * 1024))?);
    Ok(())
}
