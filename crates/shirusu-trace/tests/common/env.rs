//! durability.rs / global_order.rs / writer_reader.rs が共有する CaptureEnvironment。

use shirusu_trace::proto::{CaptureEnvironment, SessionMetadata};

pub fn env(session_id: u64) -> CaptureEnvironment {
    CaptureEnvironment {
        session: SessionMetadata {
            session_id,
            created_by: "shirusu-host test".into(),
            qpc_frequency: 10_000_000,
        },
        os_version: "Windows 11".into(),
        os_build: 26100,
        cpu_arch: "x86_64".into(),
        pointer_width: 64,
        capture_config: "L2".into(),
        feature_flags: 3,
        target_description: "game.exe".into(),
        modules: Vec::new(),
        static_dependencies: Vec::new(),
        hook_evidence: Vec::new(),
    }
}
