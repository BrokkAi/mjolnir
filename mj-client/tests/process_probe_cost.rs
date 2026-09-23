//! The liveness probe runs every 250 ms while a new client waits for an older
//! daemon's upgrade handoff, for as long as that daemon has work. It must cost
//! one small read, not a scan of every process on the machine. This file holds
//! a single test so the thread count it measures belongs to this process alone.

#[cfg(target_os = "linux")]
fn thread_count() -> usize {
    std::fs::read_dir("/proc/self/task").unwrap().count()
}

#[cfg(target_os = "linux")]
#[test]
fn repeated_liveness_probes_start_no_threads_and_stay_cheap() {
    let pid = std::process::id();
    let threads = thread_count();
    let started = std::time::Instant::now();
    for _ in 0..200 {
        assert!(mj_client::daemon::process_is_alive(pid));
        assert!(!mj_client::daemon::process_is_zombie(pid));
    }
    assert_eq!(
        thread_count(),
        threads,
        "a liveness probe must not start a thread pool"
    );
    // 200 probes of one /proc file take well under a millisecond each; a full
    // process-table scan per probe takes far longer on a busy host.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "200 probes took {:?}",
        started.elapsed()
    );
}
