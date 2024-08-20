use crate::settings::SETTINGS;
use bollard::container::{CreateContainerOptions, RemoveContainerOptions, StartContainerOptions};
use bollard::image::CreateImageOptions;
use bollard::models::HostConfig;
use bollard::service::PortBinding;
use bollard::Docker;
use miette::Result;
use std::collections::HashMap;
use std::default::Default;
use std::{panic, str};
use tokio::time::{self, Duration};
use tokio_stream::StreamExt;
pub struct SurrealDBContainer {
    docker: Docker,
}

impl SurrealDBContainer {
    pub async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let docker = Docker::connect_with_unix_defaults()?;
        Ok(Self { docker })
    }

    pub async fn start(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.pull_image().await?;
        self.create_and_start_container().await?;
        Ok(())
    }

    async fn pull_image(&self) -> Result<(), Box<dyn std::error::Error>> {
        let create_image_options: CreateImageOptions<'_, &str> = CreateImageOptions {
            from_image: SETTINGS.sdb.image.as_str(),
            tag: SETTINGS.sdb.tag.as_str(),
            platform: SETTINGS.docker.platform.as_str(),
            ..Default::default()
        };

        let mut stream = self.docker.create_image(Some(create_image_options), None, None);
        while let Some(pull_result) = stream.next().await {
            tracing::trace!("Pulling image: {:?}", pull_result?);
        }
        Ok(())
    }

    async fn create_and_start_container(&self) -> Result<(), Box<dyn std::error::Error>> {
        let bind_address = format!("0.0.0.0:{}", SETTINGS.sdb.port);
        let cmd = vec![
            "start", "--log", "trace", "-u", &SETTINGS.sdb.username, "-p", &SETTINGS.sdb.password, "-b", bind_address.as_str(), "memory"
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

        let create_container_options = CreateContainerOptions {
            name: SETTINGS.sdb.container_name.as_str(),
            platform: Some(SETTINGS.docker.platform.as_str()),
        };

        let config = bollard::container::Config {
            image: Some(format!("{}:{}", SETTINGS.sdb.image, SETTINGS.sdb.tag)),
            cmd: Some(cmd.iter().map(|&s| s.to_string()).collect()),
            exposed_ports: Some(
                vec![(format!("{}/tcp", SETTINGS.sdb.port), HashMap::new())]
                    .into_iter()
                    .collect(),
            ),
            host_config: Some(host_config),
            ..Default::default()
        };

        self.docker
            .create_container(Some(create_container_options), config)
            .await?;

        let start_container_options = StartContainerOptions::<String> {
            ..Default::default()
        };
        self.docker
            .start_container(
                SETTINGS.sdb.container_name.as_str(),
                Some(start_container_options),
            )
            .await?;
        Ok(())
    }

    pub async fn stop(&self) -> Result<(), Box<dyn std::error::Error>> {
        match self.docker
            .stop_container(SETTINGS.sdb.container_name.as_str(), None)
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

impl Drop for SurrealDBContainer {
    fn drop(&mut self) {
        let docker = self.docker.clone();
        tokio::spawn(async move {
            if let Err(e) = docker
                .remove_container(
                    SETTINGS.sdb.container_name.as_str(),
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await
            {
                tracing::warn!("Failed to remove container: {:?}", e);
            }
        });
    }
}

pub async fn surrealdb_ready() -> Result<(), Box<dyn std::error::Error>> {
    let url = &format!("http://{}:{}/health", SETTINGS.sdb.host, SETTINGS.sdb.port);

    let mut attempts = 25;

    while attempts > 0 {
        if let Err(_e) = reqwest::Client::default().get(url).send().await {
            tracing::debug!("attempt {:?} failed", 26 - attempts);
        } else {
            tracing::debug!("surrealdb ready after {:?} attempts", 26 - attempts);
            return Ok(());
        }

        attempts -= 1;
        time::sleep(Duration::from_millis(200)).await;
    }

    panic!("timeout waiting for surrealdb");
}