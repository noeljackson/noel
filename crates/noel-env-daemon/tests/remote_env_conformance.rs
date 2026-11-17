//! `RemoteEnv` over a daemon started directly (no ssh) against upstream's goldens: the same ported conformance cases
//! as `LocalEnv`'s (`crates/noel-env/tests/conformance/mod.rs`), whose assertions must equal what `NodeExecutionEnv`
//! gave (`local-env.json`), plus the watch cases, which the daemon runs, and upstream's `listFiles` cases, as
//! upstream runs its suite against `RemoteExecutionEnv` (`listFiles: true`, polling every 100 ms).
//!
//! The daemon is `noel-env serve`, or upstream's `pi-env` with `NOEL_ENV_TEST_DAEMON` (`make env-interop`).

#[path = "../../noel-env/tests/conformance/mod.rs"]
mod conformance;
mod support;

use std::time::Duration;

use noel_env::{Connection, ExecutionEnv, RemoteEnv, RemoteEnvOptions, RemoteWatchOptions};

fn make(connection: &Connection) -> impl Fn(&str) -> Box<dyn ExecutionEnv> + '_ {
    move |cwd: &str| -> Box<dyn ExecutionEnv> {
        Box::new(RemoteEnv::new(RemoteEnvOptions {
            watch: RemoteWatchOptions {
                poll_interval: Some(Duration::from_millis(100)),
                ..RemoteWatchOptions::default()
            },
            ..RemoteEnvOptions::new(connection.clone(), "pi-env:test", cwd)
        }))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_env_makes_the_observations_node_execution_env_makes() {
    let connection = support::connection();
    let report =
        conformance::run_fixture_cases(&make(&connection), conformance::Watches::Run).await;
    connection.close();
    report.assert(15, 9, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_env_lists_files_as_upstreams_suite_expects() {
    let connection = support::connection();
    let failures = conformance::run_list_files_cases(&make(&connection)).await;
    connection.close();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
