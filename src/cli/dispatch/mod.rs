use crate::cli::actions::Action;
use anyhow::Result;
use std::path::PathBuf;

pub fn handler(matches: &clap::ArgMatches) -> Result<Action> {
    matches.subcommand_name();
    {
        // return error if no matches
        let config = matches.get_one::<PathBuf>("config").unwrap().to_path_buf();

        Ok(Action::Run { config })
    }
}
