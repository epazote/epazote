use crate::cli::actions::Action;
use anyhow::Result;
use std::path::PathBuf;

pub fn handler(matches: &clap::ArgMatches) -> Result<Action> {
    Ok(Action::Run {
        config: matches.get_one::<PathBuf>("config").unwrap().to_path_buf(),
        port: matches.get_one::<u16>("port").copied().unwrap_or(9080),
    })
}
