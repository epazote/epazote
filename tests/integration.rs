#![allow(deprecated, clippy::unwrap_used, clippy::expect_used)]
use assert_cmd::cargo::CommandCargoExt;
use reqwest::Client;
use std::process::Command;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::time::sleep;

#[tokio::test]
async fn test_epazote_integration() {
    // 1. Start Mockito Server
    let mut server = mockito::Server::new_async().await;
    let mock_url = server.url();

    let _m = server
        .mock("GET", "/health")
        .with_status(200)
        .create_async()
        .await;

    // 2. Create Config File
    let config_content = format!(
        r"
services:
  test_service:
    url: {mock_url}/health
    every: 1s
    expect:
      status: 200
"
    );

    let config_file = NamedTempFile::new().expect("Failed to create temp file");
    std::fs::write(config_file.path(), config_content).expect("Failed to write config");

    // 3. Pick a random port for metrics (to avoid conflicts)
    // Using port 0 lets OS pick, but epazote needs to know it.
    // We'll pick a likely free port or just let the OS bind and we'd need to parse logs,
    // but epazote takes -p. Let's pick 19090 and hope.
    // Better: let's try to bind a TcpListener to 0, get the port, drop it, and use that.
    let metrics_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };

    // 4. Spawn Epazote
    let mut cmd = Command::cargo_bin("epazote").expect("Failed to find binary");

    let mut child = cmd
        .arg("-c")
        .arg(config_file.path())
        .arg("-p")
        .arg(metrics_port.to_string())
        .spawn()
        .expect("Failed to start epazote");

    // Give it some time to start and scrape
    // Retry loop to check metrics
    let client = Client::new();
    let metrics_url = format!("http://localhost:{metrics_port}/metrics");

    let mut success = false;
    for _ in 0..10 {
        sleep(Duration::from_secs(1)).await;

        if let Ok(response) = client.get(&metrics_url).send().await
            && response.status().is_success()
        {
            let text = response.text().await.unwrap_or_default();
            println!("Metrics: {text}"); // For debugging if needed

            // Check for specific metric
            if text.contains(r#"epazote_status{service_name="test_service"} 1"#) {
                success = true;
                break;
            }
        }
    }

    // 5. Cleanup
    let _ = child.kill(); // Kill the process
    let _ = child.wait(); // Wait for it to exit

    assert!(
        success,
        "Failed to verify epazote metrics indicating success"
    );
}

#[tokio::test]
async fn test_epazote_ssl_integration() {
    // 1. Create Config File with an HTTPS service
    let config_content = r"
services:
  google_ssl:
    url: https://www.google.com
    every: 1s
    expect:
      status: 200
";

    let config_file = NamedTempFile::new().expect("Failed to create temp file");
    std::fs::write(config_file.path(), config_content).expect("Failed to write config");

    // 2. Pick a random port for metrics
    let metrics_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };

    // 3. Spawn Epazote
    let mut cmd = Command::cargo_bin("epazote").expect("Failed to find binary");

    let mut child = cmd
        .arg("-c")
        .arg(config_file.path())
        .arg("-p")
        .arg(metrics_port.to_string())
        .spawn()
        .expect("Failed to start epazote");

    // 4. Retry loop to check metrics for SSL expiration
    let client = Client::new();
    let metrics_url = format!("http://localhost:{metrics_port}/metrics");

    let mut success = false;
    for _ in 0..15 {
        sleep(Duration::from_secs(1)).await;

        if let Ok(response) = client.get(&metrics_url).send().await
            && response.status().is_success()
        {
            let text = response.text().await.unwrap_or_default();

            // Check for SSL expiry metric
            if let Some(line) = text.lines().find(|l| {
                l.contains(r#"epazote_ssl_cert_expiry_seconds{service_name="google_ssl"}"#)
                    && !l.starts_with('#')
            }) {
                // Also verify it's a positive number (not 0 or -1)
                if let Some(val_str) = line.split_whitespace().last()
                    && let Ok(val) = val_str.parse::<i64>()
                    && val > 0
                {
                    success = true;
                    break;
                }
            }
        }
    }

    // 5. Cleanup
    let _ = child.kill();
    let _ = child.wait();

    assert!(success, "Failed to verify epazote SSL metrics");
}

#[tokio::test]
async fn test_epazote_if_not_cmd_integration() {
    // 1. Start Mockito Server that returns failure
    let mut server = mockito::Server::new_async().await;
    let mock_url = server.url();

    let _m = server
        .mock("GET", "/fail")
        .with_status(500)
        .create_async()
        .await;

    // 2. Create a temporary marker file path
    let marker_file = tempfile::NamedTempFile::new().expect("Failed to create marker file");
    let marker_path = marker_file.path().to_owned();
    // Remove the file so we can detect when epazote creates/touches it
    std::fs::remove_file(&marker_path).expect("Failed to remove initial marker file");

    // 3. Create Config File with if_not cmd
    let config_content = format!(
        r"
services:
  fail_service:
    url: {mock_url}/fail
    every: 1s
    expect:
      status: 200
      if_not:
        cmd: touch {}
",
        marker_path.to_str().expect("Invalid marker path")
    );

    let config_file = NamedTempFile::new().expect("Failed to create config file");
    std::fs::write(config_file.path(), config_content).expect("Failed to write config");

    // 4. Pick a random port for metrics
    let metrics_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };

    // 5. Spawn Epazote
    let mut cmd = Command::cargo_bin("epazote").expect("Failed to find binary");

    let mut child = cmd
        .arg("-c")
        .arg(config_file.path())
        .arg("-p")
        .arg(metrics_port.to_string())
        .spawn()
        .expect("Failed to start epazote");

    // 6. Wait for the marker file to be created by the fallback command
    let mut success = false;
    for _ in 0..10 {
        sleep(Duration::from_secs(1)).await;
        if marker_path.exists() {
            success = true;
            break;
        }
    }

    // 7. Cleanup
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        success,
        "Fallback command was not executed (marker file not found)"
    );
}
