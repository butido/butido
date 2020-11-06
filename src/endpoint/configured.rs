use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use std::sync::RwLock;
use std::str::FromStr;
use std::path::PathBuf;

use anyhow::Error;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use getset::{Getters, CopyGetters};
use shiplift::ContainerOptions;
use shiplift::Docker;
use shiplift::ExecContainerOptions;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use typed_builder::TypedBuilder;

use crate::util::docker::ImageName;
use crate::endpoint::EndpointConfiguration;
use crate::job::RunnableJob;
use crate::job::JobResource;
use crate::log::LogItem;
use crate::filestore::StagingStore;

#[derive(Getters, CopyGetters, TypedBuilder)]
pub struct Endpoint {
    #[getset(get = "pub")]
    name: String,

    #[getset(get = "pub")]
    docker: Docker,

    #[getset(get_copy = "pub")]
    speed: usize,

    #[getset(get_copy = "pub")]
    num_max_jobs: usize,
}

impl Debug for Endpoint {
    fn fmt(&self, f: &mut Formatter) -> std::result::Result<(), std::fmt::Error> {
        write!(f, "Endpoint({}, max: {})", self.name, self.num_max_jobs)
    }
}

impl Endpoint {

    pub(in super) async fn setup(epc: EndpointConfiguration) -> Result<Self> {
        let ep = Endpoint::setup_endpoint(epc.endpoint())?;

        let versions_compat     = Endpoint::check_version_compat(epc.required_docker_versions().as_ref(), &ep);
        let api_versions_compat = Endpoint::check_api_version_compat(epc.required_docker_api_versions().as_ref(), &ep);
        let imgs_avail          = Endpoint::check_images_available(epc.required_images().as_ref(), &ep);

        let (versions_compat, api_versions_compat, imgs_avail) =
            tokio::join!(versions_compat, api_versions_compat, imgs_avail);

        let _ = versions_compat?;
        let _ = api_versions_compat?;
        let _ = imgs_avail?;

        Ok(ep)
    }

    fn setup_endpoint(ep: &crate::config::Endpoint) -> Result<Endpoint> {
        match ep.endpoint_type() {
            crate::config::EndpointType::Http => {
                shiplift::Uri::from_str(ep.uri())
                    .map(|uri| shiplift::Docker::host(uri))
                    .map_err(Error::from)
                    .map(|docker| {
                        Endpoint::builder()
                            .name(ep.name().clone())
                            .docker(docker)
                            .speed(ep.speed())
                            .num_max_jobs(ep.maxjobs())
                            .build()
                    })
            }

            crate::config::EndpointType::Socket => {
                Ok({
                    Endpoint::builder()
                        .name(ep.name().clone())
                        .speed(ep.speed())
                        .num_max_jobs(ep.maxjobs())
                        .docker(shiplift::Docker::unix(ep.uri()))
                        .build()
                })
            }
        }
    }

    async fn check_version_compat(req: Option<&Vec<String>>, ep: &Endpoint) -> Result<()> {
        match req {
            None => Ok(()),
            Some(v) => {
                let avail = ep.docker().version().await?;

                if !v.contains(&avail.version) {
                    Err(anyhow!("Incompatible docker version on endpoint {}: {}",
                            ep.name(), avail.version))
                } else {
                    Ok(())
                }
            }
        }
    }

    async fn check_api_version_compat(req: Option<&Vec<String>>, ep: &Endpoint) -> Result<()> {
        match req {
            None => Ok(()),
            Some(v) => {
                let avail = ep.docker().version().await?;

                if !v.contains(&avail.api_version) {
                    Err(anyhow!("Incompatible docker API version on endpoint {}: {}",
                            ep.name(), avail.api_version))
                } else {
                    Ok(())
                }
            }
        }
    }

    async fn check_images_available(imgs: &Vec<ImageName>, ep: &Endpoint) -> Result<()> {
        use shiplift::ImageListOptions;

        trace!("Checking availability of images: {:?}", imgs);
        let available_names = ep
            .docker()
            .images()
            .list(&ImageListOptions::builder().all().build())
            .await
            .with_context(|| anyhow!("Listing images on endpoint: {}", ep.name))?
            .into_iter()
            .map(|image_rep| {
                image_rep.repo_tags
                    .unwrap_or_default()
                    .into_iter()
                    .map(ImageName::from)
            })
            .flatten()
            .collect::<Vec<ImageName>>();

        imgs.iter()
            .map(|img| {
                if !available_names.contains(img) {
                    Err(anyhow!("Image '{}' missing from endpoint '{}'", img.as_ref(), ep.name))
                } else {
                    Ok(())
                }
            })
            .collect::<Result<Vec<_>>>()
            .map(|_| ())
    }

    pub async fn run_job(&self, job: RunnableJob, logsink: UnboundedSender<LogItem>, staging: Arc<RwLock<StagingStore>>) -> Result<Vec<PathBuf>>  {
        use crate::log::buffer_stream_to_line_stream;
        use tokio::stream::StreamExt;

        let (container_id, _warnings) = {
            let envs: Vec<String> = job.resources()
                .iter()
                .filter_map(|r| match r {
                    JobResource::Environment(k, v) => Some(format!("{}={}", k, v)),
                    JobResource::Artifact(_)       => None,
                })
                .collect();

            let builder_opts = shiplift::ContainerOptions::builder(job.image().as_ref())
                    .env(envs.iter().map(AsRef::as_ref).collect())
                    .build();

            let create_info = self.docker
                .containers()
                .create(&builder_opts)
                .await?;

            if create_info.warnings.is_some() {
                // TODO: Handle warnings
            }

            (create_info.id, create_info.warnings)
        };

        let script      = job.script().as_ref().as_bytes();
        let script_path = PathBuf::from("/script");
        let exec_opts   = ExecContainerOptions::builder()
            .cmd(vec!["/script"])
            .build();

        let container = self.docker.containers().get(&container_id);
        container.copy_file_into(script_path, script).await?;
        let stream = container.exec(&exec_opts);
        let _ = buffer_stream_to_line_stream(stream)
            .map(|line| {
                line.map_err(Error::from)
                    .and_then(|l| {
                        crate::log::parser()
                            .parse(l.as_bytes())
                            .map_err(Error::from)
                            .and_then(|item| logsink.send(item).map_err(Error::from))
                    })
            })
            .collect::<Result<Vec<_>>>()
            .await?;

        let tar_stream = container.copy_from(&PathBuf::from("/outputs/"))
            .map(|item| item.map_err(Error::from));

        staging
            .write()
            .map_err(|_| anyhow!("Lock poisoned"))?
            .write_files_from_tar_stream(tar_stream)
            .await
    }

    pub async fn number_of_running_containers(&self) -> Result<usize> {
        self.docker
            .containers()
            .list(&Default::default())
            .await
            .map_err(Error::from)
            .map(|list| list.len())
    }

}
