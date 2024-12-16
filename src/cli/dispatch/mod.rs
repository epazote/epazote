use crate::cli::actions::Action;
use anyhow::Result;
use std::path::PathBuf;

pub fn handler(matches: &clap::ArgMatches) -> Result<Action> {
    match matches.subcommand_name() {
        // Subcommands
        // Some("run") => cmd_run::dispatch(get_subcommand_matches(matches, "run")?),

        // Default
        _ => {
            // return error if no matches
            let config = matches.get_one::<PathBuf>("config").unwrap().to_path_buf();

            Ok(Action::Run { config })
        }
    }
}
