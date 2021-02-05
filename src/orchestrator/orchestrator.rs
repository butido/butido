//
// Copyright (c) 2020-2021 science+computing ag and other contributors
//
// This program and the accompanying materials are made
// available under the terms of the Eclipse Public License 2.0
// which is available at https://www.eclipse.org/legal/epl-2.0/
//
// SPDX-License-Identifier: EPL-2.0
//

#![allow(unused)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use diesel::PgConnection;
use indicatif::ProgressBar;
use log::debug;
use log::trace;
use tokio::sync::RwLock;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;
use tokio_stream::StreamExt;
use typed_builder::TypedBuilder;
use uuid::Uuid;

use crate::config::Configuration;
use crate::db::models as dbmodels;
use crate::endpoint::EndpointConfiguration;
use crate::endpoint::EndpointScheduler;
use crate::filestore::Artifact;
use crate::filestore::MergedStores;
use crate::filestore::ReleaseStore;
use crate::filestore::StagingStore;
use crate::job::JobDefinition;
use crate::job::RunnableJob;
use crate::job::Dag;
use crate::source::SourceCache;
use crate::util::progress::ProgressBars;

#[cfg_attr(doc, aquamarine::aquamarine)]
/// The Orchestrator
///
/// The Orchestrator is used to orchestrate the work on one submit.
/// On a very high level: It uses a [Dag](crate::job::Dag) to build a number (list) of
/// [JobTasks](crate::orchestrator::JobTask) that is then run concurrently.
///
/// Because of the implementation of [JobTask], the work happens in
/// form of a tree, propagating results to the root (which is held by the Orchestrator itself).
/// The Orchestrator also holds the connection to the database, the access to the filesystem via
/// the [ReleaseStore](crate::filestore::ReleaseStore) and the
/// [StagingStore](crate::filestore::StagingStore), which are merged into a
/// [MergedStores](crate::filestore::MergedStores) object.
///
///
/// # Control Flow
///
/// This section describes the control flow starting with the construction of the Orchestrator
/// until the exit of the Orchestrator.
///
/// ```mermaid
/// sequenceDiagram
///     participant Caller as User
///     participant O   as Orchestrator
///     participant JT1 as JobTask
///     participant JT2 as JobTask
///     participant SCH as Scheduler
///     participant EP1 as Endpoint
///
///     Caller->>+O: run()
///         O->>+O: run_tree()
///
///             par Starting jobs
///                 O->>+JT1: run()
///             and
///                 O->>+JT2: run()
///             end
///
///             par Working on jobs
///                 loop until dependencies received
///                     JT1->>JT1: recv()
///                 end
///
///                 JT1->>+JT1: build()
///                 JT1->>SCH: schedule(job)
///                 SCH->>+EP1: run(job)
///                 EP1->>-SCH: [Artifacts]
///                 SCH->>JT1: [Artifacts]
///                 JT1->>-JT1: send_artifacts
///             and
///                 loop until dependencies received
///                     JT2->>JT2: recv()
///                 end
///
///                 JT2->>+JT2: build()
///                 JT2->>SCH: schedule(job)
///                 SCH->>+EP1: run(job)
///                 EP1->>-SCH: [Artifacts]
///                 SCH->>JT2: [Artifacts]
///                 JT2->>-JT2: send_artifacts
///             end
///
///         O->>-O: recv(): [Artifacts]
///     O-->>-Caller: [Artifacts]
/// ```
///
/// Because the chart from above is already rather big, the described submit works with only two
/// packages being built on one endpoint.
///
/// The Orchestrator starts the JobTasks in parallel, and they are executed in parallel.
/// Each JobTask receives dependencies until there are no more dependencies to receive. Then, it
/// starts building the job by forwarding the actual job to the scheduler, which in turn schedules
/// the Job on one of the endpoints.
///
///
/// # JobTask
///
/// A [JobTask] is run in parallel to all other JobTasks (concurrently on the tokio runtime).
/// Leveraging the async runtime, it waits until it received all dependencies from it's "child
/// tasks" (the nodes further down in the tree of jobs), which semantically means that it blocks
/// until it can run.
///
/// ```mermaid
/// graph TD
///     r[Receiving deps]
///     dr{All deps received}
///     ae{Any error received}
///     se[Send errors to parent]
///     b[Schedule job]
///     be{error during sched}
///     asum[received artifacts + artifacts from sched]
///     sa[Send artifacts to parent]
///
///     r --> dr
///     dr -->|no| r
///     dr -->|yes| ae
///
///     ae -->|yes| se
///     ae -->|no| b
///     b --> be
///     be -->|yes| se
///     be -->|no| asum
///     asum --> sa
/// ```
///
/// The "root" JobTask sends its artifacts to the orchestrator, which returns them to the caller.
///
pub struct Orchestrator<'a> {
    scheduler: EndpointScheduler,
    progress_generator: ProgressBars,
    merged_stores: MergedStores,
    source_cache: SourceCache,
    jobdag: Dag,
    config: &'a Configuration,
    database: Arc<PgConnection>,
}

#[derive(TypedBuilder)]
pub struct OrchestratorSetup<'a> {
    progress_generator: ProgressBars,
    endpoint_config: Vec<EndpointConfiguration>,
    staging_store: Arc<RwLock<StagingStore>>,
    release_store: Arc<RwLock<ReleaseStore>>,
    source_cache: SourceCache,
    jobdag: Dag,
    database: Arc<PgConnection>,
    submit: dbmodels::Submit,
    log_dir: Option<PathBuf>,
    config: &'a Configuration,
}

impl<'a> OrchestratorSetup<'a> {
    pub async fn setup(self) -> Result<Orchestrator<'a>> {
        let scheduler = EndpointScheduler::setup(
            self.endpoint_config,
            self.staging_store.clone(),
            self.database.clone(),
            self.submit.clone(),
            self.log_dir,
        )
        .await?;

        Ok(Orchestrator {
            scheduler,
            progress_generator: self.progress_generator,
            merged_stores: MergedStores::new(self.release_store, self.staging_store),
            source_cache: self.source_cache,
            jobdag: self.jobdag,
            config: self.config,
            database: self.database,
        })
    }
}

/// Helper type
///
/// Represents a result that came from the run of a job inside a container
///
/// It is either a list of artifacts with the UUID of the job they were produced by,
/// or a UUID and an Error object, where the UUID is the job UUID and the error is the
/// anyhow::Error that was issued.
type JobResult = std::result::Result<HashMap<Uuid, Vec<Artifact>>, HashMap<Uuid, Error>>;

impl<'a> Orchestrator<'a> {
    pub async fn run(self, output: &mut Vec<Artifact>) -> Result<HashMap<Uuid, Error>> {
        let (results, errors) = self.run_tree().await?;
        output.extend(results.into_iter());
        Ok(errors)
    }

    async fn run_tree(self) -> Result<(Vec<Artifact>, HashMap<Uuid, Error>)> {
        let multibar = Arc::new(indicatif::MultiProgress::new());

        // For each job in the jobdag, built a tuple with
        //
        // 1. The receiver that is used by the task to receive results from dependency tasks from
        // 2. The task itself (as a TaskPreparation object)
        // 3. The sender, that can be used to send results to this task
        // 4. An Option<Sender> that this tasks uses to send its results with
        //    This is an Option<> because we need to set it later and the root of the tree needs a
        //    special handling, as this very function will wait on a receiver that gets the results
        //    of the root task
        let jobs: Vec<(Receiver<JobResult>, TaskPreparation, Sender<JobResult>, _)> = self.jobdag
            .iter()
            .map(|jobdef| {
                // We initialize the channel with 100 elements here, as there is unlikely a task
                // that depends on 100 other tasks.
                // Either way, this might be increased in future.
                let (sender, receiver) = tokio::sync::mpsc::channel(100);

                trace!("Creating TaskPreparation object for job {}", jobdef.job.uuid());
                let bar = self.progress_generator.bar();
                let bar = multibar.add(bar);
                bar.set_length(100);
                let tp = TaskPreparation {
                    jobdef,

                    bar,
                    config: self.config,
                    source_cache: &self.source_cache,
                    scheduler: &self.scheduler,
                    merged_stores: &self.merged_stores,
                    database: self.database.clone(),
                };

                (receiver, tp, sender, std::cell::RefCell::new(None as Option<Vec<Sender<JobResult>>>))
            })
            .collect();

        // Associate tasks with their appropriate sender
        //
        // Right now, the tuple yielded from above contains (rx, task, tx, _), where rx and tx belong
        // to eachother.
        // But what we need is the tx (sender) that the task should send its result to, of course.
        //
        // So this algorithm in plain text is:
        //   for each job
        //      find the job that depends on this job
        //      use the sender of the found job and set it as sender for this job
        for job in jobs.iter() {
            if let Some(mut v) = job.3.borrow_mut().as_mut() {
                v.extend({
                jobs.iter()
                    .filter(|j| j.1.jobdef.dependencies.contains(job.1.jobdef.job.uuid()))
                    .map(|j| j.2.clone())
                });
            } else {
                *job.3.borrow_mut() = {
                    let depending_on_job = jobs.iter()
                        .filter(|j| j.1.jobdef.dependencies.contains(job.1.jobdef.job.uuid()))
                        .map(|j| j.2.clone())
                        .collect::<Vec<Sender<JobResult>>>();

                    if depending_on_job.is_empty() {
                        None
                    } else {
                        Some(depending_on_job)
                    }
                };
            }
        }

        // Find the id of the root task
        //
        // By now, all tasks should be associated with their respective sender.
        // Only one has None sender: The task that is the "root" of the tree.
        // By that property, we can find the root task.
        //
        // Here, we copy its uuid, because we need it later.
        let root_job_id = jobs.iter()
            .find(|j| j.3.borrow().is_none())
            .map(|j| j.1.jobdef.job.uuid())
            .ok_or_else(|| anyhow!("Failed to find root task"))?;
        trace!("Root job id = {}", root_job_id);

        // Create a sender and a receiver for the root of the tree
        let (root_sender, mut root_receiver) = tokio::sync::mpsc::channel(100);

        // Make all prepared jobs into real jobs and run them
        //
        // This maps each TaskPreparation with its sender and receiver to a JobTask and calls the
        // async fn JobTask::run() to run the task.
        //
        // The JobTask::run implementation handles the rest, we just have to wait for all futures
        // to succeed.
        let running_jobs = jobs
            .into_iter()
            .map(|prep| {
                trace!("Creating JobTask for = {}", prep.1.jobdef.job.uuid());
                let root_sender = root_sender.clone();
                JobTask {
                    jobdef: prep.1.jobdef,

                    bar: prep.1.bar.clone(),

                    config: prep.1.config,
                    source_cache: prep.1.source_cache,
                    scheduler: prep.1.scheduler,
                    merged_stores: prep.1.merged_stores,
                    database: prep.1.database.clone(),

                    receiver: prep.0,

                    // the sender is set or we need to use the root sender
                    sender: prep.3.into_inner().unwrap_or_else(|| vec![root_sender]),
                }
            })
            .map(|task| task.run())
            .collect::<futures::stream::FuturesUnordered<_>>();
        debug!("Built {} jobs", running_jobs.len());

        let multibar_block = tokio::task::spawn_blocking(move || multibar.join());
        let (_, jobs_result) = tokio::join!(multibar_block, running_jobs.collect::<Result<()>>());
        let _ = jobs_result?;
        match root_receiver.recv().await {
            None                     => Err(anyhow!("No result received...")),
            Some(Ok(results)) => {
                let results = results.into_iter().map(|tpl| tpl.1.into_iter()).flatten().collect();
                Ok((results, HashMap::with_capacity(0)))
            },
            Some(Err(errors))        => Ok((vec![], errors)),
        }
    }
}

/// Helper type: A task with all things attached, but not sender and receivers
///
/// This is the preparation of the JobTask, but without the associated sender and receiver, because
/// it is not mapped to the task yet.
///
/// This simply holds data and does not contain any more functionality
struct TaskPreparation<'a> {
    jobdef: JobDefinition<'a>,

    bar: ProgressBar,

    config: &'a Configuration,
    source_cache: &'a SourceCache,
    scheduler: &'a EndpointScheduler,
    merged_stores: &'a MergedStores,
    database: Arc<PgConnection>,
}

/// Helper type for executing one job task
///
/// This type represents a task for a job that can immediately be executed (see `JobTask::run()`).
struct JobTask<'a> {
    jobdef: JobDefinition<'a>,

    bar: ProgressBar,

    config: &'a Configuration,
    source_cache: &'a SourceCache,
    scheduler: &'a EndpointScheduler,
    merged_stores: &'a MergedStores,
    database: Arc<PgConnection>,

    /// Channel where the dependencies arrive
    receiver: Receiver<JobResult>,

    /// Channel to send the own build outputs to
    sender: Vec<Sender<JobResult>>,
}

impl<'a> JobTask<'a> {

    /// Run the job
    ///
    /// This function runs the job from this object on the scheduler as soon as all dependend jobs
    /// returned successfully.
    async fn run(mut self) -> Result<()> {
        debug!("[{}]: Running", self.jobdef.job.uuid());
        debug!("[{}]: Waiting for dependencies = {:?}", self.jobdef.job.uuid(), {
            self.jobdef.dependencies.iter().map(|u| u.to_string()).collect::<Vec<String>>()
        });

        // A list of job run results from dependencies that were received from the tasks for the
        // dependencies
        let mut received_dependencies: HashMap<Uuid, Vec<Artifact>> = HashMap::new();

        // A list of errors that were received from the tasks for the dependencies
        let mut received_errors: HashMap<Uuid, Error> = HashMap::with_capacity(self.jobdef.dependencies.len());

        // Helper function to check whether all UUIDs are in a list of UUIDs
        let all_dependencies_are_in = |dependency_uuids: &[Uuid], list: &HashMap<Uuid, Vec<_>>| {
            dependency_uuids.iter().all(|dependency_uuid| {
                list.keys().any(|id| id == dependency_uuid)
            })
        };

        // as long as the job definition lists dependencies that are not in the received_dependencies list...
        while !all_dependencies_are_in(&self.jobdef.dependencies, &received_dependencies) {
            // Update the status bar message
            self.bar.set_message({
                &format!("[{} {} {}]: Waiting ({}/{})...",
                    self.jobdef.job.uuid(),
                    self.jobdef.job.package().name(),
                    self.jobdef.job.package().version(),
                    received_dependencies.len(),
                    self.jobdef.dependencies.len())
            });
            trace!("[{}]: Updated bar", self.jobdef.job.uuid());

            trace!("[{}]: receiving...", self.jobdef.job.uuid());
            // receive from the receiver
            let continue_receiving = self.perform_receive(&mut received_dependencies, &mut received_errors).await?;
            if !continue_receiving {
                break;
            }

            trace!("[{}]: Received errors = {:?}", self.jobdef.job.uuid(), received_errors);
            // if there are any errors from child tasks
            if !received_errors.is_empty() {
                // send them to the parent,...
                //
                // We only send to one parent, because it doesn't matter
                // And we know that we have at least one sender
                self.sender[0].send(Err(received_errors)).await;

                // ... and stop operation, because the whole tree will fail anyways.
                return Ok(())
            }
        }

        // receive items until the channel is empty.
        //
        // In the above loop, it could happen that we have all dependencies to run, but there is
        // another job that reports artifacts.
        // We need to collect them, too.
        //
        // This is techically not possible, because in a tree, we need all results from all childs.
        // It just feels better having this in place as well.
        //
        // Sorry, not sorry.
        while self.perform_receive(&mut received_dependencies, &mut received_errors).await? {
            ;
        }

        // Map the list of received dependencies from
        //      Vec<(Uuid, Vec<Artifact>)>
        // to
        //      Vec<Artifact>
        let dependency_artifacts = received_dependencies
            .values()
            .map(|v| v.iter())
            .flatten()
            .cloned()
            .collect();
        trace!("[{}]: Dependency artifacts = {:?}", self.jobdef.job.uuid(), dependency_artifacts);
        self.bar.set_message(&format!("[{} {} {}]: Preparing...",
            self.jobdef.job.uuid(),
            self.jobdef.job.package().name(),
            self.jobdef.job.package().version()
        ));

        // Create a RunnableJob object
        let runnable = RunnableJob::build_from_job(
            &self.jobdef.job,
            self.source_cache,
            self.config,
            dependency_artifacts)?;

        self.bar.set_message(&format!("[{} {} {}]: Scheduling...",
            self.jobdef.job.uuid(),
            self.jobdef.job.package().name(),
            self.jobdef.job.package().version()
        ));
        let job_uuid = *self.jobdef.job.uuid();

        // Schedule the job on the scheduler
        match self.scheduler.schedule_job(runnable, self.bar).await?.run().await {
            // if the scheduler run reports an error,
            // that is an error from the actual execution of the job ...
            Err(e) => {
                trace!("[{}]: Scheduler returned error = {:?}", self.jobdef.job.uuid(), e);
                // ... and we send that to our parent
                //
                // We only send to one parent, because it doesn't matter anymore
                // We know that we have at least one sender available
                let mut errormap = HashMap::with_capacity(1);
                errormap.insert(job_uuid, e);
                self.sender[0].send(Err(errormap)).await?;
            },

            // if the scheduler run reports success,
            // it returns the database artifact objects it created!
            Ok(artifacts) => {
                trace!("[{}]: Scheduler returned artifacts = {:?}", self.jobdef.job.uuid(), artifacts);
                received_dependencies.insert(*self.jobdef.job.uuid(), artifacts);
                for s in self.sender {
                    s.send(Ok(received_dependencies.clone())).await?;
                }
            },
        }

        trace!("[{}]: Finished successfully", self.jobdef.job.uuid());
        Ok(())
    }

    /// Performe a recv() call on the receiving side of the channel
    ///
    /// Put the dependencies you received into the `received_dependencies`, the errors in the
    /// `received_errors`
    ///
    /// Return Ok(true) if we should continue operation
    /// Return Ok(false) if the channel is empty and we're done receiving
    async fn perform_receive(&mut self, received_dependencies: &mut HashMap<Uuid, Vec<Artifact>>, received_errors: &mut HashMap<Uuid, Error>) -> Result<bool> {
        match self.receiver.recv().await {
            Some(Ok(mut v)) => {
                // The task we depend on succeeded and returned an
                // (uuid of the job, [Artifact])
                trace!("[{}]: Received: {:?}", self.jobdef.job.uuid(), v);
                received_dependencies.extend(v);
                Ok(true)
            },
            Some(Err(mut e)) => {
                // The task we depend on failed
                // we log that error for now
                trace!("[{}]: Received: {:?}", self.jobdef.job.uuid(), e);
                received_errors.extend(e);
                Ok(true)
            },
            None => {
                // The task we depend on finished... we must check what we have now...
                trace!("[{}]: Received nothing, channel seems to be empty", self.jobdef.job.uuid());

                // Find all dependencies that we need but which are not received
                let received = received_dependencies.keys().collect::<Vec<_>>();
                let missing_deps: Vec<_> = self.jobdef
                    .dependencies
                    .iter()
                    .filter(|d| !received.contains(d))
                    .collect();
                trace!("[{}]: Missing dependencies = {:?}", self.jobdef.job.uuid(), missing_deps);

                // ... if there are any, error
                if !missing_deps.is_empty() {
                    let missing: Vec<String> = missing_deps.iter().map(|u| u.to_string()).collect();
                    return Err(anyhow!("Childs finished, but dependencies still missing: {:?}", missing))
                } else {
                    // all dependencies are received
                   Ok(false)
                }
            },
        }
    }

}

