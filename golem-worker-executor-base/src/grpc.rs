// Copyright 2024 Golem Cloud
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use gethostname::gethostname;
use std::collections::HashMap;
use std::fmt::{Debug, Display, Formatter};
use std::marker::PhantomData;

use std::sync::Arc;

use golem_api_grpc::proto::golem;
use golem_api_grpc::proto::golem::common::ResourceLimits as GrpcResourceLimits;
use golem_api_grpc::proto::golem::workerexecutor::worker_executor_server::WorkerExecutor;
use golem_common::cache::PendingOrFinal;
use golem_common::model as common_model;
use golem_common::model::{
    AccountId, CallingConvention, InvocationKey, ShardId, WorkerMetadata, WorkerStatus,
};
use golem_wasm_rpc::protobuf::Val;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use wasmtime::Error;

use crate::error::*;
use crate::metrics::grpc::{
    record_closed_grpc_active_stream, record_new_grpc_active_stream, RecordedGrpcRequest,
};
use crate::model::InterruptKind;
use crate::services::worker_activator::{DefaultWorkerActivator, LazyWorkerActivator};
use crate::services::worker_event::LogLevel;
use crate::services::{
    worker_event, All, HasActiveWorkers, HasAll, HasInvocationKeyService, HasPromiseService,
    HasShardManagerService, HasShardService, HasWorkerService, UsesAllDeps,
};
use crate::worker::{invoke, invoke_and_await, Worker};
use crate::workerctx::{PublicWorkerIo, WorkerCtx};

pub enum GrpcError<E> {
    Transport(tonic::transport::Error),
    Status(Status),
    Domain(E),
    Unexpected(String),
}

impl<E: Debug> Debug for GrpcError<E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            GrpcError::Transport(err) => write!(f, "Transport({err:?})"),
            GrpcError::Status(err) => write!(f, "Status({err:?})"),
            GrpcError::Domain(err) => write!(f, "Domain({err:?})"),
            GrpcError::Unexpected(err) => write!(f, "Unexpected({err:?})"),
        }
    }
}

impl<E: Debug> Display for GrpcError<E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            GrpcError::Transport(err) => write!(f, "gRPC transport error: {err})"),
            GrpcError::Status(err) => write!(f, "Failed gRPC request: {err})"),
            GrpcError::Domain(err) => write!(f, "gRPC request failed with {err:?}"),
            GrpcError::Unexpected(err) => write!(f, "Unexpected error {err}"),
        }
    }
}

impl<E: Debug> std::error::Error for GrpcError<E> {}

impl<E> From<tonic::transport::Error> for GrpcError<E> {
    fn from(value: tonic::transport::Error) -> Self {
        Self::Transport(value)
    }
}

impl<E> From<Status> for GrpcError<E> {
    fn from(value: Status) -> Self {
        Self::Status(value)
    }
}

impl<E> From<String> for GrpcError<E> {
    fn from(value: String) -> Self {
        Self::Unexpected(value)
    }
}

pub fn is_grpc_retriable<E>(error: &GrpcError<E>) -> bool {
    match error {
        GrpcError::Transport(_) => true,
        GrpcError::Status(status) => status.code() == tonic::Code::Unavailable,
        GrpcError::Domain(_) => false,
        GrpcError::Unexpected(_) => false,
    }
}

/// This is the implementation of the Worker Executor gRPC API
pub struct WorkerExecutorImpl<
    Ctx: WorkerCtx,
    Svcs: HasAll<Ctx> + UsesAllDeps<Ctx = Ctx> + Send + Sync + 'static,
> {
    /// Reference to all the initialized services
    services: Svcs,
    ctx: PhantomData<Ctx>,
}

impl<Ctx: WorkerCtx, Svcs: HasAll<Ctx> + UsesAllDeps<Ctx = Ctx> + Send + Sync + 'static> Clone
    for WorkerExecutorImpl<Ctx, Svcs>
{
    fn clone(&self) -> Self {
        Self {
            services: self.services.clone(),
            ctx: PhantomData,
        }
    }
}

type ResponseResult<T> = Result<Response<T>, Status>;
type ResponseStream = ReceiverStream<Result<golem::worker::LogEvent, Status>>;

impl<Ctx: WorkerCtx, Svcs: HasAll<Ctx> + UsesAllDeps<Ctx = Ctx> + Send + Sync + 'static>
    WorkerExecutorImpl<Ctx, Svcs>
{
    pub async fn new(
        services: Svcs,
        lazy_worker_activator: Arc<LazyWorkerActivator>,
        port: u16,
    ) -> Result<Self, Error> {
        let worker_executor = WorkerExecutorImpl {
            services: services.clone(),
            ctx: PhantomData,
        };
        let worker_activator = Arc::new(DefaultWorkerActivator::new(services));
        lazy_worker_activator.set(worker_activator);

        let host = gethostname().to_string_lossy().to_string();

        info!("Registering worker executor as {}:{}", host, port);

        let shard_assignment = worker_executor
            .shard_manager_service()
            .register(host, port)
            .await?;

        worker_executor.shard_service().register(
            shard_assignment.number_of_shards,
            &shard_assignment.shard_ids,
        );

        info!("Registered worker executor, waiting for shard assignment...");

        Ctx::on_shard_assignment_changed(&worker_executor).await?;

        Ok(worker_executor)
    }

    async fn validate_worker_status(
        &self,
        worker_id: &common_model::WorkerId,
        metadata: &Option<WorkerMetadata>,
    ) -> Result<WorkerStatus, GolemError> {
        debug!("validate_worker_status for {worker_id}");
        let worker_status = Ctx::get_assumed_worker_status(self, worker_id, metadata).await;

        match worker_status {
            WorkerStatus::Failed => Err(GolemError::PreviousInvocationFailed),
            WorkerStatus::Exited => Err(GolemError::PreviousInvocationExited),
            _ => {
                let error_count = Ctx::get_worker_retry_count(self, worker_id).await;
                debug!(
                    "Trailing error count for worker {}: {}",
                    worker_id, error_count
                );
                if error_count > 0 {
                    Err(GolemError::PreviousInvocationFailed)
                } else {
                    Ok(worker_status)
                }
            }
        }
    }

    fn validate_worker_id(&self, worker_id: &common_model::WorkerId) -> Result<(), GolemError> {
        self.shard_service().check_worker(worker_id)
    }

    async fn create_worker_internal(
        &self,
        request: golem::workerexecutor::CreateWorkerRequest,
    ) -> Result<(), GolemError> {
        let proto_worker_id = request
            .worker_id
            .ok_or(GolemError::invalid_request("worker_id not found"))?;

        let account_id = request
            .account_id
            .ok_or(GolemError::invalid_request("account_id not found"))?
            .into();

        if let Some(limits) = request.account_limits {
            Ctx::record_last_known_limits(self, &account_id, &limits.into()).await?;
        }

        let template_version = request.template_version;

        let worker_id = common_model::WorkerId::from_proto(proto_worker_id);

        self.validate_worker_id(&worker_id)?;

        let args = request.args;
        let env = request
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        Worker::get_or_create_with_config(
            self,
            &worker_id,
            args,
            env,
            Some(template_version),
            account_id,
        )
        .await?;

        Ok(())
    }

    async fn complete_promise_internal(
        &self,
        request: golem::workerexecutor::CompletePromiseRequest,
    ) -> Result<golem::workerexecutor::CompletePromiseSuccess, GolemError> {
        debug!("complete_promise: {:?}", request);

        let promise_id = request
            .promise_id
            .ok_or(GolemError::invalid_request("promise_id not found"))?;
        let data = request.data;

        let worker_id = common_model::WorkerId::from_proto(
            promise_id
                .worker_id
                .clone()
                .ok_or(GolemError::invalid_request("worker_id not found"))?,
        );

        self.validate_worker_id(&worker_id)?;

        let promise_id: common_model::PromiseId =
            promise_id.try_into().map_err(GolemError::invalid_request)?;
        let completed = self.promise_service().complete(promise_id, data).await?;

        let metadata = self
            .worker_service()
            .get(&worker_id)
            .await
            .ok_or(GolemError::worker_not_found(worker_id.clone()))?;

        let worker_status =
            Ctx::get_assumed_worker_status(self, &worker_id, &Some(metadata.clone())).await;
        let should_activate = match worker_status {
            WorkerStatus::Interrupted
            | WorkerStatus::Running
            | WorkerStatus::Suspended
            | WorkerStatus::Retrying => true,
            WorkerStatus::Exited | WorkerStatus::Failed | WorkerStatus::Idle => false,
        };

        if should_activate {
            // By making sure the worker is in memory, if it is waiting for the promise it is going to
            // be notified when we complete it below.
            Worker::activate(
                &self.services,
                &metadata.worker_id.worker_id,
                metadata.args,
                metadata.env,
                Some(metadata.worker_id.template_version),
                metadata.account_id,
            )
            .await;
        }

        let success = golem::workerexecutor::CompletePromiseSuccess { completed };

        Ok(success)
    }

    async fn delete_worker_internal(
        &self,
        inner: golem::worker::WorkerId,
    ) -> Result<(), GolemError> {
        let worker_id = common_model::WorkerId::from_proto(inner);

        self.validate_worker_id(&worker_id)?;

        let metadata = self.worker_service().get(&worker_id).await;

        let worker_status = Ctx::get_assumed_worker_status(self, &worker_id, &metadata).await;
        let metadata = metadata.ok_or(GolemError::invalid_request("Worker not found"))?;

        let should_interrupt = match worker_status {
            WorkerStatus::Running | WorkerStatus::Suspended | WorkerStatus::Retrying => true,
            WorkerStatus::Exited
            | WorkerStatus::Failed
            | WorkerStatus::Idle
            | WorkerStatus::Interrupted => false,
        };

        if should_interrupt {
            let worker_details = Worker::get_or_create_with_config(
                self,
                &metadata.worker_id.worker_id,
                metadata.args,
                metadata.env,
                Some(metadata.worker_id.template_version),
                metadata.account_id,
            )
            .await?;

            if let Some(mut await_interrupted) =
                worker_details.set_interrupting(InterruptKind::Interrupt)
            {
                await_interrupted.recv().await.unwrap();
            }
        }

        Ctx::on_worker_deleted(self, &worker_id).await?;
        self.worker_service().remove(&worker_id).await;
        self.active_workers().remove(&worker_id);

        Ok(())
    }

    async fn get_invocation_key_internal(
        &self,
        request: golem::workerexecutor::GetInvocationKeyRequest,
    ) -> Result<golem::workerexecutor::GetInvocationKeySuccess, GolemError> {
        let worker_id = common_model::WorkerId::from_proto(
            request
                .worker_id
                .ok_or(GolemError::invalid_request("worker_id not found"))?,
        );

        self.validate_worker_id(&worker_id)?;

        let invocation_key = self.invocation_key_service().generate_key(&worker_id);

        Ok(golem::workerexecutor::GetInvocationKeySuccess {
            invocation_key: Some(invocation_key.into()),
        })
    }

    async fn interrupt_worker_internal(
        &self,
        request: golem::workerexecutor::InterruptWorkerRequest,
    ) -> Result<(), GolemError> {
        let worker_id = request
            .worker_id
            .ok_or(GolemError::invalid_request("worker_id not found"))?;

        let worker_id = common_model::WorkerId::from_proto(worker_id);

        let metadata = self.worker_service().get(&worker_id).await;
        let worker_status = Ctx::get_assumed_worker_status(self, &worker_id, &metadata).await;

        if metadata.is_none() {
            // Worker does not exist, we still check if it is in the list active workers due to some inconsistency
            if let Some((_, worker_state)) = self
                .active_workers()
                .enum_workers()
                .iter()
                .find(|(id, _)| *id == worker_id)
            {
                worker_state.set_interrupting(if request.recover_immediately {
                    InterruptKind::Restart
                } else {
                    InterruptKind::Interrupt
                });
            }
        }

        let metadata = metadata.ok_or(GolemError::worker_not_found(worker_id.clone()))?;

        match worker_status {
            WorkerStatus::Exited => {
                warn!("Attempted interrupting worker {worker_id} which already exited")
            }
            WorkerStatus::Idle => {
                warn!("Attempted interrupting worker {worker_id} which is idle")
            }
            WorkerStatus::Failed => {
                warn!("Attempted interrupting worker {worker_id} which is failed")
            }
            WorkerStatus::Interrupted => {
                warn!("Attempted interrupting worker {worker_id} which is already interrupted")
            }
            WorkerStatus::Suspended => {
                debug!("Marking suspended worker {worker_id} as interrupted");
                Ctx::set_worker_status(self, &worker_id, WorkerStatus::Interrupted).await;
            }
            WorkerStatus::Retrying => {
                debug!("Marking worker {worker_id} scheduled to be retried as interrupted");
                Ctx::set_worker_status(self, &worker_id, WorkerStatus::Interrupted).await;
            }
            WorkerStatus::Running => {
                let worker_state = Worker::get_or_create_with_config(
                    self,
                    &metadata.worker_id.worker_id,
                    metadata.args,
                    metadata.env,
                    Some(metadata.worker_id.template_version),
                    metadata.account_id,
                )
                .await?;

                worker_state.set_interrupting(if request.recover_immediately {
                    InterruptKind::Restart
                } else {
                    InterruptKind::Interrupt
                });
            }
        }

        Ok(())
    }

    async fn resume_worker_internal(
        &self,
        request: golem::workerexecutor::ResumeWorkerRequest,
    ) -> Result<(), GolemError> {
        let worker_id = request
            .worker_id
            .ok_or(GolemError::invalid_request("worker_id not found"))?;

        let worker_id = common_model::WorkerId::from_proto(worker_id);

        self.validate_worker_id(&worker_id)?;

        let metadata = self.worker_service().get(&worker_id).await;
        self.validate_worker_status(&worker_id, &metadata).await?;

        let worker_status = Ctx::get_assumed_worker_status(self, &worker_id, &metadata).await;

        match worker_status {
            WorkerStatus::Suspended | WorkerStatus::Interrupted => {
                let metadata = metadata.ok_or(GolemError::invalid_request("Worker not found"))?;
                info!("Activating ${worker_status:?} worker {worker_id} due to explicit resume request");
                Worker::activate(
                    &self.services,
                    &metadata.worker_id.worker_id,
                    metadata.args,
                    metadata.env,
                    Some(metadata.worker_id.template_version),
                    metadata.account_id,
                )
                .await;
                Ok(())
            }
            _ => Err(GolemError::invalid_request(format!(
                "Worker {worker_id} is not suspended or interrupted",
                worker_id = worker_id
            ))),
        }
    }

    async fn invoke_and_await_worker_internal(
        &self,
        request: &golem::workerexecutor::InvokeAndAwaitWorkerRequest,
    ) -> Result<golem::workerexecutor::InvokeAndAwaitWorkerSuccess, GolemError> {
        let full_function_name = request.name();

        let proto_function_input: Vec<Val> = request.input();
        let function_input = proto_function_input
            .iter()
            .map(|val| val.clone().try_into())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|msg| GolemError::ValueMismatch { details: msg })?;

        let calling_convention = request.calling_convention();
        let invocation_key = request.invocation_key()?;

        let worker_details = self.get_or_create(request).await?;
        let values = invoke_and_await(
            worker_details,
            self,
            invocation_key,
            calling_convention.into(),
            full_function_name,
            function_input,
        )
        .await?;
        let output = values.into_iter().map(|val| val.into()).collect();
        Ok(golem::workerexecutor::InvokeAndAwaitWorkerSuccess { output })
    }

    async fn get_or_create<Req: GrpcInvokeRequest>(
        &self,
        request: &Req,
    ) -> Result<Arc<Worker<Ctx>>, GolemError> {
        let worker_id = request.worker_id()?;
        let account_id: AccountId = request.account_id()?;

        self.validate_worker_id(&worker_id)?;

        let metadata = self.worker_service().get(&worker_id).await;
        self.validate_worker_status(&worker_id, &metadata).await?;

        if let Some(limits) = request.account_limits() {
            Ctx::record_last_known_limits(self, &account_id, &limits.into()).await?;
        }

        let (worker_args, worker_env, template_version, account_id) = match metadata {
            Some(metadata) => (
                metadata.args,
                metadata.env,
                Some(metadata.worker_id.template_version),
                metadata.account_id,
            ),
            None => (vec![], vec![], None, account_id),
        };

        Worker::get_or_create_with_config(
            self,
            &worker_id,
            worker_args.clone(),
            worker_env.clone(),
            template_version,
            account_id,
        )
        .await
    }

    async fn invoke_worker_internal<Req: GrpcInvokeRequest>(
        &self,
        request: &Req,
    ) -> Result<Option<Result<Vec<Val>, GolemError>>, GolemError> {
        let full_function_name = request.name();

        let proto_function_input: Vec<Val> = request.input();
        let function_input = proto_function_input
            .iter()
            .map(|val| val.clone().try_into())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|msg| GolemError::ValueMismatch { details: msg })?;

        let calling_convention = request.calling_convention();
        let invocation_key = request.invocation_key()?;

        let worker_details = self.get_or_create(request).await?;
        let result = invoke(
            worker_details,
            self,
            invocation_key,
            calling_convention,
            full_function_name,
            function_input,
        )
        .await?;
        Ok(result.map(|inner_result| {
            inner_result.map(|values| values.into_iter().map(|val| val.into()).collect())
        }))
    }

    async fn revoke_shards_internal(
        &self,
        request: golem::workerexecutor::RevokeShardsRequest,
    ) -> Result<(), GolemError> {
        let proto_shard_ids = request.shard_ids;

        let shard_ids = proto_shard_ids.into_iter().map(ShardId::from).collect();

        self.shard_service().revoke_shards(&shard_ids);

        let workers = self.active_workers().enum_workers();

        for (worker_id, worker_details) in workers {
            if self.shard_service().check_worker(&worker_id).is_err() {
                if let Some(mut await_interrupted) =
                    worker_details.set_interrupting(InterruptKind::Restart)
                {
                    await_interrupted.recv().await.unwrap();
                }
            }
        }

        Ok(())
    }

    async fn assign_shards_internal(
        &self,
        request: golem::workerexecutor::AssignShardsRequest,
    ) -> Result<(), GolemError> {
        let proto_shard_ids = request.shard_ids;

        let shard_ids = proto_shard_ids.into_iter().map(ShardId::from).collect();

        self.shard_service().assign_shards(&shard_ids);
        Ctx::on_shard_assignment_changed(self).await?;

        Ok(())
    }

    async fn get_worker_metadata_internal(
        &self,
        worker_id: &golem::worker::WorkerId,
    ) -> Result<golem::worker::WorkerMetadata, GolemError> {
        let worker_id = common_model::WorkerId::from_proto(worker_id.clone());
        let metadata = self
            .worker_service()
            .get(&worker_id)
            .await
            .ok_or(GolemError::worker_not_found(worker_id.clone()))?;

        let retry_count = Ctx::get_worker_retry_count(self, &worker_id).await as i32;

        Ok(golem::worker::WorkerMetadata {
            worker_id: Some(worker_id.into_proto()),
            args: metadata.args.clone(),
            env: HashMap::from_iter(metadata.env.iter().cloned()),
            account_id: Some(metadata.account_id.into()),
            template_version: metadata.worker_id.template_version,
            status: Into::<golem::worker::WorkerStatus>::into(metadata.last_known_status.status)
                .into(),
            retry_count,
        })
    }
}

impl<Ctx: WorkerCtx, Svcs: HasAll<Ctx> + UsesAllDeps<Ctx = Ctx> + Send + Sync + 'static> UsesAllDeps
    for WorkerExecutorImpl<Ctx, Svcs>
{
    type Ctx = Ctx;

    fn all(&self) -> &All<Ctx> {
        self.services.all()
    }
}

#[tonic::async_trait]
impl<Ctx: WorkerCtx, Svcs: HasAll<Ctx> + UsesAllDeps<Ctx = Ctx> + Send + Sync + 'static>
    WorkerExecutor for WorkerExecutorImpl<Ctx, Svcs>
{
    async fn create_worker(
        &self,
        request: Request<golem::workerexecutor::CreateWorkerRequest>,
    ) -> Result<Response<golem::workerexecutor::CreateWorkerResponse>, Status> {
        let request = request.into_inner();
        let record = RecordedGrpcRequest::new(
            "create_instance",
            format!(
                "worker_id={:?}, template_version={:?}, account_id={:?}",
                request.worker_id, request.template_version, request.account_id
            ),
        );
        match self.create_worker_internal(request).await {
            Ok(_) => record.succeed(Ok(Response::new(
                golem::workerexecutor::CreateWorkerResponse {
                    result: Some(
                        golem::workerexecutor::create_worker_response::Result::Success(
                            golem::common::Empty {},
                        ),
                    ),
                },
            ))),
            Err(err) => record.fail(
                Ok(Response::new(golem::workerexecutor::CreateWorkerResponse {
                    result: Some(
                        golem::workerexecutor::create_worker_response::Result::Failure(
                            err.clone().into(),
                        ),
                    ),
                })),
                &err,
            ),
        }
    }

    async fn get_invocation_key(
        &self,
        request: Request<golem::workerexecutor::GetInvocationKeyRequest>,
    ) -> Result<Response<golem::workerexecutor::GetInvocationKeyResponse>, Status> {
        let request = request.into_inner();
        let record = RecordedGrpcRequest::new(
            "get_invocation_key",
            format!("worker_id={:?}", request.worker_id),
        );
        match self.get_invocation_key_internal(request).await {
            Ok(result) => record.succeed(Ok(Response::new(
                golem::workerexecutor::GetInvocationKeyResponse {
                    result: Some(
                        golem::workerexecutor::get_invocation_key_response::Result::Success(result),
                    ),
                },
            ))),
            Err(err) => record.fail(
                Ok(Response::new(
                    golem::workerexecutor::GetInvocationKeyResponse {
                        result: Some(
                            golem::workerexecutor::get_invocation_key_response::Result::Failure(
                                err.clone().into(),
                            ),
                        ),
                    },
                )),
                &err,
            ),
        }
    }

    async fn invoke_and_await_worker(
        &self,
        request: Request<golem::workerexecutor::InvokeAndAwaitWorkerRequest>,
    ) -> Result<Response<golem::workerexecutor::InvokeAndAwaitWorkerResponse>, Status> {
        let request = request.into_inner();
        let record = RecordedGrpcRequest::new(
            "invoke_and_await_instance",
            format!(
                "worker_id={:?}, name={:?}, invocation_key={:?}, calling_convention={:?}, account_id={:?}",
                request.worker_id, request.name, request.invocation_key, request.calling_convention, request.account_id
            ),
        );
        match self.invoke_and_await_worker_internal(&request).await {
            Ok(result) => record.succeed(Ok(Response::new(
                golem::workerexecutor::InvokeAndAwaitWorkerResponse {
                    result: Some(
                        golem::workerexecutor::invoke_and_await_worker_response::Result::Success(result),
                    ),
                },
            ))),
            Err(err) => record.fail(
                Ok(Response::new(
                    golem::workerexecutor::InvokeAndAwaitWorkerResponse {
                        result: Some(
                            golem::workerexecutor::invoke_and_await_worker_response::Result::Failure(
                                err.clone().into(),
                            ),
                        ),
                    },
                )),
                &err,
            ),
        }
    }

    async fn invoke_worker(
        &self,
        request: Request<golem::workerexecutor::InvokeWorkerRequest>,
    ) -> Result<Response<golem::workerexecutor::InvokeWorkerResponse>, Status> {
        let request = request.into_inner();
        let record = RecordedGrpcRequest::new(
            "invoke_instance",
            format!(
                "worker_id={:?}, name={:?}, account_id={:?}",
                request.worker_id, request.name, request.account_id
            ),
        );
        match self.invoke_worker_internal(&request).await {
            Ok(_) => record.succeed(Ok(Response::new(
                golem::workerexecutor::InvokeWorkerResponse {
                    result: Some(
                        golem::workerexecutor::invoke_worker_response::Result::Success(
                            golem::common::Empty {},
                        ),
                    ),
                },
            ))),
            Err(err) => record.fail(
                Ok(Response::new(golem::workerexecutor::InvokeWorkerResponse {
                    result: Some(
                        golem::workerexecutor::invoke_worker_response::Result::Failure(
                            err.clone().into(),
                        ),
                    ),
                })),
                &err,
            ),
        }
    }

    type ConnectWorkerStream = ResponseStream;

    async fn connect_worker(
        &self,
        request: Request<golem::workerexecutor::ConnectWorkerRequest>,
    ) -> ResponseResult<Self::ConnectWorkerStream> {
        let inner = request.into_inner();

        let worker_id = common_model::WorkerId::from_proto(
            inner
                .worker_id
                .ok_or(GolemError::invalid_request("missing worker_id"))?,
        );
        let account_id: AccountId = inner
            .account_id
            .ok_or(GolemError::invalid_request("missing account_id"))?
            .into();

        self.validate_worker_id(&worker_id)?;

        let metadata = self.worker_service().get(&worker_id).await;
        let status = self
            .validate_worker_status(&worker_id, &metadata)
            .await
            .map_err(|e| {
                error!("Failed to connect to instance {worker_id}: {:?}", e);
                Status::internal(format!("Error connecting to instance: {e}"))
            })?;

        if status != WorkerStatus::Interrupted {
            let metadata = metadata.ok_or(Status::not_found("Instance not found"))?;

            let event_service = match Worker::get_or_create_pending(
                self,
                metadata.worker_id.worker_id,
                metadata.args,
                metadata.env,
                Some(metadata.worker_id.template_version),
                account_id,
            )
            .await?
            {
                PendingOrFinal::Pending(pending) => pending.event_service.clone(),
                PendingOrFinal::Final(worker_details) => {
                    worker_details.public_state.event_service().clone()
                }
            };

            let mut receiver = event_service.receiver();

            info!("Client connected to {worker_id}");
            record_new_grpc_active_stream();

            // spawn and channel are required if you want handle "disconnect" functionality
            // the `out_stream` will not be polled after client disconnect
            let (tx, rx) = mpsc::channel(128);

            tokio::spawn(async move {
                while let Ok(item) = receiver.recv().await {
                    match item {
                        worker_event::WorkerEvent::Close => {
                            break;
                        }
                        worker_event::WorkerEvent::StdOut(line) => {
                            match tx
                                .send(Result::<_, Status>::Ok(golem::worker::LogEvent {
                                    event: Some(golem::worker::log_event::Event::Stdout(
                                        golem::worker::StdOutLog {
                                            message: String::from_utf8(line).unwrap(),
                                        },
                                    )),
                                }))
                                .await
                            {
                                Ok(_) => {
                                    // item (server response) was queued to be send to client
                                }
                                Err(_item) => {
                                    // output_stream was build from rx and both are dropped
                                    break;
                                }
                            }
                        }
                        worker_event::WorkerEvent::StdErr(line) => {
                            match tx
                                .send(Result::<_, Status>::Ok(golem::worker::LogEvent {
                                    event: Some(golem::worker::log_event::Event::Stderr(
                                        golem::worker::StdErrLog {
                                            message: String::from_utf8(line).unwrap(),
                                        },
                                    )),
                                }))
                                .await
                            {
                                Ok(_) => {
                                    // item (server response) was queued to be send to client
                                }
                                Err(_item) => {
                                    // output_stream was build from rx and both are dropped
                                    break;
                                }
                            }
                        }
                        worker_event::WorkerEvent::Log {
                            level,
                            context,
                            message,
                        } => match tx
                            .send(Result::<_, Status>::Ok(golem::worker::LogEvent {
                                event: Some(golem::worker::log_event::Event::Log(
                                    golem::worker::Log {
                                        level: match level {
                                            LogLevel::Trace => golem::worker::Level::Trace.into(),
                                            LogLevel::Debug => golem::worker::Level::Debug.into(),
                                            LogLevel::Info => golem::worker::Level::Info.into(),
                                            LogLevel::Warn => golem::worker::Level::Warn.into(),
                                            LogLevel::Error => golem::worker::Level::Error.into(),
                                            LogLevel::Critical => {
                                                golem::worker::Level::Critical.into()
                                            }
                                        },
                                        context,
                                        message,
                                    },
                                )),
                            }))
                            .await
                        {
                            Ok(_) => {
                                // item (server response) was queued to be send to client
                            }
                            Err(_item) => {
                                // output_stream was build from rx and both are dropped
                                break;
                            }
                        },
                    }
                }

                record_closed_grpc_active_stream();
                info!("Client disconnected from {worker_id}");
            });

            let output_stream = ReceiverStream::new(rx);
            Ok(Response::new(output_stream))
        } else {
            // We don't want 'connect' to resume interrupted workers
            Err(GolemError::Interrupted {
                kind: InterruptKind::Interrupt,
            }
            .into())
        }
    }

    async fn delete_worker(
        &self,
        request: Request<golem::worker::WorkerId>,
    ) -> Result<Response<golem::workerexecutor::DeleteWorkerResponse>, Status> {
        let request = request.into_inner();
        let record =
            RecordedGrpcRequest::new("delete_instance", format!("worker_id={:?}", request));
        match self.delete_worker_internal(request).await {
            Ok(_) => record.succeed(Ok(Response::new(
                golem::workerexecutor::DeleteWorkerResponse {
                    result: Some(
                        golem::workerexecutor::delete_worker_response::Result::Success(
                            golem::common::Empty {},
                        ),
                    ),
                },
            ))),
            Err(err) => record.fail(
                Ok(Response::new(golem::workerexecutor::DeleteWorkerResponse {
                    result: Some(
                        golem::workerexecutor::delete_worker_response::Result::Failure(
                            err.clone().into(),
                        ),
                    ),
                })),
                &err,
            ),
        }
    }

    async fn complete_promise(
        &self,
        request: Request<golem::workerexecutor::CompletePromiseRequest>,
    ) -> Result<Response<golem::workerexecutor::CompletePromiseResponse>, Status> {
        let request = request.into_inner();
        let record = RecordedGrpcRequest::new(
            "complete_promise",
            format!("promise_id={:?}", request.promise_id),
        );
        match self.complete_promise_internal(request).await {
            Ok(success) => record.succeed(Ok(Response::new(
                golem::workerexecutor::CompletePromiseResponse {
                    result: Some(
                        golem::workerexecutor::complete_promise_response::Result::Success(success),
                    ),
                },
            ))),
            Err(err) => record.fail(
                Ok(Response::new(
                    golem::workerexecutor::CompletePromiseResponse {
                        result: Some(
                            golem::workerexecutor::complete_promise_response::Result::Failure(
                                err.clone().into(),
                            ),
                        ),
                    },
                )),
                &err,
            ),
        }
    }

    async fn interrupt_worker(
        &self,
        request: Request<golem::workerexecutor::InterruptWorkerRequest>,
    ) -> Result<Response<golem::workerexecutor::InterruptWorkerResponse>, Status> {
        let request = request.into_inner();
        let record = RecordedGrpcRequest::new(
            "interrupt_instance",
            format!("worker_id={:?}", request.worker_id),
        );
        match self.interrupt_worker_internal(request).await {
            Ok(_) => record.succeed(Ok(Response::new(
                golem::workerexecutor::InterruptWorkerResponse {
                    result: Some(
                        golem::workerexecutor::interrupt_worker_response::Result::Success(
                            golem::common::Empty {},
                        ),
                    ),
                },
            ))),
            Err(err) => record.fail(
                Ok(Response::new(
                    golem::workerexecutor::InterruptWorkerResponse {
                        result: Some(
                            golem::workerexecutor::interrupt_worker_response::Result::Failure(
                                err.clone().into(),
                            ),
                        ),
                    },
                )),
                &err,
            ),
        }
    }

    async fn revoke_shards(
        &self,
        request: Request<golem::workerexecutor::RevokeShardsRequest>,
    ) -> Result<Response<golem::workerexecutor::RevokeShardsResponse>, Status> {
        let request = request.into_inner();
        let record = RecordedGrpcRequest::new(
            "revoke_shards",
            format!("shard_ids={:?}", request.shard_ids),
        );
        match self.revoke_shards_internal(request).await {
            Ok(_) => record.succeed(Ok(Response::new(
                golem::workerexecutor::RevokeShardsResponse {
                    result: Some(
                        golem::workerexecutor::revoke_shards_response::Result::Success(
                            golem::common::Empty {},
                        ),
                    ),
                },
            ))),
            Err(err) => record.fail(
                Ok(Response::new(golem::workerexecutor::RevokeShardsResponse {
                    result: Some(
                        golem::workerexecutor::revoke_shards_response::Result::Failure(
                            err.clone().into(),
                        ),
                    ),
                })),
                &err,
            ),
        }
    }

    async fn assign_shards(
        &self,
        request: Request<golem::workerexecutor::AssignShardsRequest>,
    ) -> Result<Response<golem::workerexecutor::AssignShardsResponse>, Status> {
        let request = request.into_inner();
        let record = RecordedGrpcRequest::new(
            "assign_shards",
            format!("shard_ids={:?}", request.shard_ids),
        );
        match self.assign_shards_internal(request).await {
            Ok(_) => record.succeed(Ok(Response::new(
                golem::workerexecutor::AssignShardsResponse {
                    result: Some(
                        golem::workerexecutor::assign_shards_response::Result::Success(
                            golem::common::Empty {},
                        ),
                    ),
                },
            ))),
            Err(err) => record.fail(
                Ok(Response::new(golem::workerexecutor::AssignShardsResponse {
                    result: Some(
                        golem::workerexecutor::assign_shards_response::Result::Failure(
                            err.clone().into(),
                        ),
                    ),
                })),
                &err,
            ),
        }
    }

    async fn get_worker_metadata(
        &self,
        request: Request<golem::worker::WorkerId>,
    ) -> Result<Response<golem::workerexecutor::GetWorkerMetadataResponse>, Status> {
        let worker_id = request.into_inner();
        let record =
            RecordedGrpcRequest::new("get_worker_metadata", format!("worker_id={:?}", worker_id));
        let result = self.get_worker_metadata_internal(&worker_id).await;
        match result {
            Ok(result) => record.succeed(Ok(Response::new(
                golem::workerexecutor::GetWorkerMetadataResponse {
                    result: Some(
                        golem::workerexecutor::get_worker_metadata_response::Result::Success(
                            result,
                        ),
                    ),
                },
            ))),
            Err(err @ GolemError::WorkerNotFound { .. }) => record.succeed(Ok(Response::new(
                golem::workerexecutor::GetWorkerMetadataResponse {
                    result: Some(
                        golem::workerexecutor::get_worker_metadata_response::Result::Failure(
                            err.clone().into(),
                        ),
                    ),
                },
            ))),
            Err(err) => record.fail(
                Ok(Response::new(
                    golem::workerexecutor::GetWorkerMetadataResponse {
                        result: Some(
                            golem::workerexecutor::get_worker_metadata_response::Result::Failure(
                                err.clone().into(),
                            ),
                        ),
                    },
                )),
                &err,
            ),
        }
    }

    async fn resume_worker(
        &self,
        request: Request<golem::workerexecutor::ResumeWorkerRequest>,
    ) -> Result<Response<golem::workerexecutor::ResumeWorkerResponse>, Status> {
        let request = request.into_inner();
        let record = RecordedGrpcRequest::new(
            "resume_instance",
            format!("worker_id={:?}", request.worker_id),
        );
        match self.resume_worker_internal(request).await {
            Ok(_) => record.succeed(Ok(Response::new(
                golem::workerexecutor::ResumeWorkerResponse {
                    result: Some(
                        golem::workerexecutor::resume_worker_response::Result::Success(
                            golem::common::Empty {},
                        ),
                    ),
                },
            ))),
            Err(err) => record.fail(
                Ok(Response::new(golem::workerexecutor::ResumeWorkerResponse {
                    result: Some(
                        golem::workerexecutor::resume_worker_response::Result::Failure(
                            err.clone().into(),
                        ),
                    ),
                })),
                &err,
            ),
        }
    }
}

trait GrpcInvokeRequest {
    fn account_id(&self) -> Result<AccountId, GolemError>;
    fn account_limits(&self) -> Option<GrpcResourceLimits>;
    fn calling_convention(&self) -> CallingConvention;
    fn input(&self) -> Vec<Val>;
    fn worker_id(&self) -> Result<common_model::WorkerId, GolemError>;
    fn invocation_key(&self) -> Result<Option<InvocationKey>, GolemError>;
    fn name(&self) -> String;
}

impl GrpcInvokeRequest for golem::workerexecutor::InvokeWorkerRequest {
    fn account_id(&self) -> Result<AccountId, GolemError> {
        Ok(self
            .account_id
            .clone()
            .ok_or(GolemError::invalid_request("account_id not found"))?
            .into())
    }

    fn account_limits(&self) -> Option<GrpcResourceLimits> {
        self.account_limits.clone()
    }

    fn calling_convention(&self) -> CallingConvention {
        CallingConvention::Component
    }

    fn input(&self) -> Vec<Val> {
        self.input.clone()
    }

    fn worker_id(&self) -> Result<common_model::WorkerId, GolemError> {
        Ok(common_model::WorkerId::from_proto(
            self.worker_id
                .clone()
                .ok_or(GolemError::invalid_request("worker_id not found"))?,
        ))
    }

    fn invocation_key(&self) -> Result<Option<InvocationKey>, GolemError> {
        Ok(None)
    }

    fn name(&self) -> String {
        self.name.clone()
    }
}

impl GrpcInvokeRequest for golem::workerexecutor::InvokeAndAwaitWorkerRequest {
    fn account_id(&self) -> Result<AccountId, GolemError> {
        Ok(self
            .account_id
            .clone()
            .ok_or(GolemError::invalid_request("account_id not found"))?
            .into())
    }

    fn account_limits(&self) -> Option<GrpcResourceLimits> {
        self.account_limits.clone()
    }

    fn calling_convention(&self) -> CallingConvention {
        match self.calling_convention() {
            golem::worker::CallingConvention::Component => CallingConvention::Component,
            golem::worker::CallingConvention::Stdio => CallingConvention::Stdio,
            golem::worker::CallingConvention::StdioEventloop => CallingConvention::StdioEventloop,
        }
    }

    fn input(&self) -> Vec<Val> {
        self.input.clone()
    }

    fn worker_id(&self) -> Result<common_model::WorkerId, GolemError> {
        Ok(common_model::WorkerId::from_proto(
            self.worker_id
                .clone()
                .ok_or(GolemError::invalid_request("worker_id not found"))?,
        ))
    }

    fn invocation_key(&self) -> Result<Option<InvocationKey>, GolemError> {
        self.invocation_key
            .clone()
            .ok_or(GolemError::invalid_request(
                "invocation_key not found in InvokeAndAwaitWorkerRequest",
            ))
            .map(|key| Some(InvocationKey::from(key)))
    }

    fn name(&self) -> String {
        self.name.clone()
    }
}

pub trait UriBackConversion {
    fn as_http_02(&self) -> http_02::Uri;
}

impl UriBackConversion for http::Uri {
    fn as_http_02(&self) -> http_02::Uri {
        self.to_string().parse().unwrap()
    }
}

pub fn authorised_grpc_request<T>(request: T, access_token: &Uuid) -> Request<T> {
    let mut req = Request::new(request);
    req.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", access_token).parse().unwrap(),
    );
    req
}
