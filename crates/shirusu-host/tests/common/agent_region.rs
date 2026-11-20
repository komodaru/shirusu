//! recorder.rs と recorder_coverage_gaps.rs が共有する Agent と共有領域の組。

use shirusu_agent::Agent;
use shirusu_transport::context::HostMonitor;
use shirusu_transport::ring::WriterPolicy;
use shirusu_transport::slot::SlotPool;
use shirusu_transport::RingConfig;
use shirusu_transport::Shm;
use std::sync::Arc;

pub fn agent_and_region(tid: u64) -> (Arc<Agent>, u64, Shm) {
    let cfg = RingConfig::new(64, 4096);
    let pool = Arc::new(SlotPool::new(2));
    let agent = Arc::new(
        Agent::new(pool, cfg, Arc::new(HostMonitor::always_alive())).with_policy(WriterPolicy {
            max_wait: std::time::Duration::ZERO,
            ..Default::default()
        }),
    );
    let handle = agent.thread_begin(tid).expect("slot");
    let base = agent.thread_region(tid).expect("region");
    (agent, handle.generation, base)
}
