use crate::cli::actions::Action;
use anyhow::{Context, Result};
use std::path::PathBuf;

/// Helper function to get subcommand matches
pub fn get_subcommand_matches<'a>(
    matches: &'a clap::ArgMatches,
    subcommand: &str,
) -> Result<&'a clap::ArgMatches> {
    matches
        .subcommand_matches(subcommand)
        .context("arguments not found")
}

pub fn handler(matches: &clap::ArgMatches) -> Result<Action> {
    match matches.subcommand_name() {
        // Subcommands
        // Some("run") => cmd_run::dispatch(get_subcommand_matches(matches, "run")?),

        // Default
        _ => {
            // return error if no matches
            let config = matches.get_one::<PathBuf>("config").unwrap().to_path_buf();

            let debug = matches.get_one::<bool>("debug").copied().unwrap_or(false);

            Ok(Action::Run { config, debug })
        }
    }
}
