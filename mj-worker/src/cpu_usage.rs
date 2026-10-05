//! Worker-owned CPU measurement and history; survives controller replacement.
use anyhow::{Result, bail};
use mj_core::cpu_usage::SessionCpuUsage;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

pub fn process_tree_cpu_time(root: u32) -> Result<Duration> {
    #[cfg(target_os = "linux")]
    return linux::process_tree_cpu_time(root);
    #[cfg(target_os = "macos")]
    return macos::process_tree_cpu_time(root);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = root;
        bail!("CPU sampling is not supported on this platform")
    }
}

pub fn online_cpus() -> Result<u32> {
    #[cfg(unix)]
    {
        // sysconf reports the host's online CPUs, independently of cgroup quotas.
        let count = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        if count <= 0 {
            bail!("could not read the machine's online CPU count");
        }
        Ok(u32::try_from(count)?)
    }
    #[cfg(not(unix))]
    bail!("CPU sampling is not supported on this platform")
}

#[derive(Default)]
pub struct CpuSampler {
    last: Option<(Instant, Duration)>,
    weighted_share: f64,
    weight: f64,
    covered: Duration,
    latest: Option<SessionCpuUsage>,
}

impl CpuSampler {
    pub fn observe(&mut self, at: Instant, cpu_time: Duration, online_cpus: u32) {
        if online_cpus == 0 {
            return;
        }
        let Some((last_at, last_cpu)) = self.last else {
            self.last = Some((at, cpu_time));
            return;
        };
        let dt = at.saturating_duration_since(last_at);
        if dt < Duration::from_secs(1) {
            return;
        }
        self.last = Some((at, cpu_time));
        let seconds = dt.as_secs_f64();
        let share = (cpu_time.saturating_sub(last_cpu).as_secs_f64()
            / (seconds * f64::from(online_cpus)))
        .clamp(0.0, 1.0);
        let decay = (-seconds / 3600.0).exp();
        self.weighted_share = self.weighted_share * decay + share * seconds;
        self.weight = self.weight * decay + seconds;
        self.covered = self
            .covered
            .saturating_add(dt)
            .min(Duration::from_secs(3600));
        self.latest = Some(SessionCpuUsage {
            recent_permille: (share * 1000.0).round() as u16,
            hourly_permille: (self.weighted_share / self.weight * 1000.0).round() as u16,
            hourly_covered_secs: self.covered.as_secs() as u32,
            online_cpus,
        });
    }

    pub fn latest(&self) -> Option<SessionCpuUsage> {
        self.latest
    }
}

/// The serving runtime that samples CPU is Unix-only, so the channel type and
/// the sampler task exist only there. The measurement API above stays
/// available everywhere.
#[cfg(unix)]
pub(crate) type CpuRead = Result<Option<SessionCpuUsage>, String>;

/// One sampler per serving worker. The serving loop supervises this task.
#[cfg(unix)]
pub(crate) async fn sample_cpu(sender: tokio::sync::watch::Sender<CpuRead>) {
    let mut sampler = CpuSampler::default();
    let mut ticks = tokio::time::interval(Duration::from_secs(10));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticks.tick().await;
        let result = tokio::task::spawn_blocking(|| -> Result<(Instant, Duration, u32)> {
            Ok((
                Instant::now(),
                process_tree_cpu_time(std::process::id())?,
                online_cpus()?,
            ))
        })
        .await;
        let value = match result {
            Ok(Ok((at, cpu, count))) => {
                sampler.observe(at, cpu, count);
                Ok(sampler.latest())
            }
            Ok(Err(error)) => Err(format!("{error:#}")),
            Err(error) => Err(format!("CPU measurement task failed: {error}")),
        };
        let _ = sender.send_replace(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cpu_sampler_hourly_average_is_not_diluted_at_start() {
        let mut sampler = CpuSampler::default();
        let at = Instant::now();
        for n in 0..=5 {
            sampler.observe(
                at + Duration::from_secs(n * 10),
                Duration::from_secs(n * 20),
                4,
            );
        }
        let usage = sampler.latest().unwrap();
        assert_eq!(usage.hourly_permille, 500);
        assert_eq!(usage.hourly_covered_secs, 50);
    }
    #[test]
    fn cpu_sampler_treats_a_falling_counter_as_idle() {
        let mut sampler = CpuSampler::default();
        let at = Instant::now();
        sampler.observe(at, Duration::from_secs(20), 1);
        sampler.observe(at + Duration::from_secs(10), Duration::from_secs(10), 1);
        assert_eq!(sampler.latest().unwrap().recent_permille, 0);
        sampler.observe(at + Duration::from_secs(20), Duration::from_secs(15), 1);
        assert_eq!(sampler.latest().unwrap().recent_permille, 500);
    }
    #[test]
    fn cpu_sampler_ignores_short_intervals_and_caps_coverage() {
        let mut sampler = CpuSampler::default();
        let at = Instant::now();
        sampler.observe(at, Duration::ZERO, 1);
        sampler.observe(at + Duration::from_millis(500), Duration::from_secs(1), 1);
        assert_eq!(sampler.latest(), None);
        sampler.observe(at + Duration::from_secs(4000), Duration::from_secs(8000), 1);
        assert_eq!(sampler.latest().unwrap().recent_permille, 1000);
        assert_eq!(sampler.latest().unwrap().hourly_covered_secs, 3600);
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn process_tree_cpu_time_counts_a_busy_child_after_it_is_reaped() {
        const CHILD: &str = "MJ_CPU_REAPED_TEST_CHILD";
        const BURN: &str = "MJ_CPU_REAPED_TEST_BURN";
        // A loaded runner slows the wall clock, not the child's own CPU use,
        // so waiting for this much CPU keeps the lower bound valid under
        // parallel test load instead of scaling it with the clock.
        const CHILD_CPU: Duration = Duration::from_millis(1200);
        if std::env::var_os(BURN).is_some() {
            burn_own_cpu(CHILD_CPU);
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        if std::env::var_os(CHILD).is_none() {
            // The counter includes every thread and descendant of this process.
            // Run alone so parallel worker tests cannot contribute CPU time.
            let mut command = cpu_test_command(&directory, CHILD);
            let output = mj_core::subprocess::run_with_input(&mut command, &[]).unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let before = process_tree_cpu_time(std::process::id()).unwrap();
        // A separate process burns the CPU, so this process counts it only
        // through the reaped-child accounting of the process tree.
        let mut command = cpu_test_command(&directory, BURN);
        let output = mj_core::subprocess::run_with_input(&mut command, &[]).unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let used = process_tree_cpu_time(std::process::id())
            .unwrap()
            .saturating_sub(before);
        assert!(used >= Duration::from_secs(1), "{used:?}");
        assert!(used < Duration::from_secs(5), "{used:?}");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn cpu_test_command(directory: &tempfile::TempDir, variable: &str) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "cpu_usage::tests::process_tree_cpu_time_counts_a_busy_child_after_it_is_reaped",
                "--nocapture",
            ])
            .env(variable, "1")
            .env("MJ_INSTANCE", "cpu-counter-test")
            .env("MJ_DATA_DIR", directory.path().join("data"))
            .env("MJ_CONFIG_DIR", directory.path().join("config"));
        command
    }

    /// Spend `target` of this process's own CPU, however slowly the scheduler
    /// runs it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn burn_own_cpu(target: Duration) {
        let start = own_cpu_time();
        let mut state = 0_u64;
        while own_cpu_time().saturating_sub(start) < target {
            for step in 0..1_000_000_u64 {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(step);
            }
            std::hint::black_box(state);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn own_cpu_time() -> Duration {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: `getrusage` writes the usage counters for the requested id.
        let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        assert_eq!(result, 0, "read own CPU time");
        let usage = unsafe { usage.assume_init() };
        let seconds = u64::try_from(usage.ru_utime.tv_sec).unwrap()
            + u64::try_from(usage.ru_stime.tv_sec).unwrap();
        let micros = u64::try_from(usage.ru_utime.tv_usec).unwrap()
            + u64::try_from(usage.ru_stime.tv_usec).unwrap();
        Duration::from_secs(seconds) + Duration::from_micros(micros)
    }
}
