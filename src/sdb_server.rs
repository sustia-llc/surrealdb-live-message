use crate::settings::SETTINGS;
use bollard::Docker;
use bollard::models::ContainerCreateBody;
use bollard::models::HostConfig;
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, StartContainerOptionsBuilder,
};
use bollard::service::PortBinding;
use miette::Result;
use std::collections::HashMap;
use std::default::Default;
use tokio::sync::oneshot;
use tokio::time::{Duration, sleep};
use tokio_stream::StreamExt;

pub struct SurrealDBContainer {
    docker: Docker,
}

impl SurrealDBContainer {
    pub async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let docker = Docker::connect_with_unix_defaults()?;
        Ok(Self { docker })
    }

    pub async fn start_and_wait(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.pull_image().await?;
        self.create_and_start_container().await?;

        let (tx, rx) = oneshot::channel();
        let url = format!("http://{}:{}/health", SETTINGS.sdb.host, SETTINGS.sdb.port);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()?;

        tokio::spawn(async move {
            let mut attempts = 25;
            while attempts > 0 {
                match client.get(&url).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        tracing::debug!("SurrealDB health check succeeded");
                        let _ = tx.send(());
                        return;
                    }
                    Ok(resp) => {
                        tracing::debug!("Health check returned status: {}", resp.status());
                        attempts -= 1;
                    }
                    Err(e) => {
                        tracing::debug!("Health check failed: {}", e);
                        attempts -= 1;
                    }
                }
                if attempts > 0 {
                    sleep(Duration::from_millis(1000)).await;
                }
            }
            // Ensure the channel is closed if we run out of attempts
            drop(tx);
        });

        // Wait for ready signal or timeout
        match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(())) => {
                tracing::info!("SurrealDB is ready and accepting connections");
                Ok(())
            }
            Ok(Err(_)) => Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                "health check failed after all attempts",
            ))),
            Err(_) => Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timeout waiting for surrealdb to start",
            ))),
        }
    }

    async fn pull_image(&self) -> Result<()> {
        let create_image_options = CreateImageOptionsBuilder::default()
            .from_image(&SETTINGS.sdb.image)
            .tag(&SETTINGS.sdb.tag)
            .platform(&SETTINGS.docker.platform)
            .build();
        let mut stream = self
            .docker
            .create_image(Some(create_image_options), None, None);
        while let Some(pull_result) = stream.next().await {
            tracing::trace!("Pulling image: {:?}", pull_result);
        }
        Ok(())
    }

    async fn create_and_start_container(&self) -> Result<(), Box<dyn std::error::Error>> {
        let bind_address = format!("0.0.0.0:{}", SETTINGS.sdb.port);
        let cmd: Vec<String> = vec![
            "start".to_string(),
            "--log".to_string(),
            "trace".to_string(),
            "-u".to_string(),
            SETTINGS.sdb.username.to_string(),
            "-p".to_string(),
            SETTINGS.sdb.password.to_string(),
            "-b".to_string(),
            bind_address,
            "memory".to_string(),
        ];

        let port_bindings = {
            let mut port_map = HashMap::new();
            port_map.insert(
                format!("{}/tcp", SETTINGS.sdb.port),
                Some(vec![PortBinding {
                    host_ip: None,
                    host_port: Some(format!("{}", SETTINGS.sdb.port)),
                }]),
            );
            port_map
        };

        let host_config = HostConfig {
            port_bindings: Some(port_bindings),
            auto_remove: Some(true), // This will remove the container when it stops
            ..Default::default()
        };

        let container_body = ContainerCreateBody {
            image: Some(format!("{}:{}", SETTINGS.sdb.image, SETTINGS.sdb.tag)),
            cmd: Some(cmd),
            exposed_ports: Some(
                vec![(format!("{}/tcp", SETTINGS.sdb.port), HashMap::new())]
                    .into_iter()
                    .collect(),
            ),
            host_config: Some(host_config),
            ..Default::default()
        };

        let create_container_options = CreateContainerOptionsBuilder::default()
            .name(&SETTINGS.sdb.container_name)
            .platform(&SETTINGS.docker.platform)
            .build();
        let container = self
            .docker
            .create_container(Some(create_container_options), container_body)
            .await?;
        let start_container_options = StartContainerOptionsBuilder::default().build();
        self.docker
            .start_container(&container.id, Some(start_container_options))
            .await?;
        Ok(())
    }

    pub async fn stop(&self) -> Result<(), Box<dyn std::error::Error>> {
        match self
            .docker
            .stop_container(
                SETTINGS.sdb.container_name.as_str(),
                Some(bollard::query_parameters::StopContainerOptionsBuilder::default().build()),
            )
            .await
        {
            Ok(result) => {
                tracing::debug!("docker stop result: {:?}", result);
            }
            Err(error) => {
                tracing::warn!("docker stop failed: {:?}", error);
            }
        }
        Ok(())
    }
}
