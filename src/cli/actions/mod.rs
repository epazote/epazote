pub mod metrics;
pub mod request;
pub mod run;
pub mod ssl;

use std::path::PathBuf;

#[derive(Debug)]
pub enum Action {
    Run { config: PathBuf, port: u16 },
}
