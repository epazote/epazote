pub mod metrics;
pub mod run;

use std::path::PathBuf;

#[derive(Debug)]
pub enum Action {
    Run { config: PathBuf, port: u16 },
}
