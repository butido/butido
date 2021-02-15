//
// Copyright (c) 2020-2021 science+computing ag and other contributors
//
// This program and the accompanying materials are made
// available under the terms of the Eclipse Public License 2.0
// which is available at https://www.eclipse.org/legal/epl-2.0/
//
// SPDX-License-Identifier: EPL-2.0
//

use anyhow::Error;
use anyhow::Context;
use anyhow::Result;
use diesel::prelude::*;
use diesel::PgConnection;
use log::trace;

use crate::db::models::{Endpoint, Image, Package, Submit};
use crate::package::Script;
use crate::schema::jobs;
use crate::schema::jobs::*;
use crate::util::docker::ContainerHash;

#[derive(Debug, Eq, PartialEq, Identifiable, Queryable, Associations)]
#[belongs_to(Submit)]
#[belongs_to(Endpoint)]
#[belongs_to(Package)]
#[belongs_to(Image)]
#[table_name = "jobs"]
pub struct Job {
    pub id: i32,
    pub submit_id: i32,
    pub endpoint_id: i32,
    pub package_id: i32,
    pub image_id: i32,
    pub container_hash: String,
    pub script_text: String,
    pub log_text: String,
    pub uuid: ::uuid::Uuid,
}

#[derive(Debug, Insertable)]
#[table_name = "jobs"]
struct NewJob<'a> {
    pub submit_id: i32,
    pub endpoint_id: i32,
    pub package_id: i32,
    pub image_id: i32,
    pub container_hash: &'a str,
    pub script_text: &'a str,
    pub log_text: &'a str,
    pub uuid: &'a ::uuid::Uuid,
}

impl Job {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        database_connection: &PgConnection,
        job_uuid: &::uuid::Uuid,
        submit: &Submit,
        endpoint: &Endpoint,
        package: &Package,
        image: &Image,
        container: &ContainerHash,
        script: &Script,
        log: &str,
    ) -> Result<Job> {
        let new_job = NewJob {
            uuid: job_uuid,
            submit_id: submit.id,
            endpoint_id: endpoint.id,
            package_id: package.id,
            image_id: image.id,
            container_hash: container.as_ref(),
            script_text: script.as_ref(),
            log_text: log,
        };

        trace!("Creating Job in database: {:?}", new_job);
        diesel::insert_into(jobs::table)
            .values(&new_job)
            .on_conflict_do_nothing()
            .execute(database_connection)
            .context("Creating job in database")?;

        dsl::jobs
            .filter(uuid.eq(job_uuid))
            .first::<Job>(database_connection)
            .with_context(|| format!("Finding created job in database: {}", job_uuid))
            .map_err(Error::from)
    }

    pub fn env(&self, database_connection: &PgConnection) -> Result<Vec<crate::db::models::EnvVar>> {
        use crate::schema;

        schema::job_envs::table
            .inner_join(schema::envvars::table)
            .filter(schema::job_envs::job_id.eq(self.id))
            .select(schema::envvars::all_columns)
            .load::<crate::db::models::EnvVar>(database_connection)
            .map_err(Error::from)
    }
}
