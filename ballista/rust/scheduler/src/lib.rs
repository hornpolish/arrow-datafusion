// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Ballista Scheduler Process
//!
//! This crate contains the Ballista scheduler process. Refer to <https://crates.io/crates/ballista> for
//! documentation.

pub mod api;
pub mod planner;
#[cfg(feature = "sled")]
mod standalone;
pub mod state;
#[cfg(feature = "sled")]
pub use standalone::new_standalone_scheduler;

#[cfg(test)]
pub mod test_utils;

// include the generated protobuf source as a submodule
#[allow(clippy::all)]
pub mod externalscaler {
    include!(concat!(env!("OUT_DIR"), "/externalscaler.rs"));
}

use std::{convert::TryInto, sync::Arc};
use std::{fmt, net::IpAddr};

use ballista_core::serde::protobuf::{
    execute_query_params::Query, executor_registration::OptionalHost, job_status,
    scheduler_grpc_server::SchedulerGrpc, task_status, ExecuteQueryParams,
    ExecuteQueryResult, FailedJob, FilePartitionMetadata, FileType,
    GetFileMetadataParams, GetFileMetadataResult, GetJobStatusParams, GetJobStatusResult,
    JobStatus, PartitionId, PollWorkParams, PollWorkResult, QueuedJob, RunningJob,
    TaskDefinition, TaskStatus,
};
use ballista_core::serde::scheduler::ExecutorMeta;

use clap::arg_enum;
use datafusion::physical_plan::ExecutionPlan;
#[cfg(feature = "sled")]
extern crate sled_package as sled;

// an enum used to configure the backend
// needs to be visible to code generated by configure_me
arg_enum! {
    #[derive(Debug, serde::Deserialize)]
    pub enum ConfigBackend {
        Etcd,
        Standalone
    }
}

impl parse_arg::ParseArgFromStr for ConfigBackend {
    fn describe_type<W: fmt::Write>(mut writer: W) -> fmt::Result {
        write!(writer, "The configuration backend for the scheduler")
    }
}

use crate::externalscaler::{
    external_scaler_server::ExternalScaler, GetMetricSpecResponse, GetMetricsRequest,
    GetMetricsResponse, IsActiveResponse, MetricSpec, MetricValue, ScaledObjectRef,
};
use crate::planner::DistributedPlanner;

use log::{debug, error, info, warn};
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use tonic::{Request, Response, Status};

use self::state::{ConfigBackendClient, SchedulerState};
use ballista_core::config::BallistaConfig;
use ballista_core::execution_plans::ShuffleWriterExec;
use ballista_core::serde::scheduler::to_proto::hash_partitioning_to_proto;
use datafusion::physical_plan::parquet::ParquetExec;
use datafusion::prelude::{ExecutionConfig, ExecutionContext};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct SchedulerServer {
    caller_ip: IpAddr,
    pub(crate) state: Arc<SchedulerState>,
    start_time: u128,
}

impl SchedulerServer {
    pub fn new(
        config: Arc<dyn ConfigBackendClient>,
        namespace: String,
        caller_ip: IpAddr,
    ) -> Self {
        let state = Arc::new(SchedulerState::new(config, namespace));
        let state_clone = state.clone();

        // TODO: we should elect a leader in the scheduler cluster and run this only in the leader
        tokio::spawn(async move { state_clone.synchronize_job_status_loop().await });

        Self {
            caller_ip,
            state,
            start_time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        }
    }
}

const INFLIGHT_TASKS_METRIC_NAME: &str = "inflight_tasks";

#[tonic::async_trait]
impl ExternalScaler for SchedulerServer {
    async fn is_active(
        &self,
        _request: Request<ScaledObjectRef>,
    ) -> Result<Response<IsActiveResponse>, tonic::Status> {
        let tasks = self.state.get_all_tasks().await.map_err(|e| {
            let msg = format!("Error reading tasks: {}", e);
            error!("{}", msg);
            tonic::Status::internal(msg)
        })?;
        let result = tasks.iter().any(|(_key, task)| {
            !matches!(
                task.status,
                Some(task_status::Status::Completed(_))
                    | Some(task_status::Status::Failed(_))
            )
        });
        debug!("Are there active tasks? {}", result);
        Ok(Response::new(IsActiveResponse { result }))
    }

    async fn get_metric_spec(
        &self,
        _request: Request<ScaledObjectRef>,
    ) -> Result<Response<GetMetricSpecResponse>, tonic::Status> {
        Ok(Response::new(GetMetricSpecResponse {
            metric_specs: vec![MetricSpec {
                metric_name: INFLIGHT_TASKS_METRIC_NAME.to_string(),
                target_size: 1,
            }],
        }))
    }

    async fn get_metrics(
        &self,
        _request: Request<GetMetricsRequest>,
    ) -> Result<Response<GetMetricsResponse>, tonic::Status> {
        Ok(Response::new(GetMetricsResponse {
            metric_values: vec![MetricValue {
                metric_name: INFLIGHT_TASKS_METRIC_NAME.to_string(),
                metric_value: 10000000, // A very high number to saturate the HPA
            }],
        }))
    }
}

#[tonic::async_trait]
impl SchedulerGrpc for SchedulerServer {
    async fn poll_work(
        &self,
        request: Request<PollWorkParams>,
    ) -> std::result::Result<Response<PollWorkResult>, tonic::Status> {
        if let PollWorkParams {
            metadata: Some(metadata),
            can_accept_task,
            task_status,
        } = request.into_inner()
        {
            debug!("Received poll_work request for {:?}", metadata);
            let metadata: ExecutorMeta = ExecutorMeta {
                id: metadata.id,
                host: metadata
                    .optional_host
                    .map(|h| match h {
                        OptionalHost::Host(host) => host,
                    })
                    .unwrap_or_else(|| self.caller_ip.to_string()),
                port: metadata.port as u16,
            };
            let mut lock = self.state.lock().await.map_err(|e| {
                let msg = format!("Could not lock the state: {}", e);
                error!("{}", msg);
                tonic::Status::internal(msg)
            })?;
            self.state
                .save_executor_metadata(metadata.clone())
                .await
                .map_err(|e| {
                    let msg = format!("Could not save executor metadata: {}", e);
                    error!("{}", msg);
                    tonic::Status::internal(msg)
                })?;
            for task_status in task_status {
                self.state
                    .save_task_status(&task_status)
                    .await
                    .map_err(|e| {
                        let msg = format!("Could not save task status: {}", e);
                        error!("{}", msg);
                        tonic::Status::internal(msg)
                    })?;
            }
            let task: Result<Option<_>, Status> = if can_accept_task {
                let plan = self
                    .state
                    .assign_next_schedulable_task(&metadata.id)
                    .await
                    .map_err(|e| {
                        let msg = format!("Error finding next assignable task: {}", e);
                        error!("{}", msg);
                        tonic::Status::internal(msg)
                    })?;
                if let Some((task, _plan)) = &plan {
                    let partition_id = task.partition_id.as_ref().unwrap();
                    info!(
                        "Sending new task to {}: {}/{}/{}",
                        metadata.id,
                        partition_id.job_id,
                        partition_id.stage_id,
                        partition_id.partition_id
                    );
                }
                match plan {
                    Some((status, plan)) => {
                        let plan_clone = plan.clone();
                        let output_partitioning = if let Some(shuffle_writer) =
                            plan_clone.as_any().downcast_ref::<ShuffleWriterExec>()
                        {
                            shuffle_writer.shuffle_output_partitioning()
                        } else {
                            return Err(Status::invalid_argument(format!(
                                "Task root plan was not a ShuffleWriterExec: {:?}",
                                plan_clone
                            )));
                        };
                        Ok(Some(TaskDefinition {
                            plan: Some(plan.try_into().unwrap()),
                            task_id: status.partition_id,
                            output_partitioning: hash_partitioning_to_proto(
                                output_partitioning,
                            )
                            .map_err(|_| Status::internal("TBD".to_string()))?,
                        }))
                    }
                    None => Ok(None),
                }
            } else {
                Ok(None)
            };
            lock.unlock().await;
            Ok(Response::new(PollWorkResult { task: task? }))
        } else {
            warn!("Received invalid executor poll_work request");
            Err(tonic::Status::invalid_argument(
                "Missing metadata in request",
            ))
        }
    }

    async fn get_file_metadata(
        &self,
        request: Request<GetFileMetadataParams>,
    ) -> std::result::Result<Response<GetFileMetadataResult>, tonic::Status> {
        let GetFileMetadataParams { path, file_type } = request.into_inner();

        let file_type: FileType = file_type.try_into().map_err(|e| {
            let msg = format!("Error reading request: {}", e);
            error!("{}", msg);
            tonic::Status::internal(msg)
        })?;

        match file_type {
            FileType::Parquet => {
                let parquet_exec =
                    ParquetExec::try_from_path(&path, None, None, 1024, 1, None)
                        .map_err(|e| {
                            let msg = format!("Error opening parquet files: {}", e);
                            error!("{}", msg);
                            tonic::Status::internal(msg)
                        })?;

                //TODO include statistics and any other info needed to reconstruct ParquetExec
                Ok(Response::new(GetFileMetadataResult {
                    schema: Some(parquet_exec.schema().as_ref().into()),
                    partitions: parquet_exec
                        .partitions()
                        .iter()
                        .map(|part| FilePartitionMetadata {
                            filename: part.filenames().to_vec(),
                        })
                        .collect(),
                }))
            }
            //TODO implement for CSV
            _ => Err(tonic::Status::unimplemented(
                "get_file_metadata unsupported file type",
            )),
        }
    }

    async fn execute_query(
        &self,
        request: Request<ExecuteQueryParams>,
    ) -> std::result::Result<Response<ExecuteQueryResult>, tonic::Status> {
        if let ExecuteQueryParams {
            query: Some(query),
            settings,
        } = request.into_inner()
        {
            // parse config
            let mut config_builder = BallistaConfig::builder();
            for kv_pair in &settings {
                config_builder = config_builder.set(&kv_pair.key, &kv_pair.value);
            }
            let config = config_builder.build().map_err(|e| {
                let msg = format!("Could not parse configs: {}", e);
                error!("{}", msg);
                tonic::Status::internal(msg)
            })?;

            let plan = match query {
                Query::LogicalPlan(logical_plan) => {
                    // parse protobuf
                    (&logical_plan).try_into().map_err(|e| {
                        let msg = format!("Could not parse logical plan protobuf: {}", e);
                        error!("{}", msg);
                        tonic::Status::internal(msg)
                    })?
                }
                Query::Sql(sql) => {
                    //TODO we can't just create a new context because we need a context that has
                    // tables registered from previous SQL statements that have been executed
                    let mut ctx = create_datafusion_context(&config);
                    let df = ctx.sql(&sql).map_err(|e| {
                        let msg = format!("Error parsing SQL: {}", e);
                        error!("{}", msg);
                        tonic::Status::internal(msg)
                    })?;
                    df.to_logical_plan()
                }
            };
            debug!("Received plan for execution: {:?}", plan);
            let job_id: String = {
                let mut rng = thread_rng();
                std::iter::repeat(())
                    .map(|()| rng.sample(Alphanumeric))
                    .map(char::from)
                    .take(7)
                    .collect()
            };

            // Save placeholder job metadata
            self.state
                .save_job_metadata(
                    &job_id,
                    &JobStatus {
                        status: Some(job_status::Status::Queued(QueuedJob {})),
                    },
                )
                .await
                .map_err(|e| {
                    tonic::Status::internal(format!("Could not save job metadata: {}", e))
                })?;

            let state = self.state.clone();
            let job_id_spawn = job_id.clone();
            tokio::spawn(async move {
                // create physical plan using DataFusion
                let datafusion_ctx = create_datafusion_context(&config);
                macro_rules! fail_job {
                    ($code :expr) => {{
                        match $code {
                            Err(error) => {
                                warn!("Job {} failed with {}", job_id_spawn, error);
                                state
                                    .save_job_metadata(
                                        &job_id_spawn,
                                        &JobStatus {
                                            status: Some(job_status::Status::Failed(
                                                FailedJob {
                                                    error: format!("{}", error),
                                                },
                                            )),
                                        },
                                    )
                                    .await
                                    .unwrap();
                                return;
                            }
                            Ok(value) => value,
                        }
                    }};
                }

                let start = Instant::now();

                let optimized_plan =
                    fail_job!(datafusion_ctx.optimize(&plan).map_err(|e| {
                        let msg =
                            format!("Could not create optimized logical plan: {}", e);
                        error!("{}", msg);
                        tonic::Status::internal(msg)
                    }));

                debug!("Calculated optimized plan: {:?}", optimized_plan);

                let plan = fail_job!(datafusion_ctx
                    .create_physical_plan(&optimized_plan)
                    .map_err(|e| {
                        let msg = format!("Could not create physical plan: {}", e);
                        error!("{}", msg);
                        tonic::Status::internal(msg)
                    }));

                info!(
                    "DataFusion created physical plan in {} milliseconds",
                    start.elapsed().as_millis(),
                );

                // create distributed physical plan using Ballista
                if let Err(e) = state
                    .save_job_metadata(
                        &job_id_spawn,
                        &JobStatus {
                            status: Some(job_status::Status::Running(RunningJob {})),
                        },
                    )
                    .await
                {
                    warn!(
                        "Could not update job {} status to running: {}",
                        job_id_spawn, e
                    );
                }
                let mut planner = DistributedPlanner::new();
                let stages = fail_job!(planner
                    .plan_query_stages(&job_id_spawn, plan)
                    .map_err(|e| {
                        let msg = format!("Could not plan query stages: {}", e);
                        error!("{}", msg);
                        tonic::Status::internal(msg)
                    }));

                // save stages into state
                for shuffle_writer in stages {
                    fail_job!(state
                        .save_stage_plan(
                            &job_id_spawn,
                            shuffle_writer.stage_id(),
                            shuffle_writer.clone()
                        )
                        .await
                        .map_err(|e| {
                            let msg = format!("Could not save stage plan: {}", e);
                            error!("{}", msg);
                            tonic::Status::internal(msg)
                        }));
                    let num_partitions =
                        shuffle_writer.output_partitioning().partition_count();
                    for partition_id in 0..num_partitions {
                        let pending_status = TaskStatus {
                            partition_id: Some(PartitionId {
                                job_id: job_id_spawn.clone(),
                                stage_id: shuffle_writer.stage_id() as u32,
                                partition_id: partition_id as u32,
                            }),
                            status: None,
                        };
                        fail_job!(state.save_task_status(&pending_status).await.map_err(
                            |e| {
                                let msg = format!("Could not save task status: {}", e);
                                error!("{}", msg);
                                tonic::Status::internal(msg)
                            }
                        ));
                    }
                }
            });

            Ok(Response::new(ExecuteQueryResult { job_id }))
        } else {
            Err(tonic::Status::internal("Error parsing request"))
        }
    }

    async fn get_job_status(
        &self,
        request: Request<GetJobStatusParams>,
    ) -> std::result::Result<Response<GetJobStatusResult>, tonic::Status> {
        let job_id = request.into_inner().job_id;
        debug!("Received get_job_status request for job {}", job_id);
        let job_meta = self.state.get_job_metadata(&job_id).await.map_err(|e| {
            let msg = format!("Error reading job metadata: {}", e);
            error!("{}", msg);
            tonic::Status::internal(msg)
        })?;
        Ok(Response::new(GetJobStatusResult {
            status: Some(job_meta),
        }))
    }
}

/// Create a DataFusion context that is compatible with Ballista
pub fn create_datafusion_context(config: &BallistaConfig) -> ExecutionContext {
    let config =
        ExecutionConfig::new().with_concurrency(config.default_shuffle_partitions());
    ExecutionContext::with_config(config)
}

#[cfg(all(test, feature = "sled"))]
mod test {
    use std::{
        net::{IpAddr, Ipv4Addr},
        sync::Arc,
    };

    use tonic::Request;

    use ballista_core::error::BallistaError;
    use ballista_core::serde::protobuf::{
        executor_registration::OptionalHost, ExecutorRegistration, PollWorkParams,
    };

    use super::{
        state::{SchedulerState, StandaloneClient},
        SchedulerGrpc, SchedulerServer,
    };

    #[tokio::test]
    async fn test_poll_work() -> Result<(), BallistaError> {
        let state = Arc::new(StandaloneClient::try_new_temporary()?);
        let namespace = "default";
        let scheduler = SchedulerServer::new(
            state.clone(),
            namespace.to_owned(),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        );
        let state = SchedulerState::new(state, namespace.to_string());
        let exec_meta = ExecutorRegistration {
            id: "abc".to_owned(),
            optional_host: Some(OptionalHost::Host("".to_owned())),
            port: 0,
        };
        let request: Request<PollWorkParams> = Request::new(PollWorkParams {
            metadata: Some(exec_meta.clone()),
            can_accept_task: false,
            task_status: vec![],
        });
        let response = scheduler
            .poll_work(request)
            .await
            .expect("Received error response")
            .into_inner();
        // no response task since we told the scheduler we didn't want to accept one
        assert!(response.task.is_none());
        // executor should be registered
        assert_eq!(state.get_executors_metadata().await.unwrap().len(), 1);

        let request: Request<PollWorkParams> = Request::new(PollWorkParams {
            metadata: Some(exec_meta.clone()),
            can_accept_task: true,
            task_status: vec![],
        });
        let response = scheduler
            .poll_work(request)
            .await
            .expect("Received error response")
            .into_inner();
        // still no response task since there are no tasks in the scheduelr
        assert!(response.task.is_none());
        // executor should be registered
        assert_eq!(state.get_executors_metadata().await.unwrap().len(), 1);
        Ok(())
    }
}
