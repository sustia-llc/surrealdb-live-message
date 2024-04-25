use crate::settings::SETTINGS;
use bollard::container::{CreateContainerOptions, StartContainerOptions};
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

pub(crate) async fn start_surrealdb_container() -> Result<(), Box<dyn std::error::Error>> {
    let docker = Docker::connect_with_unix_defaults()?;

    let create_image_options: CreateImageOptions<'_, &str> = CreateImageOptions {
        from_image: SETTINGS.sdb.image.as_str(),
        tag: SETTINGS.sdb.tag.as_str(),
        platform: SETTINGS.docker.platform.as_str(),
        ..Default::default()
    };

    let mut stream = docker.create_image(Some(create_image_options), None, None);
    while let Some(pull_result) = stream.next().await {
        tracing::trace!("Pulling image: {:?}", pull_result?);
    }

    let cmd = vec![
        "start", "--log", "trace", "--user", "root", "--pass", "root", "memory",
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

    docker
        .create_container(Some(create_container_options), config)
        .await?;

    let start_container_options = StartContainerOptions::<String> {
        ..Default::default()
    };
    docker
        .start_container(
            SETTINGS.sdb.container_name.as_str(),
            Some(start_container_options),
        )
        .await?;
    Ok(())
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

pub async fn stop_surrealdb_container() -> Result<(), Box<dyn std::error::Error>> {
    let docker = Docker::connect_with_unix_defaults()?;
    match docker
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
    match docker
        .remove_container(SETTINGS.sdb.container_name.as_str(), None)
        .await
    {
        Ok(result) => {
            tracing::debug!("docker remove result: {:?}", result);
        }
        Err(error) => {
            tracing::warn!("docker remove failed: {:?}", error);
        }
    }
    Ok(())
}
