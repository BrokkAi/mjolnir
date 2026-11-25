use super::*;
use chrono::{DateTime, Datelike, Local, TimeZone};
use serde::{Deserialize, Serialize};
struct Schedule {
    fields: Vec<Vec<u32>>,
    dom_any: bool,
    dow_any: bool,
}
impl Schedule {
    fn parse(s: &str) -> Result<Self> {
        let parts = s.split_whitespace().collect::<Vec<_>>();
        anyhow::ensure!(parts.len() == 5, "cron requires five fields");
        let mut fields = Vec::new();
        for (i, (lo, hi)) in [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)]
            .into_iter()
            .enumerate()
        {
            let mut values = Vec::new();
            for part in parts[i].split(',') {
                let (range, step) = match part.split_once('/') {
                    Some((r, n)) => (r, n.parse::<u32>()?),
                    None => (part, 1),
                };
                anyhow::ensure!(step > 0, "cron step must be positive");
                let (a, b) = if range == "*" {
                    (lo, hi)
                } else if let Some((a, b)) = range.split_once('-') {
                    (a.parse()?, b.parse()?)
                } else {
                    let a = range.parse()?;
                    (a, a)
                };
                anyhow::ensure!(a >= lo && b <= hi && a <= b, "cron value out of range");
                values.extend(
                    (a..=b)
                        .step_by(step as usize)
                        .map(|v| if i == 4 { v % 7 } else { v }),
                );
            }
            fields.push(values);
        }
        Ok(Self {
            fields,
            dom_any: parts[2].starts_with('*'),
            dow_any: parts[4].starts_with('*'),
        })
    }
    fn next_fire_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        let mut date = after.date_naive();
        let end = date.year().checked_add(400)?;
        while date.year() <= end {
            let dom = self.fields[2].contains(&date.day());
            let dow = self.fields[4].contains(&date.weekday().num_days_from_sunday());
            let day = match (self.dom_any, self.dow_any) {
                (true, true) => dom && dow,
                (true, false) => dom && dow,
                (false, true) => dom && dow,
                (false, false) => dom || dow,
            };
            if self.fields[3].contains(&date.month()) && day {
                for hour in 0..24 {
                    if !self.fields[1].contains(&hour) {
                        continue;
                    }
                    for minute in 0..60 {
                        if !self.fields[0].contains(&minute) {
                            continue;
                        }
                        let local = date.and_hms_opt(hour, minute, 0)?;
                        if let Some(time) = Local.from_local_datetime(&local).earliest()
                            && time > after
                        {
                            return Some(time);
                        }
                    }
                }
            }
            date = date.succ_opt()?;
        }
        None
    }
}
#[derive(Serialize, Deserialize, Clone)]
pub(super) struct Job {
    id: String,
    cron: String,
    prompt: String,
    recurring: bool,
    durable: bool,
    next_fire_at_ms: i64,
    expires_at_ms: Option<i64>,
    #[serde(skip)]
    cancel: CancellationToken,
}
impl Job {
    pub fn view(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap();
        v["next_fire_at_local"] = json!(
            Local
                .timestamp_millis_opt(self.next_fire_at_ms)
                .single()
                .map(|d| d.to_rfc3339())
        );
        v
    }
}
async fn persist(s: &Service, jobs: &BTreeMap<String, Job>) -> Result<()> {
    let bytes = serde_json::to_vec(&jobs.values().filter(|j| j.durable).collect::<Vec<_>>())?;
    let path = s.root.join("claude-cron.json");
    mj_core::runtime::spawn_blocking(move || mj_core::config::atomic_write(&path, &bytes))
        .await??;
    Ok(())
}
fn arm(s: &Arc<Service>, job: Job) {
    let owner = s.clone();
    let s = s.clone();
    let task = mj_core::runtime::spawn(async move {
        let mut job = job;
        loop {
            let until = job
                .expires_at_ms
                .map_or(job.next_fire_at_ms, |e| e.min(job.next_fire_at_ms));
            tokio::select! {_=s.cancel.cancelled()=>break,_=job.cancel.cancelled()=>break,_=tokio::time::sleep(Duration::from_millis((until-chrono::Utc::now().timestamp_millis()).max(0) as u64))=>{}}
            let mut jobs = s.jobs.lock().await;
            if !jobs.contains_key(&job.id) {
                break;
            }
            let now = chrono::Utc::now().timestamp_millis();
            let expired = job.expires_at_ms.is_some_and(|e| now >= e);
            if !expired {
                s.deliver_wake_with_key(
                    &format!("claude-cron-{}-{}", job.id, job.next_fire_at_ms),
                    format!("[mj-claude cron {} ({})]\n{}", job.id, job.cron, job.prompt),
                );
            }
            let next = Schedule::parse(&job.cron)
                .ok()
                .and_then(|c| c.next_fire_after(Local::now()));
            if let Some(next) = next.filter(|_| !expired && job.recurring) {
                job.next_fire_at_ms = next.timestamp_millis();
                jobs.insert(job.id.clone(), job.clone());
            } else {
                jobs.remove(&job.id);
            }
            if job.durable
                && let Err(error) = persist(&s, &jobs).await
            {
                tracing::error!(%error,"persist cron firing failed");
            }
            if !jobs.contains_key(&job.id) {
                break;
            }
        }
    });
    owner.cron_tasks.lock().unwrap().push(task.abort_handle());
}

pub(super) async fn create(s: &Arc<Service>, a: Value) -> Result<Value> {
    let cron = required(&a, "cron")?;
    let prompt = required(&a, "prompt")?;
    let schedule = Schedule::parse(&cron)?;
    let recurring = a["recurring"].as_bool().unwrap_or(true);
    let durable = a["durable"].as_bool().unwrap_or(false);
    let job = Job {
        id: id(),
        cron,
        prompt,
        recurring,
        durable,
        next_fire_at_ms: schedule
            .next_fire_after(Local::now())
            .context("cron has no matching date")?
            .timestamp_millis(),
        expires_at_ms: (!durable && recurring)
            .then(|| chrono::Utc::now().timestamp_millis() + 7 * 86_400_000),
        cancel: CancellationToken::new(),
    };
    let mut jobs = s.jobs.lock().await;
    jobs.insert(job.id.clone(), job.clone());
    if job.durable
        && let Err(e) = persist(s, &jobs).await
    {
        jobs.remove(&job.id);
        return Err(e);
    }
    arm(s, job.clone());
    Ok(job.view())
}
pub(super) async fn delete(s: &Arc<Service>, id: &str) -> Result<Value> {
    let mut jobs = s.jobs.lock().await;
    let job = jobs.remove(id).context("unknown cron job")?;
    if job.durable
        && let Err(e) = persist(s, &jobs).await
    {
        jobs.insert(id.into(), job);
        return Err(e);
    }
    job.cancel.cancel();
    Ok(json!({"id":id,"deleted":true}))
}
pub(super) async fn reload(s: &Arc<Service>) -> Result<()> {
    let bytes = match tokio::fs::read(s.root.join("claude-cron.json")).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let loaded: Vec<Job> = serde_json::from_slice(&bytes)?;
    let mut jobs = s.jobs.lock().await;
    for mut job in loaded {
        Schedule::parse(&job.cron)?;
        job.cancel = CancellationToken::new();
        if job.recurring && job.next_fire_at_ms < chrono::Utc::now().timestamp_millis() {
            job.next_fire_at_ms = Schedule::parse(&job.cron)?
                .next_fire_after(Local::now())
                .context("cron has no matching date")?
                .timestamp_millis();
        }
        jobs.insert(job.id.clone(), job.clone());
        arm(s, job);
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, Timelike};
    #[test]
    fn cron_calendar_rules() {
        let at = Local.with_ymd_and_hms(2025, 3, 1, 0, 0, 0).unwrap();
        let leap = Schedule::parse("0 0 29 2 *")
            .unwrap()
            .next_fire_after(at)
            .unwrap();
        assert_eq!(
            leap.date_naive(),
            NaiveDate::from_ymd_opt(2028, 2, 29).unwrap()
        );
        let at = Local.with_ymd_and_hms(2026, 11, 25, 0, 0, 0).unwrap();
        for dow in [0, 7] {
            let next = Schedule::parse(&format!("0 0 1 * {dow}"))
                .unwrap()
                .next_fire_after(at)
                .unwrap();
            assert_eq!(next.weekday().num_days_from_sunday(), 0);
        }
        let next = Schedule::parse("10-30/10 * * * *")
            .unwrap()
            .next_fire_after(at)
            .unwrap();
        assert_eq!(next.minute(), 10);
        let stepped = Schedule::parse("0 9 */2 * 1")
            .unwrap()
            .next_fire_after(at)
            .unwrap();
        assert_eq!(stepped.weekday().num_days_from_monday(), 0);
        assert_eq!(stepped.day() % 2, 1);
        for bad in [
            "* * *",
            "*/0 * * * *",
            "60 * * * *",
            "* * 0 * *",
            "* * * * 8",
        ] {
            assert!(Schedule::parse(bad).is_err());
        }
    }
}
