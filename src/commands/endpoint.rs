//
// Copyright (c) 2020-2021 science+computing ag and other contributors
//
// This program and the accompanying materials are made
// available under the terms of the Eclipse Public License 2.0
// which is available at https://www.eclipse.org/legal/epl-2.0/
//
// SPDX-License-Identifier: EPL-2.0
//

use std::str::FromStr;
use std::sync::Arc;

use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use clap::ArgMatches;
use log::{debug, info};
use itertools::Itertools;
use tokio_stream::StreamExt;

use crate::config::Configuration;
use crate::util::progress::ProgressBars;
use crate::endpoint::Endpoint;

pub async fn endpoint(matches: &ArgMatches, config: &Configuration, progress_generator: ProgressBars) -> Result<()> {
    let endpoint_names = matches
        .value_of("endpoint_name")
        .map(String::from)
        .map(|ep| vec![ep])
        .unwrap_or_else(|| {
            config.docker()
                .endpoints()
                .iter()
                .map(|ep| ep.name())
                .cloned()
                .collect()
        });

    match matches.subcommand() {
        Some(("ping", matches)) => ping(endpoint_names, matches, config, progress_generator).await,
        Some(("stats", matches)) => stats(endpoint_names, matches, config, progress_generator).await,
        Some(("container", matches)) => container(endpoint_names, matches, config).await,
        Some(("containers", matches)) => containers(endpoint_names, matches, config).await,
        Some((other, _)) => Err(anyhow!("Unknown subcommand: {}", other)),
        None => Err(anyhow!("No subcommand")),
    }
}

async fn ping(endpoint_names: Vec<String>,
    matches: &ArgMatches,
    config: &Configuration,
    progress_generator: ProgressBars
) -> Result<()> {
    let n_pings = matches.value_of("ping_n").map(u64::from_str).transpose()?.unwrap(); // safe by clap
    let sleep = matches.value_of("ping_sleep").map(u64::from_str).transpose()?.unwrap(); // safe by clap
    let endpoints = connect_to_endpoints(config, &endpoint_names).await?;
    let multibar = Arc::new({
        let mp = indicatif::MultiProgress::new();
        if progress_generator.hide() {
            mp.set_draw_target(indicatif::ProgressDrawTarget::hidden());
        }
        mp
    });

    let ping_process = endpoints
        .iter()
        .map(|endpoint| {
            let bar = multibar.add(progress_generator.bar());
            bar.set_length(n_pings);
            bar.set_message(&format!("Pinging {}", endpoint.name()));

            async move {
                for i in 1..(n_pings + 1) {
                    debug!("Pinging {} for the {} time", endpoint.name(), i);
                    let r = endpoint.ping().await;
                    bar.inc(1);
                    if let Err(e) = r {
                        bar.finish_with_message(&format!("Pinging {} failed", endpoint.name()));
                        return Err(e)
                    }

                    tokio::time::sleep(tokio::time::Duration::from_secs(sleep)).await;
                }

                bar.finish_with_message(&format!("Pinging {} successful", endpoint.name()));
                Ok(())
            }
        })
        .collect::<futures::stream::FuturesUnordered<_>>()
        .collect::<Result<()>>();

    let multibar_block = tokio::task::spawn_blocking(move || multibar.join());
    tokio::join!(ping_process, multibar_block).0
}

async fn stats(endpoint_names: Vec<String>,
    matches: &ArgMatches,
    config: &Configuration,
    progress_generator: ProgressBars
) -> Result<()> {
    let csv = matches.is_present("csv");
    let endpoints = connect_to_endpoints(config, &endpoint_names).await?;
    let bar = progress_generator.bar();
    bar.set_length(endpoint_names.len() as u64);
    bar.set_message("Fetching stats");

    let hdr = crate::commands::util::mk_header([
        "Name",
        "Containers",
        "Images",
        "Kernel",
        "Memory",
        "Memory limit",
        "Cores",
        "OS",
        "System Time",
    ].to_vec());

    let data = endpoints
        .into_iter()
        .map(|endpoint| {
            let bar = bar.clone();
            async move {
                let r = endpoint.stats().await;
                bar.inc(1);
                r
            }
        })
        .collect::<futures::stream::FuturesUnordered<_>>()
        .collect::<Result<Vec<_>>>()
        .await
        .map_err(|e| {
            bar.finish_with_message("Fetching stats errored");
            e
        })?
        .into_iter()
        .map(|stat| {
            vec![
                stat.name,
                stat.containers.to_string(),
                stat.images.to_string(),
                stat.kernel_version,
                bytesize::ByteSize::b(stat.mem_total).to_string(),
                stat.memory_limit.to_string(),
                stat.n_cpu.to_string(),
                stat.operating_system.to_string(),
                stat.system_time.map(|t| t.to_string()).unwrap_or_else(|| String::from("unknown")),
            ]
        })
        .collect();

    bar.finish_with_message("Fetching stats successful");
    crate::commands::util::display_data(hdr, data, csv)
}

async fn container(endpoint_names: Vec<String>,
    matches: &ArgMatches,
    config: &Configuration,
) -> Result<()> {
    let container_id = matches.value_of("container_id").unwrap();
    let endpoints = connect_to_endpoints(config, &endpoint_names).await?;
    let relevant_endpoints = endpoints.into_iter()
        .map(|ep| async {
            ep.has_container_with_id(container_id)
                .await
                .map(|b| (ep, b))
        })
        .collect::<futures::stream::FuturesUnordered<_>>()
        .collect::<Result<Vec<(_, bool)>>>()
        .await?
        .into_iter()
        .filter_map(|tpl| {
            if tpl.1 {
                Some(tpl.0)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    if relevant_endpoints.len() > 1 {
        return Err(anyhow!("Found more than one container for id {}", container_id))
    }

    let relevant_endpoint = relevant_endpoints.get(0).ok_or_else(|| {
        anyhow!("Found no container for id {}", container_id)
    })?;

    match matches.subcommand() {
        Some(("top", matches)) => container_top(matches, relevant_endpoint, container_id).await,
        Some(("kill", matches)) => container_kill(matches, relevant_endpoint, container_id).await,
        Some(("delete", _)) => container_delete(relevant_endpoint, container_id).await,
        Some(("start", _)) => container_start(relevant_endpoint, container_id).await,
        Some(("stop", matches)) => container_stop(matches, relevant_endpoint, container_id).await,
        Some(("exec", matches)) => container_exec(matches, relevant_endpoint, container_id).await,
        Some((other, _)) => Err(anyhow!("Unknown subcommand: {}", other)),
        None => Err(anyhow!("No subcommand")),
    }
}

async fn container_top(
    matches: &ArgMatches,
    endpoint: &Endpoint,
    container_id: &str,
) -> Result<()> {
    let csv = matches.is_present("csv");
    let top = endpoint
        .get_container_by_id(container_id)
        .await?
        .ok_or_else(|| anyhow!("Cannot find container {} on {}", container_id, endpoint.name()))?
        .top(None)
        .await?;

    let hdr = crate::commands::util::mk_header(top.titles.iter().map(|s| s.as_ref()).collect());
    crate::commands::util::display_data(hdr, top.processes, csv)
}

async fn container_kill(
    matches: &ArgMatches,
    endpoint: &Endpoint,
    container_id: &str,
) -> Result<()> {
    let signal = matches.value_of("signal");
    let prompt = if let Some(sig) = signal.as_ref() {
        format!("Really kill {} with {}?", container_id, sig)
    } else {
        format!("Really kill {}?", container_id)
    };

    dialoguer::Confirm::new().with_prompt(prompt).interact()?;

    endpoint
        .get_container_by_id(container_id)
        .await?
        .ok_or_else(|| anyhow!("Cannot find container {} on {}", container_id, endpoint.name()))?
        .kill(signal)
        .await
        .map_err(Error::from)
}

async fn container_delete(
    endpoint: &Endpoint,
    container_id: &str,
) -> Result<()> {
    let prompt = format!("Really delete {}?", container_id);
    dialoguer::Confirm::new().with_prompt(prompt).interact()?;

    endpoint
        .get_container_by_id(container_id)
        .await?
        .ok_or_else(|| anyhow!("Cannot find container {} on {}", container_id, endpoint.name()))?
        .delete()
        .await
        .map_err(Error::from)
}

async fn container_start(
    endpoint: &Endpoint,
    container_id: &str,
) -> Result<()> {
    let prompt = format!("Really start {}?", container_id);
    dialoguer::Confirm::new().with_prompt(prompt).interact()?;

    endpoint
        .get_container_by_id(container_id)
        .await?
        .ok_or_else(|| anyhow!("Cannot find container {} on {}", container_id, endpoint.name()))?
        .start()
        .await
        .map_err(Error::from)
}

async fn container_stop(
    matches: &ArgMatches,
    endpoint: &Endpoint,
    container_id: &str,
) -> Result<()> {
    let timeout = matches.value_of("timeout").map(u64::from_str).transpose()?.map(std::time::Duration::from_secs);
    let prompt = format!("Really stop {}?", container_id);
    dialoguer::Confirm::new().with_prompt(prompt).interact()?;

    endpoint
        .get_container_by_id(container_id)
        .await?
        .ok_or_else(|| anyhow!("Cannot find container {} on {}", container_id, endpoint.name()))?
        .stop(timeout)
        .await
        .map_err(Error::from)
}

async fn container_exec(
    matches: &ArgMatches,
    endpoint: &Endpoint,
    container_id: &str,
) -> Result<()> {
    use std::io::Write;
    use futures::TryStreamExt;

    let commands = matches.values_of("commands").unwrap().collect::<Vec<&str>>();
    let prompt = format!("Really run '{}' in {}?", commands.join(" "), container_id);
    dialoguer::Confirm::new().with_prompt(prompt).interact()?;

    let execopts = shiplift::builder::ExecContainerOptions::builder()
        .cmd(commands)
        .attach_stdout(true)
        .attach_stderr(true)
        .build();

    endpoint
        .get_container_by_id(container_id)
        .await?
        .ok_or_else(|| anyhow!("Cannot find container {} on {}", container_id, endpoint.name()))?
        .exec(&execopts)
        .map_err(Error::from)
        .try_for_each(|chunk| async {
            let mut stdout = std::io::stdout();
            let mut stderr = std::io::stderr();
            match chunk {
                shiplift::tty::TtyChunk::StdIn(_) => Err(anyhow!("Cannot handle STDIN TTY chunk")),
                shiplift::tty::TtyChunk::StdOut(v) => stdout.write(&v).map_err(Error::from).map(|_| ()),
                shiplift::tty::TtyChunk::StdErr(v) => stderr.write(&v).map_err(Error::from).map(|_| ()),
            }
        })
        .await
}


async fn containers(endpoint_names: Vec<String>,
    matches: &ArgMatches,
    config: &Configuration,
) -> Result<()> {
    match matches.subcommand() {
        Some(("list", matches)) => containers_list(endpoint_names, matches, config).await,
        Some((other, _)) => Err(anyhow!("Unknown subcommand: {}", other)),
        None => Err(anyhow!("No subcommand")),
    }
}

async fn containers_list(endpoint_names: Vec<String>,
    matches: &ArgMatches,
    config: &Configuration,
) -> Result<()> {
    let list_stopped = matches.is_present("list_stopped");
    let filter_image = matches.value_of("filter_image");
    let older_than_filter = matches.value_of("older_than")
        .map(humantime::parse_rfc3339_weak)
        .transpose()?
        .map(chrono::DateTime::<chrono::Local>::from);
    let newer_than_filter = matches.value_of("newer_than")
        .map(humantime::parse_rfc3339_weak)
        .transpose()?
        .map(chrono::DateTime::<chrono::Local>::from);
    let csv = matches.is_present("csv");
    let hdr = crate::commands::util::mk_header([
        "Endpoint",
        "Container id",
        "Image",
        "Created",
        "Status",
    ].to_vec());

    let data = connect_to_endpoints(config, &endpoint_names)
        .await?
        .into_iter()
        .map(|ep| async move {
            ep.container_stats().await.map(|stats| (ep.name().clone(), stats))
        })
        .collect::<futures::stream::FuturesUnordered<_>>()
        .collect::<Result<Vec<(_, _)>>>()
        .await?
        .into_iter()
        .map(|tpl| {
            let endpoint_name = tpl.0;
            tpl.1
                .into_iter()
                .filter(|stat| list_stopped || stat.state != "exited")
                .filter(|stat| filter_image.map(|fim| fim == stat.image).unwrap_or(true))
                .filter(|stat| older_than_filter.as_ref().map(|time| time > &stat.created).unwrap_or(true))
                .filter(|stat| newer_than_filter.as_ref().map(|time| time < &stat.created).unwrap_or(true))
                .map(|stat| {
                    vec![
                        endpoint_name.clone(),
                        stat.id,
                        stat.image,
                        stat.created.to_string(),
                        stat.status,
                    ]
                })
                .collect::<Vec<Vec<String>>>()
        })
        .flatten()
        .collect::<Vec<Vec<String>>>();

    crate::commands::util::display_data(hdr, data, csv)
}

/// Helper function to connect to all endpoints from the configuration, that appear (by name) in
/// the `endpoint_names` list
async fn connect_to_endpoints(config: &Configuration, endpoint_names: &[String]) -> Result<Vec<Arc<Endpoint>>> {
    let endpoint_configurations = config
        .docker()
        .endpoints()
        .iter()
        .filter(|ep| endpoint_names.contains(ep.name()))
        .cloned()
        .map(|ep_cfg| {
            crate::endpoint::EndpointConfiguration::builder()
                .endpoint(ep_cfg)
                .required_images(config.docker().images().clone())
                .required_docker_versions(config.docker().docker_versions().clone())
                .required_docker_api_versions(config.docker().docker_api_versions().clone())
                .build()
        })
        .collect::<Vec<_>>();

    info!("Endpoint config build");
    info!("Connecting to {n} endpoints: {eps}",
        n = endpoint_configurations.len(),
        eps = endpoint_configurations.iter().map(|epc| epc.endpoint().name()).join(", "));

    crate::endpoint::util::setup_endpoints(endpoint_configurations).await
}
