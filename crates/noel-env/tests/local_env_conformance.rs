//! `LocalEnv` against upstream's goldens (`tests/fixtures/ts-parity/local-env.json`): upstream Durable's env
//! conformance cases, ported one for one in `conformance/mod.rs`, make the same assertions here, and each one's
//! actual value must equal what `NodeExecutionEnv` gave. `LocalEnv` does not watch yet, so it must refuse the watch
//! cases with `not_supported` (non-parity register, "Workspaces").

mod conformance;

use noel_env::{ExecutionEnv, LocalEnv};

#[test]
fn the_fixture_is_from_the_pinned_upstream_commit() {
    conformance::check_fixture_source();
}

#[tokio::test(flavor = "multi_thread")]
async fn local_env_makes_the_observations_node_execution_env_makes() {
    let make = |cwd: &str| -> Box<dyn ExecutionEnv> { Box::new(LocalEnv::new(cwd)) };
    conformance::run_fixture_cases(&make, conformance::Watches::Refused)
        .await
        .assert(15, 0, 9);
}
