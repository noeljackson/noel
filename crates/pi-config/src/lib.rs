use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const APP_NAME: &str = "pi";
pub const CONFIG_DIR_NAME: &str = ".pi";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigPaths {
    pub agent_dir: PathBuf,
    pub session_dir: PathBuf,
}
