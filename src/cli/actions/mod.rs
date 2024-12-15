pub mod run;

use std::path::PathBuf;

#[derive(Debug)]
pub enum Action {
    Run { config: PathBuf, debug: bool },
}
