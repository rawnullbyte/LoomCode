//! Measured benchmarks against the spec's stated targets. `#[ignore]` by default so a
//! plain `cargo test` stays fast and hermetic; run with:
//!
//! ```sh
//! cargo test --release -p loon-cli --test bench -- --ignored --nocapture
//! ```
//!
//! Everything printed is a real measurement on this machine, labeled as such — numbers
//! from this harness must never be quoted as spec-target results without saying they
//! are local-machine measurements.

use std::path::PathBuf;
use std::time::{Duration, Instant};

fn human(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

/// Target: binary startup well under a human-perceptible threshold (spec §19 names
/// startup latency). We measure `loon --version` end to end (exec + arg parse + exit),
/// which is the floor any interactive session inherits.
#[test]
#[ignore]
fn bench_cli_startup() {
    let exe = env!("CARGO_BIN_EXE_loom");
    let runs = 20;
    let mut times_ms = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t = Instant::now();
        let out = std::process::Command::new(exe)
            .arg("--version")
            .output()
            .expect("spawn loon");
        times_ms.push(t.elapsed().as_millis() as u64);
        assert!(out.status.success());
    }
    times_ms.sort_unstable();
    let mean = times_ms.iter().sum::<u64>() / runs as u64;
    let p50 = times_ms[runs / 2];
    let p95 = times_ms[(runs as f64 * 0.95) as usize % runs];
    println!("startup (loon --version, n={runs}): mean {mean}ms · p50 {p50}ms · p95 {p95}ms");
}

/// Target: indexing a large repository must be fast and the warm cache must make the
/// second scan nearly free. Builds a 100k-file tree, scans cold, scans warm.
#[test]
#[ignore]
fn bench_repo_index_100k_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    // 100k files across 100 dirs x 1000 files; content small so creation is I/O-bound.
    let dirs = 100u32;
    let files_per_dir = 1000u32;
    let t_build = Instant::now();
    for d in 0..dirs {
        let dir = root.join(format!("dir{d:03}"));
        std::fs::create_dir_all(&dir).unwrap();
        for f in 0..files_per_dir {
            std::fs::write(
                dir.join(format!("file{f:04}.txt")),
                format!("dir {d} file {f}\n"),
            )
            .unwrap();
        }
    }
    println!(
        "fixture build: {} files in {:.1}s",
        human((dirs * files_per_dir) as u64),
        t_build.elapsed().as_secs_f64()
    );

    let cache_dir = tmp.path().join("cache");
    let t_cold = Instant::now();
    let mut index = loon_core::index::RepoIndex::open(&root, &cache_dir);
    let report = index.scan().unwrap();
    let cold = t_cold.elapsed();
    println!(
        "index cold scan: {} files in {:.2}s ({:.0}k files/s) — refreshed {} reused {}",
        human(report.entries.len() as u64),
        cold.as_secs_f64(),
        report.entries.len() as f64 / 1000.0 / cold.as_secs_f64(),
        report.refreshed,
        report.reused
    );
    assert_eq!(report.entries.len(), (dirs * files_per_dir) as usize);

    let t_warm = Instant::now();
    let mut index2 = loon_core::index::RepoIndex::open(&root, &cache_dir);
    let report2 = index2.scan().unwrap();
    let warm = t_warm.elapsed();
    println!(
        "index warm scan: {} files in {:.2}s — reused {} refreshed {}",
        human(report2.entries.len() as u64),
        warm.as_secs_f64(),
        report2.reused,
        report2.refreshed
    );
    assert_eq!(report2.reused, report.entries.len());
    assert_eq!(report2.refreshed, 0);
    let _ = PathBuf::new(); // keep import honest if fixture changes
}

/// Target: an interactive turn must start producing tokens quickly. We measure the
/// mock provider's full turn overhead (session create + journal + one request) — this
/// bounds Loon's own per-turn cost; real-model latency is dominated by the provider
/// and is deliberately not claimed here.
#[tokio::test]
#[ignore]
async fn bench_turn_overhead_mock() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = loon_config::LoonPaths {
        config_dir: tmp.path().join("c"),
        data_dir: tmp.path().join("d"),
        state_dir: tmp.path().join("s"),
        cache_dir: tmp.path().join("h"),
        runtime_dir: tmp.path().join("r"),
    };
    paths.ensure().unwrap();
    let runs = 25;
    let mut times_ms = Vec::with_capacity(runs);
    for i in 0..runs {
        let registry = std::sync::Arc::new(loon_providers::ProviderRegistry::with_mock(
            loon_providers::MockProvider::with_scripts(vec![loon_providers::MockScript::text(
                format!("answer {i}"),
            )]),
        ));
        let t = Instant::now();
        let mut agent = loon_core::Agent::new_session(
            &paths,
            loon_core::AgentConfig {
                model_ref: "mock".into(),
                tool_output_bytes: 4096,
                max_retries: 0,
                allow_retry_after_partial_stream: false,
                check_command: None,
                ultra_rounds: 0,
                permissions: Default::default(),
                context_window: 128_000,
                extra_tools: Vec::new(),
            },
            registry,
            loon_tools::ToolRegistry::standard(),
            ws.clone(),
            true,
        )
        .unwrap();
        let r = agent.run_turn("timing probe").await;
        assert!(matches!(r.status, loon_core::TurnStatus::Completed));
        times_ms.push(t.elapsed().as_millis() as u64);
    }
    times_ms.sort_unstable();
    let mean = times_ms.iter().sum::<u64>() / runs as u64;
    let p50 = times_ms[runs / 2];
    println!(
        "turn overhead (mock, session+journal+request, n={runs}): mean {mean}ms · p50 {p50}ms"
    );
    let _ = Duration::ZERO; // keep import honest
}
