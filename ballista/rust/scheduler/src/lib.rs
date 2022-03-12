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

#![doc = include_str ! ("../README.md")]

pub mod api;
pub mod planner;
#[cfg(feature = "sled")]
mod standalone;
pub mod state;

use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::file_format::FileFormat;
use datafusion::datasource::object_store::{local::LocalFileSystem, ObjectStore};

use futures::StreamExt;

#[cfg(feature = "sled")]
pub use standalone::new_standalone_scheduler;

#[cfg(test)]
pub mod test_utils;

// include the generated protobuf source as a submodule
#[allow(clippy::all)]
pub mod externalscaler {
    include!(concat!(env!("OUT_DIR"), "/externalscaler.rs"));
}

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::marker::PhantomData;
use std::{convert::TryInto, sync::Arc};

use ballista_core::serde::protobuf::{
    execute_query_params::Query, executor_registration::OptionalHost, job_status,
    scheduler_grpc_server::SchedulerGrpc, task_status, ExecuteQueryParams,
    ExecuteQueryResult, ExecutorHeartbeat, FailedJob, FileType, GetFileMetadataParams,
    GetFileMetadataResult, GetJobStatusParams, GetJobStatusResult, HeartBeatParams,
    HeartBeatResult, JobStatus, LaunchTaskParams, PartitionId, PollWorkParams,
    PollWorkResult, QueuedJob, RegisterExecutorParams, RegisterExecutorResult,
    RunningJob, TaskDefinition, TaskStatus, UpdateTaskStatusParams,
    UpdateTaskStatusResult,
};
use ballista_core::serde::scheduler::{ExecutorData, ExecutorMetadata};

use clap::ArgEnum;
use datafusion::physical_plan::ExecutionPlan;

#[cfg(feature = "sled")]
extern crate sled_package as sled;

// an enum used to configure the backend
// needs to be visible to code generated by configure_me
#[derive(Debug, Clone, ArgEnum, serde::Deserialize)]
pub enum ConfigBackend {
    Etcd,
    Standalone,
}

impl std::str::FromStr for ConfigBackend {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        ArgEnum::from_str(s, true)
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

use log::{debug, error, info, trace, warn};
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use tonic::{Request, Response, Status};

use self::state::{ConfigBackendClient, SchedulerState};
use anyhow::Context;
use ballista_core::config::{BallistaConfig, TaskSchedulingPolicy};
use ballista_core::error::BallistaError;
use ballista_core::execution_plans::ShuffleWriterExec;
use ballista_core::plugin::udf::get_udf_plugin_manager;
use ballista_core::serde::protobuf::executor_grpc_client::ExecutorGrpcClient;
use ballista_core::serde::scheduler::to_proto::hash_partitioning_to_proto;
use ballista_core::serde::{AsExecutionPlan, AsLogicalPlan, BallistaCodec};
use ballista_core::utils::load_udf_from_plugin;
use datafusion::prelude::{ExecutionConfig, ExecutionContext};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, RwLock};
use tonic::transport::Channel;

type ExecutorsClient = Arc<RwLock<HashMap<String, ExecutorGrpcClient<Channel>>>>;

#[derive(Clone)]
pub struct SchedulerServer<T: 'static + AsLogicalPlan, U: 'static + AsExecutionPlan> {
    pub(crate) state: Arc<SchedulerState<T, U>>,
    start_time: u128,
    policy: TaskSchedulingPolicy,
    scheduler_env: Option<SchedulerEnv>,
    executors_client: Option<ExecutorsClient>,
    ctx: Arc<RwLock<ExecutionContext>>,
    codec: BallistaCodec<T, U>,
}

#[derive(Clone)]
pub struct SchedulerEnv {
    pub tx_job: mpsc::Sender<String>,
}

impl<T: 'static + AsLogicalPlan, U: 'static + AsExecutionPlan> SchedulerServer<T, U> {
    pub fn new(
        config: Arc<dyn ConfigBackendClient>,
        namespace: String,
        ctx: Arc<RwLock<ExecutionContext>>,
        codec: BallistaCodec<T, U>,
    ) -> Self {
        SchedulerServer::new_with_policy(
            config,
            namespace,
            TaskSchedulingPolicy::PullStaged,
            None,
            ctx,
            codec,
        )
    }

    pub fn new_with_policy(
        config: Arc<dyn ConfigBackendClient>,
        namespace: String,
        policy: TaskSchedulingPolicy,
        scheduler_env: Option<SchedulerEnv>,
        ctx: Arc<RwLock<ExecutionContext>>,
        codec: BallistaCodec<T, U>,
    ) -> Self {
        let state = Arc::new(SchedulerState::new(config, namespace, codec.clone()));

        let executors_client = if matches!(policy, TaskSchedulingPolicy::PushStaged) {
            Some(Arc::new(RwLock::new(HashMap::new())))
        } else {
            None
        };
        Self {
            state,
            start_time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            policy,
            scheduler_env,
            executors_client,
            ctx,
            codec,
        }
    }

    pub async fn init(&self) -> Result<(), BallistaError> {
        let ctx = self.ctx.read().await;
        self.state.init(&ctx).await?;

        Ok(())
    }

    async fn schedule_job(&self, job_id: String) -> Result<(), BallistaError> {
        let mut available_executors = self.state.get_available_executors_data();

        // In case of there's no enough resources, reschedule the tasks of the job
        if available_executors.is_empty() {
            let tx_job = self.scheduler_env.as_ref().unwrap().tx_job.clone();
            // TODO Maybe it's better to use an exclusive runtime for this kind task scheduling
            warn!("Not enough available executors for task running");
            tokio::time::sleep(Duration::from_millis(100)).await;
            tx_job.send(job_id).await.unwrap();
            return Ok(());
        }

        let (tasks_assigment, num_tasks) =
            self.fetch_tasks(&mut available_executors, &job_id).await?;
        if num_tasks > 0 {
            for (idx_executor, tasks) in tasks_assigment.into_iter().enumerate() {
                if !tasks.is_empty() {
                    let executor_data = &available_executors[idx_executor];
                    debug!(
                        "Start to launch tasks {:?} to executor {:?}",
                        tasks, executor_data.executor_id
                    );
                    let mut client = {
                        let clients =
                            self.executors_client.as_ref().unwrap().read().await;
                        info!("Size of executor clients: {:?}", clients.len());
                        clients.get(&executor_data.executor_id).unwrap().clone()
                    };
                    // Update the resources first
                    self.state.save_executor_data(executor_data.clone());
                    // TODO check whether launching task is successful or not
                    client.launch_task(LaunchTaskParams { task: tasks }).await?;
                } else {
                    // Since the task assignment policy is round robin,
                    // if find tasks for one executor is empty, just break fast
                    break;
                }
            }
            return Ok(());
        }

        Ok(())
    }

    async fn fetch_tasks(
        &self,
        available_executors: &mut Vec<ExecutorData>,
        job_id: &str,
    ) -> Result<(Vec<Vec<TaskDefinition>>, usize), BallistaError> {
        let mut ret: Vec<Vec<TaskDefinition>> =
            Vec::with_capacity(available_executors.len());
        for _idx in 0..available_executors.len() {
            ret.push(Vec::new());
        }
        let mut num_tasks = 0;
        loop {
            info!("Go inside fetching task loop");
            let mut has_tasks = true;
            for (idx, executor) in available_executors.iter_mut().enumerate() {
                if executor.available_task_slots == 0 {
                    break;
                }
                let plan = self
                    .state
                    .assign_next_schedulable_job_task(&executor.executor_id, job_id)
                    .await
                    .map_err(|e| {
                        let msg = format!("Error finding next assignable task: {}", e);
                        error!("{}", msg);
                        tonic::Status::internal(msg)
                    })?;
                if let Some((task, _plan)) = &plan {
                    let task_id = task.task_id.as_ref().unwrap();
                    info!(
                        "Sending new task to {}: {}/{}/{}",
                        executor.executor_id,
                        task_id.job_id,
                        task_id.stage_id,
                        task_id.partition_id
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
                            return Err(BallistaError::General(format!(
                                "Task root plan was not a ShuffleWriterExec: {:?}",
                                plan_clone
                            )));
                        };

                        let mut buf: Vec<u8> = vec![];
                        U::try_from_physical_plan(
                            plan,
                            self.codec.physical_extension_codec(),
                        )
                        .and_then(|m| m.try_encode(&mut buf))
                        .map_err(|e| {
                            Status::internal(format!(
                                "error serializing execution plan: {:?}",
                                e
                            ))
                        })?;

                        ret[idx].push(TaskDefinition {
                            plan: buf,
                            task_id: status.task_id,
                            output_partitioning: hash_partitioning_to_proto(
                                output_partitioning,
                            )
                            .map_err(|_| Status::internal("TBD".to_string()))?,
                        });
                        executor.available_task_slots -= 1;
                        num_tasks += 1;
                    }
                    _ => {
                        // Indicate there's no more tasks to be scheduled
                        has_tasks = false;
                        break;
                    }
                }
            }
            if !has_tasks {
                break;
            }
            let has_executors =
                available_executors.get(0).unwrap().available_task_slots > 0;
            if !has_executors {
                break;
            }
        }
        Ok((ret, num_tasks))
    }
}

pub struct TaskScheduler<T: 'static + AsLogicalPlan, U: 'static + AsExecutionPlan> {
    scheduler_server: Arc<SchedulerServer<T, U>>,
    plan_repr: PhantomData<T>,
    exec_repr: PhantomData<U>,
}

impl<T: 'static + AsLogicalPlan, U: 'static + AsExecutionPlan> TaskScheduler<T, U> {
    pub fn new(scheduler_server: Arc<SchedulerServer<T, U>>) -> Self {
        Self {
            scheduler_server,
            plan_repr: PhantomData,
            exec_repr: PhantomData,
        }
    }

    pub fn start(&self, mut rx_job: mpsc::Receiver<String>) {
        let scheduler_server = self.scheduler_server.clone();
        tokio::spawn(async move {
            info!("Starting the task scheduler");
            loop {
                let job_id = rx_job.recv().await.unwrap();
                info!("Fetch job {:?} to be scheduled", job_id.clone());

                let server = scheduler_server.clone();
                server.schedule_job(job_id).await.unwrap();
            }
        });
    }
}

const INFLIGHT_TASKS_METRIC_NAME: &str = "inflight_tasks";

#[tonic::async_trait]
impl<T: 'static + AsLogicalPlan, U: 'static + AsExecutionPlan> ExternalScaler
    for SchedulerServer<T, U>
{
    async fn is_active(
        &self,
        _request: Request<ScaledObjectRef>,
    ) -> Result<Response<IsActiveResponse>, tonic::Status> {
        let tasks = self.state.get_all_tasks();
        let result = tasks.iter().any(|task| {
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
impl<T: 'static + AsLogicalPlan, U: 'static + AsExecutionPlan> SchedulerGrpc
    for SchedulerServer<T, U>
{
    async fn poll_work(
        &self,
        request: Request<PollWorkParams>,
    ) -> std::result::Result<Response<PollWorkResult>, tonic::Status> {
        if let TaskSchedulingPolicy::PushStaged = self.policy {
            error!("Poll work interface is not supported for push-based task scheduling");
            return Err(tonic::Status::failed_precondition(
                "Bad request because poll work is not supported for push-based task scheduling",
            ));
        }
        let remote_addr = request.remote_addr();
        if let PollWorkParams {
            metadata: Some(metadata),
            can_accept_task,
            task_status,
        } = request.into_inner()
        {
            debug!("Received poll_work request for {:?}", metadata);
            let metadata = ExecutorMetadata {
                id: metadata.id,
                host: metadata
                    .optional_host
                    .map(|h| match h {
                        OptionalHost::Host(host) => host,
                    })
                    .unwrap_or_else(|| remote_addr.unwrap().ip().to_string()),
                port: metadata.port as u16,
                grpc_port: metadata.grpc_port as u16,
                specification: metadata.specification.unwrap().into(),
            };
            let executor_heartbeat = ExecutorHeartbeat {
                executor_id: metadata.id.clone(),
                timestamp: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Time went backwards")
                    .as_secs(),
                state: None,
            };
            // In case that it's the first time to poll work, do registration
            if let Some(_executor_meta) = self.state.get_executor_metadata(&metadata.id) {
            } else {
                self.state
                    .save_executor_metadata(metadata.clone())
                    .await
                    .map_err(|e| {
                        let msg = format!("Could not save executor metadata: {}", e);
                        error!("{}", msg);
                        tonic::Status::internal(msg)
                    })?;
            }
            self.state.save_executor_heartbeat(executor_heartbeat);
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
                    let task_id = task.task_id.as_ref().unwrap();
                    info!(
                        "Sending new task to {}: {}/{}/{}",
                        metadata.id,
                        task_id.job_id,
                        task_id.stage_id,
                        task_id.partition_id
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
                        let mut buf: Vec<u8> = vec![];
                        U::try_from_physical_plan(
                            plan,
                            self.codec.physical_extension_codec(),
                        )
                        .and_then(|m| m.try_encode(&mut buf))
                        .map_err(|e| {
                            Status::internal(format!(
                                "error serializing execution plan: {:?}",
                                e
                            ))
                        })?;
                        Ok(Some(TaskDefinition {
                            plan: buf,
                            task_id: status.task_id,
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
            Ok(Response::new(PollWorkResult { task: task? }))
        } else {
            warn!("Received invalid executor poll_work request");
            Err(tonic::Status::invalid_argument(
                "Missing metadata in request",
            ))
        }
    }

    async fn register_executor(
        &self,
        request: Request<RegisterExecutorParams>,
    ) -> Result<Response<RegisterExecutorResult>, Status> {
        let remote_addr = request.remote_addr();
        if let RegisterExecutorParams {
            metadata: Some(metadata),
        } = request.into_inner()
        {
            info!("Received register executor request for {:?}", metadata);
            let metadata = ExecutorMetadata {
                id: metadata.id,
                host: metadata
                    .optional_host
                    .map(|h| match h {
                        OptionalHost::Host(host) => host,
                    })
                    .unwrap_or_else(|| remote_addr.unwrap().ip().to_string()),
                port: metadata.port as u16,
                grpc_port: metadata.grpc_port as u16,
                specification: metadata.specification.unwrap().into(),
            };
            // Check whether the executor starts the grpc service
            {
                let executor_url =
                    format!("http://{}:{}", metadata.host, metadata.grpc_port);
                info!("Connect to executor {:?}", executor_url);
                let executor_client = ExecutorGrpcClient::connect(executor_url)
                    .await
                    .context("Could not connect to executor")
                    .map_err(|e| tonic::Status::internal(format!("{:?}", e)))?;
                let mut clients = self.executors_client.as_ref().unwrap().write().await;
                // TODO check duplicated registration
                clients.insert(metadata.id.clone(), executor_client);
                info!("Size of executor clients: {:?}", clients.len());
            }
            self.state
                .save_executor_metadata(metadata.clone())
                .await
                .map_err(|e| {
                    let msg = format!("Could not save executor metadata: {}", e);
                    error!("{}", msg);
                    tonic::Status::internal(msg)
                })?;
            let executor_data = ExecutorData {
                executor_id: metadata.id.clone(),
                total_task_slots: metadata.specification.task_slots,
                available_task_slots: metadata.specification.task_slots,
            };
            self.state.save_executor_data(executor_data);
            Ok(Response::new(RegisterExecutorResult { success: true }))
        } else {
            warn!("Received invalid register executor request");
            Err(tonic::Status::invalid_argument(
                "Missing metadata in request",
            ))
        }
    }

    async fn heart_beat_from_executor(
        &self,
        request: Request<HeartBeatParams>,
    ) -> Result<Response<HeartBeatResult>, Status> {
        let HeartBeatParams { executor_id, state } = request.into_inner();

        debug!("Received heart beat request for {:?}", executor_id);
        trace!("Related executor state is {:?}", state);
        let executor_heartbeat = ExecutorHeartbeat {
            executor_id,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards")
                .as_secs(),
            state,
        };
        self.state.save_executor_heartbeat(executor_heartbeat);
        Ok(Response::new(HeartBeatResult { reregister: false }))
    }

    async fn update_task_status(
        &self,
        request: Request<UpdateTaskStatusParams>,
    ) -> Result<Response<UpdateTaskStatusResult>, Status> {
        let UpdateTaskStatusParams {
            executor_id,
            task_status,
        } = request.into_inner();

        debug!(
            "Received task status update request for executor {:?}",
            executor_id
        );
        trace!("Related task status is {:?}", task_status);
        let mut jobs = HashSet::new();
        {
            let num_tasks = task_status.len();
            for task_status in task_status {
                self.state
                    .save_task_status(&task_status)
                    .await
                    .map_err(|e| {
                        let msg = format!("Could not save task status: {}", e);
                        error!("{}", msg);
                        tonic::Status::internal(msg)
                    })?;
                if let Some(task_id) = task_status.task_id {
                    jobs.insert(task_id.job_id.clone());
                }
            }
            if let Some(mut executor_data) = self.state.get_executor_data(&executor_id) {
                executor_data.available_task_slots += num_tasks as u32;
                self.state.save_executor_data(executor_data);
            } else {
                error!("Fail to get executor data for {:?}", &executor_id);
            }
        }
        if let Some(scheduler_env) = self.scheduler_env.as_ref() {
            let tx_job = scheduler_env.tx_job.clone();
            for job_id in jobs {
                tx_job.send(job_id.clone()).await.map_err(|e| {
                    let msg = format!(
                        "Could not send job {} to the channel due to {:?}",
                        &job_id, e
                    );
                    error!("{}", msg);
                    tonic::Status::internal(msg)
                })?;
            }
        }
        Ok(Response::new(UpdateTaskStatusResult { success: true }))
    }

    async fn get_file_metadata(
        &self,
        request: Request<GetFileMetadataParams>,
    ) -> std::result::Result<Response<GetFileMetadataResult>, tonic::Status> {
        // TODO support multiple object stores
        let obj_store = LocalFileSystem {};
        // TODO shouldn't this take a ListingOption object as input?

        let GetFileMetadataParams { path, file_type } = request.into_inner();

        let file_type: FileType = file_type.try_into().map_err(|e| {
            let msg = format!("Error reading request: {}", e);
            error!("{}", msg);
            tonic::Status::internal(msg)
        })?;

        let file_format: Arc<dyn FileFormat> = match file_type {
            FileType::Parquet => Ok(Arc::new(ParquetFormat::default())),
            //TODO implement for CSV
            _ => Err(tonic::Status::unimplemented(
                "get_file_metadata unsupported file type",
            )),
        }?;

        let file_metas = obj_store.list_file(&path).await.map_err(|e| {
            let msg = format!("Error listing files: {}", e);
            error!("{}", msg);
            tonic::Status::internal(msg)
        })?;

        let obj_readers = file_metas.map(move |f| obj_store.file_reader(f?.sized_file));

        let schema = file_format
            .infer_schema(Box::pin(obj_readers))
            .await
            .map_err(|e| {
                let msg = format!("Error infering schema: {}", e);
                error!("{}", msg);
                tonic::Status::internal(msg)
            })?;

        Ok(Response::new(GetFileMetadataResult {
            schema: Some(schema.as_ref().into()),
        }))
    }

    async fn execute_query(
        &self,
        request: Request<ExecuteQueryParams>,
    ) -> std::result::Result<Response<ExecuteQueryResult>, tonic::Status> {
        if let ExecuteQueryParams {
            query: Some(query),
            settings: _,
        } = request.into_inner()
        {
            let plan = match query {
                Query::LogicalPlan(message) => {
                    let ctx = self.ctx.read().await;
                    T::try_decode(message.as_slice())
                        .and_then(|m| {
                            m.try_into_logical_plan(
                                &ctx,
                                self.codec.logical_extension_codec(),
                            )
                        })
                        .map_err(|e| {
                            let msg =
                                format!("Could not parse logical plan protobuf: {}", e);
                            error!("{}", msg);
                            tonic::Status::internal(msg)
                        })?
                }
                Query::Sql(sql) => {
                    let mut ctx = self.ctx.write().await;
                    let df = ctx.sql(&sql).await.map_err(|e| {
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
            let tx_job: Option<mpsc::Sender<String>> = match self.policy {
                TaskSchedulingPolicy::PullStaged => None,
                TaskSchedulingPolicy::PushStaged => {
                    Some(self.scheduler_env.as_ref().unwrap().tx_job.clone())
                }
            };
            let datafusion_ctx = self.ctx.read().await.clone();
            tokio::spawn(async move {
                // create physical plan using DataFusion
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
                    .await
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
                    .await
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
                            task_id: Some(PartitionId {
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

                if let Some(tx_job) = tx_job {
                    // Send job_id to the scheduler channel
                    tx_job.send(job_id_spawn).await.unwrap();
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
        let job_meta = self.state.get_job_metadata(&job_id).unwrap();
        Ok(Response::new(GetJobStatusResult {
            status: Some(job_meta),
        }))
    }
}

#[cfg(all(test, feature = "sled"))]
mod test {
    use std::sync::Arc;
    use tokio::sync::RwLock;

    use tonic::Request;

    use ballista_core::error::BallistaError;
    use ballista_core::serde::protobuf::{
        executor_registration::OptionalHost, ExecutorRegistration, LogicalPlanNode,
        PhysicalPlanNode, PollWorkParams,
    };
    use ballista_core::serde::scheduler::ExecutorSpecification;
    use ballista_core::serde::BallistaCodec;
    use datafusion::prelude::ExecutionContext;

    use super::{
        state::{SchedulerState, StandaloneClient},
        SchedulerGrpc, SchedulerServer,
    };

    #[tokio::test]
    async fn test_poll_work() -> Result<(), BallistaError> {
        let state_storage = Arc::new(StandaloneClient::try_new_temporary()?);
        let namespace = "default";
        let scheduler: SchedulerServer<LogicalPlanNode, PhysicalPlanNode> =
            SchedulerServer::new(
                state_storage.clone(),
                namespace.to_owned(),
                Arc::new(RwLock::new(ExecutionContext::new())),
                BallistaCodec::default(),
            );
        let exec_meta = ExecutorRegistration {
            id: "abc".to_owned(),
            optional_host: Some(OptionalHost::Host("".to_owned())),
            port: 0,
            grpc_port: 0,
            specification: Some(ExecutorSpecification { task_slots: 2 }.into()),
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
        let state: SchedulerState<LogicalPlanNode, PhysicalPlanNode> =
            SchedulerState::new(
                state_storage.clone(),
                namespace.to_string(),
                BallistaCodec::default(),
            );
        let ctx = scheduler.ctx.read().await;
        state.init(&ctx).await?;
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
        let state: SchedulerState<LogicalPlanNode, PhysicalPlanNode> =
            SchedulerState::new(
                state_storage.clone(),
                namespace.to_string(),
                BallistaCodec::default(),
            );
        let ctx = scheduler.ctx.read().await;
        state.init(&ctx).await?;
        // executor should be registered
        assert_eq!(state.get_executors_metadata().await.unwrap().len(), 1);
        Ok(())
    }
}
