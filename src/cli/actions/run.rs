use crate::cli::{
    actions::Action,
    config::{Config, ServiceDetails},
    globals::GlobalArgs,
};
use anyhow::Result;
use reqwest::Client;
use std::env;
use tokio::time::interval;
use tracing::{debug, instrument};

// Name your user agent after your app?
static APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);

/// Handle the create action
#[instrument(skip(action, globals))]
pub async fn handle(action: Action, globals: GlobalArgs) -> Result<()> {
    let Action::Run { config } = action;

    let config_path = config;

    let config = Config::new(config_path)?;

    let mut handles = Vec::new();

    for (service_name, service) in &config.services {
        let service_name = service_name.clone();
        let service_details = service.clone();

        let client = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .build()?;

        // Spawn a task for each service
        let handle = tokio::spawn(async move {
            run_service(service_name, service_details, client).await;
        });

        handles.push(handle);
    }

    // Wait for all tasks to complete (runs indefinitely until app exit)
    for handle in handles {
        let _ = handle.await;
    }

    Ok(())
}
/// Runs the task for a single service
async fn run_service(service_name: String, service_details: ServiceDetails, client: Client) {
    let mut interval_timer = interval(service_details.every);

    loop {
        interval_timer.tick().await; // Wait for the next interval

        // Perform the service scan
        match scan_service(&service_name, &service_details, &client).await {
            Ok(_) => (),
            Err(e) => eprintln!("Error scanning service '{}': {}", service_name, e),
        }
    }
}

/// Simulates scanning a service (e.g., sending an HTTP request)
async fn scan_service(
    service_name: &str,
    service_details: &ServiceDetails,
    client: &Client,
) -> Result<()> {
    // Send a GET request to the service
    let response = client.get(&service_details.url).send().await?;
    let status = response.status();
    let headers = response.headers();

    let mut cmd_exit = 0;
    if status.as_u16() != service_details.expect.status {
        // If `if_not` is specified, perform the necessary actions
        if let Some(if_not) = &service_details.expect.if_not {
            // find SHELL From env
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());

            let output = tokio::process::Command::new(shell)
                .arg("-c")
                .arg(&if_not.cmd)
                .spawn()?
                .wait()
                .await?;

            if !output.success() {
                cmd_exit = 1;
            }
        }
    }

    debug!(
        service_name = service_name,
        service_url = service_details.url,
        service_status = status.as_u16(),
        cmd_exit = cmd_exit,
        response_headers = ?headers
    );

    Ok(())
}
