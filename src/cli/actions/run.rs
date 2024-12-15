use crate::cli::{actions::Action, globals::GlobalArgs};
use anyhow::{anyhow, Result};
use ignore::WalkBuilder;
use std::{
    cmp,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    fs::{remove_file, write, OpenOptions},
    io::{self, AsyncWriteExt},
    sync::Semaphore,
};
use tracing::instrument;

/// Handle the create action
#[instrument(skip(action, globals))]
pub async fn handle(action: Action, globals: GlobalArgs) -> Result<()> {
    let Action::Run { config, debug } = action;

    println!("Running: {:?}", config);
    println!("Debug: {:?}", debug);

    Ok(())
}
