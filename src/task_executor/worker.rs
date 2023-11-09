use super::{Computation, WorkerStats};
use crate::{
    config::Configurable,
    task_deport::{TaskStorage, TaskStorageError},
};
use serde::{de::DeserializeOwned, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkerId(usize);

#[derive(Clone, Default)]
pub struct WorkerOptions {
    pub max_retries: u32,
    pub no_task_found_delay_sec: u64,
}

pub enum WorkerCommand {
    Stop,      // stop after current task processed
    Terminate, // terminate immediately
}

pub struct Worker<Data, Comp, Ctx> {
    worker_id: WorkerId,
    ctx: Arc<Ctx>,
    storage: Arc<dyn TaskStorage<Data> + Send + Sync>,
    computation: Arc<Comp>,
    stats: WorkerStats,
    options: WorkerOptions,
    // phantom
    _payload_type: std::marker::PhantomData<Data>,
}

/// A worker implementation that fetches a task from the storage, processes it,
/// and then updates the task status. If the processing fails,
/// the task is retried up to N times.
impl<Data, Comp, Ctx> Worker<Data, Comp, Ctx>
where
    Data: std::fmt::Debug
        + Clone
        + Serialize
        + DeserializeOwned
        + Send
        + Sync
        + 'static,
    Comp: Computation<Data, Ctx> + Send + Sync + 'static,
    Ctx: Configurable + Send + Sync + 'static,
{
    pub fn new(
        worker_id: WorkerId,
        ctx: Arc<Ctx>,
        storage: Arc<dyn TaskStorage<Data> + Send + Sync>,
        computation: Arc<Comp>,
        options: WorkerOptions,
    ) -> Self {
        Self {
            worker_id,
            ctx,
            storage,
            computation,
            options,
            stats: WorkerStats::new(),
            // phantom
            _payload_type: std::marker::PhantomData,
        }
    }

    pub fn get_stats(&self) -> &WorkerStats {
        &self.stats
    }

    /// Worker run lify-cycle
    /// 1) pop task from queue (or wait a bit)
    /// 2) run computation over task
    /// 3) update task according to computation result
    pub async fn run(&mut self) {
        let start_time = std::time::Instant::now();
        match self.storage.task_pop().await {
            Ok(mut task) => {
                task.set_in_process();
                let result = self
                    .computation
                    .run(
                        self.worker_id,
                        self.ctx.clone(),
                        self.storage.clone(),
                        &mut task,
                    )
                    .await;
                match result {
                    Ok(_) => {
                        task.set_succeed();
                        self.storage.task_set(&task).await.unwrap();
                        let successful_task =
                            self.storage.task_ack(&task.task_id).await.unwrap();
                        tracing::info!(
                            "[worker-{}] Task {} succeed: {:?}",
                            self.worker_id,
                            &successful_task.task_id,
                            &successful_task.payload
                        );

                        // record stats on success
                        self.stats.record_execution_time(start_time.elapsed());
                        self.stats.record_success();
                    }
                    Err(err) => {
                        task.set_retry(&err.to_string());
                        if task.retries < self.options.max_retries {
                            self.storage.task_push(&task).await.unwrap();
                            tracing::error!(
                                "[worker-{}] Task {} failed, retrying ({}): {:?}",
                                self.worker_id,
                                &task.task_id,
                                &task.retries,
                                &err
                            );
                        } else {
                            task.set_dlq("Max retries");
                            self.storage.task_to_dlq(&task).await.unwrap();
                            tracing::error!(
                                "[worker-{}] Task {} failed, max reties ({}): {:?}",
                                self.worker_id,
                                &task.task_id,
                                &task.retries,
                                &err
                            );
                        }

                        self.stats.record_execution_time(start_time.elapsed());
                        self.stats.record_failure();
                    }
                }
            }
            Err(TaskStorageError::StorageIsEmptyError) => {
                tracing::warn!(
                    "[worker-{}] No tasks found, waiting...",
                    self.worker_id
                );
                // wait for a while till try to fetch task
                tokio::time::sleep(tokio::time::Duration::from_secs(
                    self.options.no_task_found_delay_sec,
                ))
                .await;
            }
            Err(_err) => {}
        }
    }
}

impl WorkerId {
    pub fn new(id: usize) -> Self {
        Self(id)
    }

    pub fn get(&self) -> usize {
        self.0
    }
}

impl std::fmt::Display for WorkerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A wrapper for the worker function that also checks for task
/// limits and handles shutdown signals.
pub async fn worker_wrapper<Data, Comp, Ctx>(
    worker_id: WorkerId,
    ctx: Arc<Ctx>,
    storage: Arc<dyn TaskStorage<Data> + Send + Sync>,
    computation: Arc<Comp>,
    mut commands: mpsc::Receiver<WorkerCommand>,
    worker_options: WorkerOptions,
) where
    Data: Clone
        + Serialize
        + DeserializeOwned
        + Send
        + Sync
        + 'static
        + std::fmt::Debug,
    Comp: Computation<Data, Ctx> + Send + Sync + 'static,
    Ctx: Configurable + Send + Sync + 'static,
{
    let mut worker = Worker::new(
        worker_id,
        ctx.clone(),
        storage.clone(),
        computation.clone(),
        worker_options,
    );

    'worker: loop {
        tokio::select! {
            command = commands.recv() => {
                let should_break = handle_command(command, worker_id).await;
                if should_break {
                    break 'worker;
                } else {
                    // Terminate immediately
                    return;
                }
            },
            _ = worker.run() => {
                // Nothing else needed here
            }
        };
    }
    tracing::info!("[worker-{}] completed", worker_id);
}

async fn handle_command(
    command: Option<WorkerCommand>,
    worker_id: WorkerId,
) -> bool {
    match command {
        Some(WorkerCommand::Stop) => {
            tracing::info!("[worker-{}] received stop command", worker_id);
            true
        }
        Some(WorkerCommand::Terminate) => {
            tracing::info!("[worker-{}] received terminate command", worker_id);
            false
        }
        None => {
            // All senders have been dropped, we can stop the worker
            true
        }
    }
}