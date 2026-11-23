//! Export classifier inputs or review evaluation rows with the configured utility model.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::num::NonZeroUsize;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use futures::{StreamExt, stream};
use mj_core::config::{Config, HarnessKind};
use mj_core::guardian::{
    GuardianReviewRequest, collect_project_instructions, compact_action_packet, rich_scene_packet,
    wolf_scene_packet_with_budget,
};
use mj_import::utility_llm::{GuardianUtilityOutcome, review_guardian_with_utility};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Deserialize)]
struct CorpusRow {
    id: String,
    harness: HarnessKind,
    session_id: String,
    request_id: Option<String>,
    action: Option<Value>,
    message: Option<String>,
    tool_title: Option<String>,
    tool_kind: Option<String>,
    raw_input: Option<Value>,
    cwd: String,
    user_instructions: Vec<String>,
    recent_transcript: String,
}

impl CorpusRow {
    fn into_request(self, with_agents_md: bool) -> Result<GuardianReviewRequest> {
        let mut action = match (self.action, self.message, self.tool_title) {
            (Some(action), _, _) => action,
            // The recorded ACP form preserves the tool description and command
            // preview. No raw tool call was recorded in this corpus.
            (None, Some(message), _) => json!({"permission_request": message}),
            (None, None, Some(tool)) => json!({
                "tool": tool,
                "kind": self.tool_kind,
                "input": self.raw_input,
            }),
            _ => bail!("row {} has no recorded action", self.id),
        };
        let fields = action
            .as_object_mut()
            .context("planned action must be a JSON object")?;
        let project_instructions = if with_agents_md {
            collect_project_instructions(Path::new(&self.cwd))
        } else {
            None
        };
        fields.entry("cwd").or_insert(Value::String(self.cwd));
        let tool_call_id = self.request_id.unwrap_or_else(|| self.id.clone());
        Ok(GuardianReviewRequest {
            request_id: self.id,
            harness: self.harness,
            session_id: self.session_id,
            tool_call_id,
            action,
            user_instructions: self.user_instructions,
            // The extractor retains at most six user messages and does not
            // record whether the history preceding the retained ones is complete.
            user_instructions_omitted: true,
            project_instructions,
            trusted_user_inputs: Vec::new(),
            prior_reviews: Vec::new(),
            retry_reason: None,
            transcript: self.recent_transcript,
        })
    }
}

fn read_jsonl<T: DeserializeOwned>(path: &Path) -> Result<impl Iterator<Item = Result<T>>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let path = path.to_owned();
    Ok(BufReader::new(file)
        .lines()
        .enumerate()
        .map(move |(index, line)| {
            let parse = || -> Result<T> { Ok(serde_json::from_str(&line?)?) };
            parse().with_context(|| format!("read {} line {}", path.display(), index + 1))
        }))
}

fn write_row(writer: &mut impl Write, row: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *writer, row)?;
    writer.write_all(b"\n")?;
    Ok(())
}

fn canonicalize(input: &Path, output: &Path) -> Result<()> {
    let rows = read_jsonl::<CorpusRow>(input)?;
    let mut writer = BufWriter::new(File::create(output)?);
    for row in rows {
        let request = row?.into_request(false)?;
        write_row(
            &mut writer,
            &json!({
                "id": request.request_id,
                "compact_packet": compact_action_packet(&request),
                "rich_scene_4k": rich_scene_packet(&request, 4_096),
                "rich_scene_12k": rich_scene_packet(&request, 12_288),
                "wolf_scene_1k": wolf_scene_packet_with_budget(&request, 1_000),
                "wolf_scene_1500": wolf_scene_packet_with_budget(&request, 1_500),
                "wolf_scene_2k": wolf_scene_packet_with_budget(&request, 2_000),
                "request_text": request.request_text(),
            }),
        )?;
    }
    writer.flush()?;
    Ok(())
}

#[derive(Deserialize)]
struct OutputId {
    id: String,
}

#[derive(Serialize)]
struct ReviewRow {
    id: String,
    #[serde(flatten)]
    review: GuardianUtilityOutcome,
}

async fn stage2_utility(
    input: &Path,
    output: &Path,
    concurrency: usize,
    with_agents_md: bool,
) -> Result<()> {
    let config = Config::load().context("load Mjolnir configuration")?;
    let mut completed = if output.exists() {
        read_jsonl::<OutputId>(output)?
            .map(|row| row.map(|row| row.id))
            .collect::<Result<HashSet<_>>>()?
    } else {
        HashSet::new()
    };
    let rows = read_jsonl::<CorpusRow>(input)?.filter_map(|row| match row {
        Ok(row) if completed.insert(row.id.clone()) => Some(row.into_request(with_agents_md)),
        Ok(_) => None,
        Err(error) => Some(Err(error)),
    });
    let cancel = CancellationToken::new();
    let _cancel_on_exit = cancel.clone().drop_guard();
    let mut results = stream::iter(rows)
        .map(|request| {
            let config = &config;
            let cancel = cancel.clone();
            async move {
                let request = request?;
                let review = review_guardian_with_utility(config, &request, cancel).await;
                Ok::<_, anyhow::Error>(ReviewRow {
                    id: request.request_id,
                    review,
                })
            }
        })
        .buffer_unordered(concurrency);
    let mut writer = BufWriter::new(OpenOptions::new().create(true).append(true).open(output)?);
    while let Some(result) = results.next().await {
        write_row(&mut writer, &result?)?;
        writer.flush()?;
    }
    Ok(())
}

const USAGE: &str = "usage: guardian_eval_tool <canonicalize|stage2-utility> <in.jsonl> <out.jsonl> [--concurrency N] [--with-agents-md]";

#[tokio::main]
async fn main() -> Result<()> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let (command, input, output, rest) = match args.as_slice() {
        [command, input, output, rest @ ..] => (command, Path::new(input), Path::new(output), rest),
        _ => bail!("{USAGE}"),
    };
    if output.exists() {
        ensure!(
            input.canonicalize()? != output.canonicalize()?,
            "input and output must be separate files"
        );
    }
    match (command.to_str(), rest) {
        (Some("canonicalize"), []) => canonicalize(input, output),
        (Some("stage2-utility"), mut rest) => {
            let mut concurrency = None;
            let mut with_agents_md = false;
            while !rest.is_empty() {
                match rest {
                    [flag, tail @ ..] if flag == "--with-agents-md" && !with_agents_md => {
                        with_agents_md = true;
                        rest = tail;
                    }
                    [flag, value, tail @ ..]
                        if flag == "--concurrency" && concurrency.is_none() =>
                    {
                        concurrency = Some(
                            value
                                .to_str()
                                .context("concurrency must be a positive integer")?
                                .parse::<NonZeroUsize>()
                                .context("concurrency must be a positive integer")?,
                        );
                        rest = tail;
                    }
                    _ => bail!("{USAGE}"),
                }
            }
            stage2_utility(
                input,
                output,
                concurrency.map_or(1, NonZeroUsize::get),
                with_agents_md,
            )
            .await
        }
        _ => bail!("{USAGE}"),
    }
}
