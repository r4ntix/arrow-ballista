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

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;

use clap::ArgEnum;
use datafusion::datasource::listing::PartitionedFile;
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::SessionContext;
use datafusion_proto::logical_plan::AsLogicalPlan;
use datafusion_proto::physical_plan::AsExecutionPlan;
use futures::Stream;
use log::{debug, info, warn};

use ballista_core::config::BallistaConfig;
use ballista_core::consistent_hash;
use ballista_core::consistent_hash::ConsistentHash;
use ballista_core::error::{BallistaError, Result};
use ballista_core::serde::protobuf::{
    job_status, AvailableTaskSlots, ExecutorHeartbeat, JobStatus,
};
use ballista_core::serde::scheduler::{ExecutorData, ExecutorMetadata, PartitionId};
use ballista_core::serde::BallistaCodec;
use ballista_core::utils::default_session_builder;

use crate::cluster::kv::KeyValueState;
use crate::cluster::memory::{InMemoryClusterState, InMemoryJobState};
use crate::cluster::storage::etcd::EtcdClient;
use crate::cluster::storage::sled::SledClient;
use crate::cluster::storage::KeyValueStore;
use crate::config::{ClusterStorageConfig, SchedulerConfig, TaskDistributionPolicy};
use crate::scheduler_server::SessionBuilder;
use crate::state::execution_graph::{create_task_info, ExecutionGraph, TaskDescription};
use crate::state::task_manager::JobInfoCache;

pub mod event;
pub mod kv;
pub mod memory;
pub mod storage;

#[cfg(test)]
#[allow(clippy::uninlined_format_args)]
pub mod test_util;

// an enum used to configure the backend
// needs to be visible to code generated by configure_me
#[derive(Debug, Clone, ArgEnum, serde::Deserialize, PartialEq, Eq)]
pub enum ClusterStorage {
    Etcd,
    Memory,
    Sled,
}

impl std::str::FromStr for ClusterStorage {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        ArgEnum::from_str(s, true)
    }
}

impl parse_arg::ParseArgFromStr for ClusterStorage {
    fn describe_type<W: fmt::Write>(mut writer: W) -> fmt::Result {
        write!(writer, "The cluster storage backend for the scheduler")
    }
}

#[derive(Clone)]
pub struct BallistaCluster {
    cluster_state: Arc<dyn ClusterState>,
    job_state: Arc<dyn JobState>,
}

impl BallistaCluster {
    pub fn new(
        cluster_state: Arc<dyn ClusterState>,
        job_state: Arc<dyn JobState>,
    ) -> Self {
        Self {
            cluster_state,
            job_state,
        }
    }

    pub fn new_memory(
        scheduler: impl Into<String>,
        session_builder: SessionBuilder,
    ) -> Self {
        Self {
            cluster_state: Arc::new(InMemoryClusterState::default()),
            job_state: Arc::new(InMemoryJobState::new(scheduler, session_builder)),
        }
    }

    pub fn new_kv<
        S: KeyValueStore,
        T: 'static + AsLogicalPlan,
        U: 'static + AsExecutionPlan,
    >(
        store: S,
        scheduler: impl Into<String>,
        session_builder: SessionBuilder,
        codec: BallistaCodec<T, U>,
    ) -> Self {
        let kv_state =
            Arc::new(KeyValueState::new(scheduler, store, codec, session_builder));
        Self {
            cluster_state: kv_state.clone(),
            job_state: kv_state,
        }
    }

    pub async fn new_from_config(config: &SchedulerConfig) -> Result<Self> {
        let scheduler = config.scheduler_name();

        match &config.cluster_storage {
            #[cfg(feature = "etcd")]
            ClusterStorageConfig::Etcd(urls) => {
                let etcd = etcd_client::Client::connect(urls.as_slice(), None)
                    .await
                    .map_err(|err| {
                        BallistaError::Internal(format!(
                            "Could not connect to etcd: {err:?}"
                        ))
                    })?;

                Ok(Self::new_kv(
                    EtcdClient::new(config.namespace.clone(), etcd),
                    scheduler,
                    default_session_builder,
                    BallistaCodec::default(),
                ))
            }
            #[cfg(not(feature = "etcd"))]
            StateBackend::Etcd => {
                unimplemented!(
                    "build the scheduler with the `etcd` feature to use the etcd config backend"
                )
            }
            #[cfg(feature = "sled")]
            ClusterStorageConfig::Sled(dir) => {
                if let Some(dir) = dir.as_ref() {
                    info!("Initializing Sled database in directory {}", dir);
                    let sled = SledClient::try_new(dir)?;

                    Ok(Self::new_kv(
                        sled,
                        scheduler,
                        default_session_builder,
                        BallistaCodec::default(),
                    ))
                } else {
                    info!("Initializing Sled database in temp directory");
                    let sled = SledClient::try_new_temporary()?;

                    Ok(Self::new_kv(
                        sled,
                        scheduler,
                        default_session_builder,
                        BallistaCodec::default(),
                    ))
                }
            }
            #[cfg(not(feature = "sled"))]
            StateBackend::Sled => {
                unimplemented!(
                    "build the scheduler with the `sled` feature to use the sled config backend"
                )
            }
            ClusterStorageConfig::Memory => Ok(BallistaCluster::new_memory(
                scheduler,
                default_session_builder,
            )),
        }
    }

    pub fn cluster_state(&self) -> Arc<dyn ClusterState> {
        self.cluster_state.clone()
    }

    pub fn job_state(&self) -> Arc<dyn JobState> {
        self.job_state.clone()
    }
}

/// Stream of `ExecutorHeartbeat`. This stream should contain all `ExecutorHeartbeats` received
/// by any schedulers with a shared `ClusterState`
pub type ExecutorHeartbeatStream = Pin<Box<dyn Stream<Item = ExecutorHeartbeat> + Send>>;

/// A task bound with an executor to execute.
/// BoundTask.0 is the executor id; While BoundTask.1 is the task description.
pub type BoundTask = (String, TaskDescription);

/// ExecutorSlot.0 is the executor id; While ExecutorSlot.1 is for slot number.
pub type ExecutorSlot = (String, u32);

/// A trait that contains the necessary method to maintain a globally consistent view of cluster resources
#[tonic::async_trait]
pub trait ClusterState: Send + Sync + 'static {
    /// Initialize when it's necessary, especially for state with backend storage
    async fn init(&self) -> Result<()> {
        Ok(())
    }

    /// Bind the ready to running tasks from [`active_jobs`] with available executors.
    ///
    /// If `executors` is provided, only bind slots from the specified executor IDs
    async fn bind_schedulable_tasks(
        &self,
        distribution: TaskDistributionPolicy,
        active_jobs: Arc<HashMap<String, JobInfoCache>>,
        executors: Option<HashSet<String>>,
    ) -> Result<Vec<BoundTask>>;

    /// Unbind executor and task when a task finishes or fails. It will increase the executor
    /// available task slots.
    ///
    /// This operations should be atomic. Either all reservations are cancelled or none are
    async fn unbind_tasks(&self, executor_slots: Vec<ExecutorSlot>) -> Result<()>;

    /// Register a new executor in the cluster.
    async fn register_executor(
        &self,
        metadata: ExecutorMetadata,
        spec: ExecutorData,
    ) -> Result<()>;

    /// Save the executor metadata. This will overwrite existing metadata for the executor ID
    async fn save_executor_metadata(&self, metadata: ExecutorMetadata) -> Result<()>;

    /// Get executor metadata for the provided executor ID. Returns an error if the executor does not exist
    async fn get_executor_metadata(&self, executor_id: &str) -> Result<ExecutorMetadata>;

    /// Save the executor heartbeat
    async fn save_executor_heartbeat(&self, heartbeat: ExecutorHeartbeat) -> Result<()>;

    /// Remove the executor from the cluster
    async fn remove_executor(&self, executor_id: &str) -> Result<()>;

    /// Return a map of the last seen heartbeat for all active executors
    fn executor_heartbeats(&self) -> HashMap<String, ExecutorHeartbeat>;

    /// Get executor heartbeat for the provided executor ID. Return None if the executor does not exist
    fn get_executor_heartbeat(&self, executor_id: &str) -> Option<ExecutorHeartbeat>;
}

/// Events related to the state of jobs. Implementations may or may not support all event types.
#[derive(Debug, Clone, PartialEq)]
pub enum JobStateEvent {
    /// Event when a job status has been updated
    JobUpdated {
        /// Job ID of updated job
        job_id: String,
        /// New job status
        status: JobStatus,
    },
    /// Event when a scheduler acquires ownership of the job. This happens
    /// either when a scheduler submits a job (in which case ownership is implied)
    /// or when a scheduler acquires ownership of a running job release by a
    /// different scheduler
    JobAcquired {
        /// Job ID of the acquired job
        job_id: String,
        /// The scheduler which acquired ownership of the job
        owner: String,
    },
    /// Event when a scheduler releases ownership of a still active job
    JobReleased {
        /// Job ID of the released job
        job_id: String,
    },
    /// Event when a new session has been created
    SessionCreated {
        session_id: String,
        config: BallistaConfig,
    },
    /// Event when a session configuration has been updated
    SessionUpdated {
        session_id: String,
        config: BallistaConfig,
    },
}

/// Stream of `JobStateEvent`. This stream should contain all `JobStateEvent`s received
/// by any schedulers with a shared `ClusterState`
pub type JobStateEventStream = Pin<Box<dyn Stream<Item = JobStateEvent> + Send>>;

/// A trait that contains the necessary methods for persisting state related to executing jobs
#[tonic::async_trait]
pub trait JobState: Send + Sync {
    /// Accept job into  a scheduler's job queue. This should be called when a job is
    /// received by the scheduler but before it is planned and may or may not be saved
    /// in global state
    fn accept_job(&self, job_id: &str, job_name: &str, queued_at: u64) -> Result<()>;

    /// Get the number of queued jobs. If it's big, then it means the scheduler is too busy.
    /// In normal case, it's better to be 0.
    fn pending_job_number(&self) -> usize;

    /// Submit a new job to the `JobState`. It is assumed that the submitter owns the job.
    /// In local state the job should be save as `JobStatus::Active` and in shared state
    /// it should be saved as `JobStatus::Running` with `scheduler` set to the current scheduler
    async fn submit_job(&self, job_id: String, graph: &ExecutionGraph) -> Result<()>;

    /// Return a `Vec` of all active job IDs in the `JobState`
    async fn get_jobs(&self) -> Result<HashSet<String>>;

    /// Fetch the job status
    async fn get_job_status(&self, job_id: &str) -> Result<Option<JobStatus>>;

    /// Get the `ExecutionGraph` for job. The job may or may not belong to the caller
    /// and should return the `ExecutionGraph` for the given job (if it exists) at the
    /// time this method is called with no guarantees that the graph has not been
    /// subsequently updated by another scheduler.
    async fn get_execution_graph(&self, job_id: &str) -> Result<Option<ExecutionGraph>>;

    /// Persist the current state of an owned job to global state. This should fail
    /// if the job is not owned by the caller.
    async fn save_job(&self, job_id: &str, graph: &ExecutionGraph) -> Result<()>;

    /// Mark a job which has not been submitted as failed. This should be called if a job fails
    /// during planning (and does not yet have an `ExecutionGraph`)
    async fn fail_unscheduled_job(&self, job_id: &str, reason: String) -> Result<()>;

    /// Delete a job from the global state
    async fn remove_job(&self, job_id: &str) -> Result<()>;

    /// Attempt to acquire ownership of the given job. If the job is still in a running state
    /// and is successfully acquired by the caller, return the current `ExecutionGraph`,
    /// otherwise return `None`
    async fn try_acquire_job(&self, job_id: &str) -> Result<Option<ExecutionGraph>>;

    /// Get a stream of all `JobState` events. An event should be published any time that status
    /// of a job changes in state
    async fn job_state_events(&self) -> Result<JobStateEventStream>;

    /// Get the `SessionContext` associated with `session_id`. Returns an error if the
    /// session does not exist
    async fn get_session(&self, session_id: &str) -> Result<Arc<SessionContext>>;

    /// Create a new saved session
    async fn create_session(
        &self,
        config: &BallistaConfig,
    ) -> Result<Arc<SessionContext>>;

    // Update a new saved session. If the session does not exist, a new one will be created
    async fn update_session(
        &self,
        session_id: &str,
        config: &BallistaConfig,
    ) -> Result<Arc<SessionContext>>;

    async fn remove_session(
        &self,
        session_id: &str,
    ) -> Result<Option<Arc<SessionContext>>>;
}

pub(crate) async fn bind_task_bias(
    mut slots: Vec<&mut AvailableTaskSlots>,
    active_jobs: Arc<HashMap<String, JobInfoCache>>,
    if_skip: fn(Arc<dyn ExecutionPlan>) -> bool,
) -> Vec<BoundTask> {
    let mut schedulable_tasks: Vec<BoundTask> = vec![];

    let total_slots = slots.iter().fold(0, |acc, s| acc + s.slots);
    if total_slots == 0 {
        warn!("Not enough available executor slots for task running!!!");
        return schedulable_tasks;
    }

    // Sort the slots by descending order
    slots.sort_by(|a, b| Ord::cmp(&b.slots, &a.slots));

    let mut idx_slot = 0usize;
    let mut slot = &mut slots[idx_slot];
    for (job_id, job_info) in active_jobs.iter() {
        if !matches!(job_info.status, Some(job_status::Status::Running(_))) {
            debug!(
                "Job {} is not in running status and will be skipped",
                job_id
            );
            continue;
        }
        let mut graph = job_info.execution_graph.write().await;
        let session_id = graph.session_id().to_string();
        let mut black_list = vec![];
        while let Some((running_stage, task_id_gen)) =
            graph.fetch_running_stage(&black_list)
        {
            if if_skip(running_stage.plan.clone()) {
                info!(
                    "Will skip stage {}/{} for bias task binding",
                    job_id, running_stage.stage_id
                );
                black_list.push(running_stage.stage_id);
                continue;
            }
            // We are sure that it will at least bind one task by going through the following logic.
            // It will not go into a dead loop.
            let runnable_tasks = running_stage
                .task_infos
                .iter_mut()
                .enumerate()
                .filter(|(_partition, info)| info.is_none())
                .take(total_slots as usize)
                .collect::<Vec<_>>();
            for (partition_id, task_info) in runnable_tasks {
                // Assign [`slot`] with a slot available slot number larger than 0
                while slot.slots == 0 {
                    idx_slot += 1;
                    if idx_slot >= slots.len() {
                        return schedulable_tasks;
                    }
                    slot = &mut slots[idx_slot];
                }
                let executor_id = slot.executor_id.clone();
                let task_id = *task_id_gen;
                *task_id_gen += 1;
                *task_info = Some(create_task_info(executor_id.clone(), task_id));

                let partition = PartitionId {
                    job_id: job_id.clone(),
                    stage_id: running_stage.stage_id,
                    partition_id,
                };
                let task_desc = TaskDescription {
                    session_id: session_id.clone(),
                    partition,
                    stage_attempt_num: running_stage.stage_attempt_num,
                    task_id,
                    task_attempt: running_stage.task_failure_numbers[partition_id],
                    data_cache: false,
                    plan: running_stage.plan.clone(),
                };
                schedulable_tasks.push((executor_id, task_desc));

                slot.slots -= 1;
            }
        }
    }

    schedulable_tasks
}

pub(crate) async fn bind_task_round_robin(
    mut slots: Vec<&mut AvailableTaskSlots>,
    active_jobs: Arc<HashMap<String, JobInfoCache>>,
    if_skip: fn(Arc<dyn ExecutionPlan>) -> bool,
) -> Vec<BoundTask> {
    let mut schedulable_tasks: Vec<BoundTask> = vec![];

    let mut total_slots = slots.iter().fold(0, |acc, s| acc + s.slots);
    if total_slots == 0 {
        warn!("Not enough available executor slots for task running!!!");
        return schedulable_tasks;
    }
    info!("Total slot number is {}", total_slots);

    // Sort the slots by descending order
    slots.sort_by(|a, b| Ord::cmp(&b.slots, &a.slots));

    let mut idx_slot = 0usize;
    for (job_id, job_info) in active_jobs.iter() {
        if !matches!(job_info.status, Some(job_status::Status::Running(_))) {
            debug!(
                "Job {} is not in running status and will be skipped",
                job_id
            );
            continue;
        }
        let mut graph = job_info.execution_graph.write().await;
        let session_id = graph.session_id().to_string();
        let mut black_list = vec![];
        while let Some((running_stage, task_id_gen)) =
            graph.fetch_running_stage(&black_list)
        {
            if if_skip(running_stage.plan.clone()) {
                info!(
                    "Will skip stage {}/{} for round robin task binding",
                    job_id, running_stage.stage_id
                );
                black_list.push(running_stage.stage_id);
                continue;
            }
            // We are sure that it will at least bind one task by going through the following logic.
            // It will not go into a dead loop.
            let runnable_tasks = running_stage
                .task_infos
                .iter_mut()
                .enumerate()
                .filter(|(_partition, info)| info.is_none())
                .take(total_slots as usize)
                .collect::<Vec<_>>();
            for (partition_id, task_info) in runnable_tasks {
                // Move to the index which has available slots
                if idx_slot >= slots.len() {
                    idx_slot = 0;
                }
                if slots[idx_slot].slots == 0 {
                    idx_slot = 0;
                }
                // Since the slots is a vector with descending order, and the total available slots is larger than 0,
                // we are sure the available slot number at idx_slot is larger than 1
                let slot = &mut slots[idx_slot];
                let executor_id = slot.executor_id.clone();
                let task_id = *task_id_gen;
                *task_id_gen += 1;
                *task_info = Some(create_task_info(executor_id.clone(), task_id));

                let partition = PartitionId {
                    job_id: job_id.clone(),
                    stage_id: running_stage.stage_id,
                    partition_id,
                };
                let task_desc = TaskDescription {
                    session_id: session_id.clone(),
                    partition,
                    stage_attempt_num: running_stage.stage_attempt_num,
                    task_id,
                    task_attempt: running_stage.task_failure_numbers[partition_id],
                    data_cache: false,
                    plan: running_stage.plan.clone(),
                };
                schedulable_tasks.push((executor_id, task_desc));

                idx_slot += 1;
                slot.slots -= 1;
                total_slots -= 1;
                if total_slots == 0 {
                    return schedulable_tasks;
                }
            }
        }
    }

    schedulable_tasks
}

type GetScanFilesFunc = fn(
    &str,
    Arc<dyn ExecutionPlan>,
) -> datafusion::common::Result<Vec<Vec<Vec<PartitionedFile>>>>;

pub(crate) async fn bind_task_consistent_hash(
    topology_nodes: HashMap<String, TopologyNode>,
    num_replicas: usize,
    tolerance: usize,
    active_jobs: Arc<HashMap<String, JobInfoCache>>,
    get_scan_files: GetScanFilesFunc,
) -> Result<(Vec<BoundTask>, Option<ConsistentHash<TopologyNode>>)> {
    let mut total_slots = 0usize;
    for (_, node) in topology_nodes.iter() {
        total_slots += node.available_slots as usize;
    }
    if total_slots == 0 {
        info!("Not enough available executor slots for binding tasks with consistent hashing policy!!!");
        return Ok((vec![], None));
    }
    info!("Total slot number is {}", total_slots);

    let node_replicas = topology_nodes
        .into_values()
        .map(|node| (node, num_replicas))
        .collect::<Vec<_>>();
    let mut ch_topology: ConsistentHash<TopologyNode> =
        ConsistentHash::new(node_replicas);

    let mut schedulable_tasks: Vec<BoundTask> = vec![];
    for (job_id, job_info) in active_jobs.iter() {
        if !matches!(job_info.status, Some(job_status::Status::Running(_))) {
            debug!(
                "Job {} is not in running status and will be skipped",
                job_id
            );
            continue;
        }
        let mut graph = job_info.execution_graph.write().await;
        let session_id = graph.session_id().to_string();
        let mut black_list = vec![];
        while let Some((running_stage, task_id_gen)) =
            graph.fetch_running_stage(&black_list)
        {
            let scan_files = get_scan_files(job_id, running_stage.plan.clone())?;
            if is_skip_consistent_hash(&scan_files) {
                info!(
                    "Will skip stage {}/{} for consistent hashing task binding",
                    job_id, running_stage.stage_id
                );
                black_list.push(running_stage.stage_id);
                continue;
            }
            let pre_total_slots = total_slots;
            let scan_files = &scan_files[0];
            let tolerance_list = vec![0, tolerance];
            // First round with 0 tolerance consistent hashing policy
            // Second round with [`tolerance`] tolerance consistent hashing policy
            for tolerance in tolerance_list {
                let runnable_tasks = running_stage
                    .task_infos
                    .iter_mut()
                    .enumerate()
                    .filter(|(_partition, info)| info.is_none())
                    .take(total_slots)
                    .collect::<Vec<_>>();
                for (partition_id, task_info) in runnable_tasks {
                    let partition_files = &scan_files[partition_id];
                    assert!(!partition_files.is_empty());
                    // Currently we choose the first file for a task for consistent hash.
                    // Later when splitting files for tasks in datafusion, it's better to
                    // introduce this hash based policy besides the file number policy or file size policy.
                    let file_for_hash = &partition_files[0];
                    if let Some(node) = ch_topology.get_mut_with_tolerance(
                        file_for_hash.object_meta.location.as_ref().as_bytes(),
                        tolerance,
                    ) {
                        let executor_id = node.id.clone();
                        let task_id = *task_id_gen;
                        *task_id_gen += 1;
                        *task_info = Some(create_task_info(executor_id.clone(), task_id));

                        let partition = PartitionId {
                            job_id: job_id.clone(),
                            stage_id: running_stage.stage_id,
                            partition_id,
                        };
                        let data_cache = tolerance == 0;
                        let task_desc = TaskDescription {
                            session_id: session_id.clone(),
                            partition,
                            stage_attempt_num: running_stage.stage_attempt_num,
                            task_id,
                            task_attempt: running_stage.task_failure_numbers
                                [partition_id],
                            data_cache,
                            plan: running_stage.plan.clone(),
                        };
                        schedulable_tasks.push((executor_id, task_desc));

                        node.available_slots -= 1;
                        total_slots -= 1;
                        if total_slots == 0 {
                            return Ok((schedulable_tasks, Some(ch_topology)));
                        }
                    }
                }
            }
            // Since there's no more tasks from this stage which can be bound,
            // we should skip this stage at the next round.
            if pre_total_slots == total_slots {
                black_list.push(running_stage.stage_id);
            }
        }
    }

    Ok((schedulable_tasks, Some(ch_topology)))
}

// If if there's no plan which needs to scan files, skip it.
// Or there are multiple plans which need to scan files for a stage, skip it.
pub(crate) fn is_skip_consistent_hash(scan_files: &[Vec<Vec<PartitionedFile>>]) -> bool {
    scan_files.is_empty() || scan_files.len() > 1
}

#[derive(Clone)]
pub struct TopologyNode {
    pub id: String,
    pub name: String,
    pub last_seen_ts: u64,
    pub available_slots: u32,
}

impl TopologyNode {
    fn new(
        host: &str,
        port: u16,
        id: &str,
        last_seen_ts: u64,
        available_slots: u32,
    ) -> Self {
        Self {
            id: id.to_string(),
            name: format!("{host}:{port}"),
            last_seen_ts,
            available_slots,
        }
    }
}

impl consistent_hash::node::Node for TopologyNode {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_valid(&self) -> bool {
        self.available_slots > 0
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;
    use std::sync::Arc;

    use datafusion::datasource::listing::PartitionedFile;
    use object_store::path::Path;
    use object_store::ObjectMeta;

    use ballista_core::error::Result;
    use ballista_core::serde::protobuf::AvailableTaskSlots;
    use ballista_core::serde::scheduler::{ExecutorMetadata, ExecutorSpecification};

    use crate::cluster::{
        bind_task_bias, bind_task_consistent_hash, bind_task_round_robin, BoundTask,
        TopologyNode,
    };
    use crate::state::execution_graph::ExecutionGraph;
    use crate::state::task_manager::JobInfoCache;
    use crate::test_utils::{mock_completed_task, test_aggregation_plan_with_job_id};

    #[tokio::test]
    async fn test_bind_task_bias() -> Result<()> {
        let num_partition = 8usize;
        let active_jobs = mock_active_jobs(num_partition).await?;
        let mut available_slots = mock_available_slots();
        let available_slots_ref: Vec<&mut AvailableTaskSlots> =
            available_slots.iter_mut().collect();

        let bound_tasks =
            bind_task_bias(available_slots_ref, Arc::new(active_jobs), |_| false).await;
        assert_eq!(9, bound_tasks.len());

        let result = get_result(bound_tasks);

        let mut expected = Vec::new();
        {
            let mut expected0 = HashMap::new();

            let mut entry_a = HashMap::new();
            entry_a.insert("executor_3".to_string(), 2);
            let mut entry_b = HashMap::new();
            entry_b.insert("executor_3".to_string(), 5);
            entry_b.insert("executor_2".to_string(), 2);

            expected0.insert("job_a".to_string(), entry_a);
            expected0.insert("job_b".to_string(), entry_b);

            expected.push(expected0);
        }
        {
            let mut expected0 = HashMap::new();

            let mut entry_b = HashMap::new();
            entry_b.insert("executor_3".to_string(), 7);
            let mut entry_a = HashMap::new();
            entry_a.insert("executor_2".to_string(), 2);

            expected0.insert("job_a".to_string(), entry_a);
            expected0.insert("job_b".to_string(), entry_b);

            expected.push(expected0);
        }

        assert!(
            expected.contains(&result),
            "The result {:?} is not as expected {:?}",
            result,
            expected
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_bind_task_round_robin() -> Result<()> {
        let num_partition = 8usize;
        let active_jobs = mock_active_jobs(num_partition).await?;
        let mut available_slots = mock_available_slots();
        let available_slots_ref: Vec<&mut AvailableTaskSlots> =
            available_slots.iter_mut().collect();

        let bound_tasks =
            bind_task_round_robin(available_slots_ref, Arc::new(active_jobs), |_| false)
                .await;
        assert_eq!(9, bound_tasks.len());

        let result = get_result(bound_tasks);

        let mut expected = Vec::new();
        {
            let mut expected0 = HashMap::new();

            let mut entry_a = HashMap::new();
            entry_a.insert("executor_3".to_string(), 1);
            entry_a.insert("executor_2".to_string(), 1);
            let mut entry_b = HashMap::new();
            entry_b.insert("executor_1".to_string(), 3);
            entry_b.insert("executor_3".to_string(), 2);
            entry_b.insert("executor_2".to_string(), 2);

            expected0.insert("job_a".to_string(), entry_a);
            expected0.insert("job_b".to_string(), entry_b);

            expected.push(expected0);
        }
        {
            let mut expected0 = HashMap::new();

            let mut entry_b = HashMap::new();
            entry_b.insert("executor_3".to_string(), 3);
            entry_b.insert("executor_2".to_string(), 2);
            entry_b.insert("executor_1".to_string(), 2);
            let mut entry_a = HashMap::new();
            entry_a.insert("executor_2".to_string(), 1);
            entry_a.insert("executor_1".to_string(), 1);

            expected0.insert("job_a".to_string(), entry_a);
            expected0.insert("job_b".to_string(), entry_b);

            expected.push(expected0);
        }

        assert!(
            expected.contains(&result),
            "The result {:?} is not as expected {:?}",
            result,
            expected
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_bind_task_consistent_hash() -> Result<()> {
        let num_partition = 8usize;
        let active_jobs = mock_active_jobs(num_partition).await?;
        let active_jobs = Arc::new(active_jobs);
        let topology_nodes = mock_topology_nodes();
        let num_replicas = 31;
        let tolerance = 0;

        // Check none scan files case
        {
            let (bound_tasks, _) = bind_task_consistent_hash(
                topology_nodes.clone(),
                num_replicas,
                tolerance,
                active_jobs.clone(),
                |_, _| Ok(vec![]),
            )
            .await?;
            assert_eq!(0, bound_tasks.len());
        }

        // Check job_b with scan files
        {
            let (bound_tasks, _) = bind_task_consistent_hash(
                topology_nodes,
                num_replicas,
                tolerance,
                active_jobs,
                |job_id, _| mock_get_scan_files("job_b", job_id, 8),
            )
            .await?;
            assert_eq!(6, bound_tasks.len());

            let result = get_result(bound_tasks);

            let mut expected = HashMap::new();
            {
                let mut entry_b = HashMap::new();
                entry_b.insert("executor_3".to_string(), 2);
                entry_b.insert("executor_2".to_string(), 3);
                entry_b.insert("executor_1".to_string(), 1);

                expected.insert("job_b".to_string(), entry_b);
            }
            assert!(
                expected.eq(&result),
                "The result {:?} is not as expected {:?}",
                result,
                expected
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_bind_task_consistent_hash_with_tolerance() -> Result<()> {
        let num_partition = 8usize;
        let active_jobs = mock_active_jobs(num_partition).await?;
        let active_jobs = Arc::new(active_jobs);
        let topology_nodes = mock_topology_nodes();
        let num_replicas = 31;
        let tolerance = 1;

        {
            let (bound_tasks, _) = bind_task_consistent_hash(
                topology_nodes,
                num_replicas,
                tolerance,
                active_jobs,
                |job_id, _| mock_get_scan_files("job_b", job_id, 8),
            )
            .await?;
            assert_eq!(7, bound_tasks.len());

            let result = get_result(bound_tasks);

            let mut expected = HashMap::new();
            {
                let mut entry_b = HashMap::new();
                entry_b.insert("executor_3".to_string(), 3);
                entry_b.insert("executor_2".to_string(), 3);
                entry_b.insert("executor_1".to_string(), 1);

                expected.insert("job_b".to_string(), entry_b);
            }
            assert!(
                expected.eq(&result),
                "The result {:?} is not as expected {:?}",
                result,
                expected
            );
        }

        Ok(())
    }

    fn get_result(
        bound_tasks: Vec<BoundTask>,
    ) -> HashMap<String, HashMap<String, usize>> {
        let mut result = HashMap::new();

        for bound_task in bound_tasks {
            let entry = result
                .entry(bound_task.1.partition.job_id)
                .or_insert_with(HashMap::new);
            let n = entry.entry(bound_task.0).or_insert_with(|| 0);
            *n += 1;
        }

        result
    }

    async fn mock_active_jobs(
        num_partition: usize,
    ) -> Result<HashMap<String, JobInfoCache>> {
        let graph_a = mock_graph("job_a", num_partition, 2).await?;

        let graph_b = mock_graph("job_b", num_partition, 7).await?;

        let mut active_jobs = HashMap::new();
        active_jobs.insert(graph_a.job_id().to_string(), JobInfoCache::new(graph_a));
        active_jobs.insert(graph_b.job_id().to_string(), JobInfoCache::new(graph_b));

        Ok(active_jobs)
    }

    async fn mock_graph(
        job_id: &str,
        num_partition: usize,
        num_pending_task: usize,
    ) -> Result<ExecutionGraph> {
        let mut graph = test_aggregation_plan_with_job_id(num_partition, job_id).await;
        let executor = ExecutorMetadata {
            id: "executor_0".to_string(),
            host: "localhost".to_string(),
            port: 50051,
            grpc_port: 50052,
            specification: ExecutorSpecification { task_slots: 32 },
        };

        if let Some(task) = graph.pop_next_task(&executor.id)? {
            let task_status = mock_completed_task(task, &executor.id);
            graph.update_task_status(&executor, vec![task_status], 1, 1)?;
        }

        graph.revive();

        for _i in 0..num_partition - num_pending_task {
            if let Some(task) = graph.pop_next_task(&executor.id)? {
                let task_status = mock_completed_task(task, &executor.id);
                graph.update_task_status(&executor, vec![task_status], 1, 1)?;
            }
        }

        Ok(graph)
    }

    fn mock_available_slots() -> Vec<AvailableTaskSlots> {
        vec![
            AvailableTaskSlots {
                executor_id: "executor_1".to_string(),
                slots: 3,
            },
            AvailableTaskSlots {
                executor_id: "executor_2".to_string(),
                slots: 5,
            },
            AvailableTaskSlots {
                executor_id: "executor_3".to_string(),
                slots: 7,
            },
        ]
    }

    fn mock_topology_nodes() -> HashMap<String, TopologyNode> {
        let mut topology_nodes = HashMap::new();
        topology_nodes.insert(
            "executor_1".to_string(),
            TopologyNode::new("localhost", 8081, "executor_1", 0, 1),
        );
        topology_nodes.insert(
            "executor_2".to_string(),
            TopologyNode::new("localhost", 8082, "executor_2", 0, 3),
        );
        topology_nodes.insert(
            "executor_3".to_string(),
            TopologyNode::new("localhost", 8083, "executor_3", 0, 5),
        );
        topology_nodes
    }

    fn mock_get_scan_files(
        expected_job_id: &str,
        job_id: &str,
        num_partition: usize,
    ) -> datafusion::common::Result<Vec<Vec<Vec<PartitionedFile>>>> {
        Ok(if expected_job_id.eq(job_id) {
            mock_scan_files(num_partition)
        } else {
            vec![]
        })
    }

    fn mock_scan_files(num_partition: usize) -> Vec<Vec<Vec<PartitionedFile>>> {
        let mut scan_files = vec![];
        for i in 0..num_partition {
            scan_files.push(vec![PartitionedFile {
                object_meta: ObjectMeta {
                    location: Path::from(format!("file--{}", i)),
                    last_modified: Default::default(),
                    size: 1,
                    e_tag: None,
                },
                partition_values: vec![],
                range: None,
                extensions: None,
            }]);
        }
        vec![scan_files]
    }
}
