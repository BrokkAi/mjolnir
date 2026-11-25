use super::*;
use chrono::{Local, TimeZone};
use croner::{
    Cron,
    parser::{CronParser, Seconds, Year},
};

fn parse(expression: &str) -> Result<Cron> {
    anyhow::ensure!(
        expression.split_whitespace().count() == 5,
        "cron requires five fields"
    );
    Ok(CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build()
        .parse(expression)?)
}

#[derive(serde::Serialize)]
pub(super) struct Job {
    id: String,
    cron: String,
    prompt: String,
    recurring: bool,
    next_fire_at_ms: i64,
    expires_at_ms: Option<i64>,
    #[serde(skip)]
    pub task: tokio::task::AbortHandle,
}
impl Job {
    pub fn view(&self) -> Value {
        let mut value = serde_json::to_value(self).unwrap();
        value["next_fire_at_local"] = json!(
            Local
                .timestamp_millis_opt(self.next_fire_at_ms)
                .single()
                .map(|d| d.to_rfc3339())
        );
        value
    }
}
fn arm(s: Arc<Service>, id: String) -> tokio::task::AbortHandle {
    mj_core::runtime::spawn(async move {
        loop {
            let until = {
                let jobs = s.jobs.lock().unwrap();
                let Some(job) = jobs.get(&id) else { break };
                job.expires_at_ms.map_or(job.next_fire_at_ms, |e| e.min(job.next_fire_at_ms))
            };
            tokio::select! {
                _ = s.cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_millis((until - chrono::Utc::now().timestamp_millis()).max(0) as u64)) => {}
            }
            let now = chrono::Utc::now().timestamp_millis();
            if now < until { continue; }
            let retry = {
                let mut jobs = s.jobs.lock().unwrap();
                let Some(job) = jobs.get_mut(&id) else { break };
                let expired = job.expires_at_ms.is_some_and(|e| now >= e);
                if !expired && !s.deliver_wake_with_key(
                    &format!("claude-cron-{}-{}", job.id, job.next_fire_at_ms),
                    format!("[mj-claude cron {} ({})]\n{}", job.id, job.cron, job.prompt),
                ) { true } else {
                    let next = if !expired && job.recurring {
                        parse(&job.cron).and_then(|c| Ok(c.find_next_occurrence(&Local::now(), false)?)).ok()
                    } else { None };
                    if let Some(next) = next { job.next_fire_at_ms = next.timestamp_millis(); }
                    else { jobs.remove(&id); break; }
                    false
                }
            };
            if retry {
                tokio::select! {
                    _ = s.cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
            }
        }
    }).abort_handle()
}
pub(super) async fn create(s: &Arc<Service>, a: Value) -> Result<Value> {
    let cron = required(&a, "cron")?;
    let prompt = required(&a, "prompt")?;
    let schedule = parse(&cron)?;
    let recurring = a["recurring"].as_bool().unwrap_or(true);
    let next_fire_at_ms = schedule
        .find_next_occurrence(&Local::now(), false)?
        .timestamp_millis();
    let id = id();
    // The map lock publishes the job and its task together before the task can read it.
    let mut jobs = s.jobs.lock().unwrap();
    let task = arm(s.clone(), id.clone());
    let job = Job {
        id: id.clone(),
        cron,
        prompt,
        recurring,
        next_fire_at_ms,
        expires_at_ms: recurring.then(|| chrono::Utc::now().timestamp_millis() + 7 * 86_400_000),
        task,
    };
    let view = job.view();
    jobs.insert(id, job);
    Ok(view)
}
pub(super) async fn delete(s: &Arc<Service>, id: &str) -> Result<Value> {
    let job = s
        .jobs
        .lock()
        .unwrap()
        .remove(id)
        .context("unknown cron job")?;
    job.task.abort();
    Ok(json!({"id":id,"deleted":true}))
}
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, NaiveDate};
    #[tokio::test]
    async fn claude_mcp_cron_firing_and_deletion() {
        let root = tempfile::tempdir().unwrap();
        let relay = Arc::new(RelayCell::new(
            crate::relay::DurableRelay::open(root.path(), "cron-test", "test").unwrap(),
        ));
        let (tx, mut rx) = mpsc::channel(10);
        let guard = start_claude_mcp_service(
            root.path(),
            root.path().into(),
            BTreeMap::new(),
            HarnessKind::Codex,
            relay,
            tx,
            &tokio::runtime::Handle::current(),
        )
        .unwrap();
        for recurring in [false, true] {
            let view = create(
                &guard.service,
                json!({"cron":"* * * * *","prompt":"fire","recurring":recurring}),
            )
            .await
            .unwrap();
            let id = view["id"].as_str().unwrap();
            {
                let mut jobs = guard.service.jobs.lock().unwrap();
                let job = jobs.get_mut(id).unwrap();
                assert_eq!(job.expires_at_ms.is_some(), recurring);
                job.task.abort();
                job.next_fire_at_ms = chrono::Utc::now().timestamp_millis() - 1;
                job.task = arm(guard.service.clone(), id.into());
            }
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            if recurring {
                assert!(guard.service.jobs.lock().unwrap().contains_key(id));
                delete(&guard.service, id).await.unwrap();
            }
            assert!(guard.service.jobs.lock().unwrap().is_empty());
        }
    }
    #[test]
    fn cron_calendar_rules() {
        let at = Local.with_ymd_and_hms(2025, 3, 1, 0, 0, 0).unwrap();
        assert_eq!(
            parse("0 0 29 2 *")
                .unwrap()
                .find_next_occurrence(&at, false)
                .unwrap()
                .date_naive(),
            NaiveDate::from_ymd_opt(2028, 2, 29).unwrap()
        );
        let at = Local.with_ymd_and_hms(2026, 11, 25, 0, 0, 0).unwrap();
        for dow in [0, 7] {
            assert_eq!(
                parse(&format!("0 0 1 * {dow}"))
                    .unwrap()
                    .find_next_occurrence(&at, false)
                    .unwrap()
                    .weekday()
                    .num_days_from_sunday(),
                0
            );
        }
        for bad in [
            "* * *",
            "* * * * * *",
            "* * * * * * *",
            "*/0 * * * *",
            "60 * * * *",
            "* * 0 * *",
            "* * * * 8",
        ] {
            assert!(parse(bad).is_err());
        }
    }
}
