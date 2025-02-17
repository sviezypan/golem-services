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

use core::task::{Context, Poll};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use golem_api_grpc::proto::golem::worker::{InvokeResult as ProtoInvokeResult, LogEvent};
use golem_api_grpc::proto::golem::workerexecutor;
use golem_api_grpc::proto::golem::workerexecutor::worker_executor_client::WorkerExecutorClient;
use golem_api_grpc::proto::golem::workerexecutor::{
    CompletePromiseRequest, ConnectWorkerRequest, CreateWorkerRequest, GetInvocationKeyRequest,
    InterruptWorkerRequest, InvokeAndAwaitWorkerRequest, InvokeWorkerRequest, ResumeWorkerRequest,
};
use golem_common::model::{
    AccountId, CallingConvention, InvocationKey, ShardId, TemplateId, WorkerStatus,
};
use golem_wasm_ast::analysis::AnalysedFunctionResult;
use golem_wasm_rpc::protobuf::Val as ProtoVal;
use serde_json::Value;
use tokio::time::sleep;
use tokio_stream::Stream;
use tonic::transport::Channel;
use tonic::{Status, Streaming};
use tracing::{debug, info};

use crate::service::template::{TemplateError, TemplateService};
use golem_service_base::model::*;
use golem_service_base::routing_table::{RoutingTableError, RoutingTableService};
use golem_service_base::typechecker::{TypeCheckIn, TypeCheckOut};
use golem_service_base::worker_executor_clients::WorkerExecutorClients;

pub struct ConnectWorkerStream {
    stream: tokio_stream::wrappers::ReceiverStream<Result<LogEvent, Status>>,
    cancel: tokio_util::sync::CancellationToken,
}

impl ConnectWorkerStream {
    pub fn new(streaming: Streaming<LogEvent>) -> Self {
        // Create a channel which is Send and Sync.
        // Streaming is not Sync.
        let (sender, receiver) = tokio::sync::mpsc::channel(32);
        let mut streaming = streaming;

        let cancel = tokio_util::sync::CancellationToken::new();

        tokio::spawn({
            let cancel = cancel.clone();

            let forward_loop = {
                let sender = sender.clone();
                async move {
                    while let Some(message) = streaming.next().await {
                        if let Err(error) = sender.send(message).await {
                            tracing::info!("Failed to forward WorkerStream: {error}");
                            break;
                        }
                    }
                }
            };

            async move {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        tracing::info!("WorkerStream cancelled");
                    }
                    _ = forward_loop => {
                        tracing::info!("WorkerStream forward loop finished");
                    }
                };
                sender.closed().await;
            }
        });

        let stream = tokio_stream::wrappers::ReceiverStream::new(receiver);

        Self { stream, cancel }
    }
}

impl Stream for ConnectWorkerStream {
    type Item = Result<LogEvent, Status>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<LogEvent, Status>>> {
        self.stream.poll_next_unpin(cx)
    }
}

impl Drop for ConnectWorkerStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub enum WorkerError {
    Internal(String),
    TypeCheckerError(String),
    DelegatedTemplateServiceError(TemplateError),
    VersionedTemplateIdNotFound(VersionedTemplateId),
    TemplateNotFound(TemplateId),
    AccountIdNotFound(AccountId),
    // FIXME: Once worker is independent of account
    WorkerNotFound(WorkerId),
    Golem(GolemError),
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            WorkerError::Internal(ref string) => write!(f, "Internal error: {}", string),
            WorkerError::TypeCheckerError(ref string) => {
                write!(f, "Type checker error: {}", string)
            }
            WorkerError::DelegatedTemplateServiceError(ref error) => {
                write!(f, "Delegated template service error: {}", error)
            }
            WorkerError::VersionedTemplateIdNotFound(ref versioned_template_id) => write!(
                f,
                "Versioned template id not found: {}",
                versioned_template_id
            ),
            WorkerError::TemplateNotFound(ref template_id) => {
                write!(f, "Template not found: {}", template_id)
            }
            WorkerError::AccountIdNotFound(ref account_id) => {
                write!(f, "Account id not found: {}", account_id)
            }
            WorkerError::WorkerNotFound(ref worker_id) => {
                write!(f, "Worker not found: {}", worker_id)
            }
            WorkerError::Golem(ref error) => write!(f, "Golem error: {:?}", error),
        }
    }
}

impl From<RoutingTableError> for WorkerError {
    fn from(error: RoutingTableError) -> Self {
        WorkerError::Internal(format!("Unable to get routing table: {:?}", error))
    }
}

impl From<TemplateError> for WorkerError {
    fn from(error: TemplateError) -> Self {
        WorkerError::DelegatedTemplateServiceError(error)
    }
}

#[async_trait]
pub trait WorkerService {
    async fn get_by_id(&self, worker_id: &WorkerId) -> Result<VersionedWorkerId, WorkerError>;

    async fn create(
        &self,
        worker_id: &WorkerId,
        template_version: i32,
        arguments: Vec<String>,
        environment_variables: HashMap<String, String>,
    ) -> Result<VersionedWorkerId, WorkerError>;

    async fn connect(&self, worker_id: &WorkerId) -> Result<ConnectWorkerStream, WorkerError>;

    async fn delete(&self, worker_id: &WorkerId) -> Result<(), WorkerError>;

    async fn get_invocation_key(&self, worker_id: &WorkerId) -> Result<InvocationKey, WorkerError>;

    async fn invoke_and_await_function(
        &self,
        worker_id: &WorkerId,
        function_name: String,
        invocation_key: &InvocationKey,
        params: Value,
        calling_convention: &CallingConvention,
    ) -> Result<Value, WorkerError>;

    async fn invoke_and_await_function_proto(
        &self,
        worker_id: &WorkerId,
        function_name: String,
        invocation_key: &InvocationKey,
        params: Vec<ProtoVal>,
        calling_convention: &CallingConvention,
    ) -> Result<ProtoInvokeResult, WorkerError>;

    async fn invoke_function(
        &self,
        worker_id: &WorkerId,
        function_name: String,
        params: Value,
    ) -> Result<(), WorkerError>;

    async fn invoke_fn_proto(
        &self,
        worker_id: &WorkerId,
        function_name: String,
        params: Vec<ProtoVal>,
    ) -> Result<(), WorkerError>;

    async fn complete_promise(
        &self,
        worker_id: &WorkerId,
        oplog_id: i32,
        data: Vec<u8>,
    ) -> Result<bool, WorkerError>;

    async fn interrupt(
        &self,
        worker_id: &WorkerId,
        recover_immediately: bool,
    ) -> Result<(), WorkerError>;

    async fn get_metadata(&self, worker_id: &WorkerId) -> Result<WorkerMetadata, WorkerError>;

    async fn resume(&self, worker_id: &WorkerId) -> Result<(), WorkerError>;
}

pub struct WorkerServiceDefault {
    worker_executor_clients: Arc<dyn WorkerExecutorClients + Send + Sync>,
    template_service: Arc<dyn TemplateService + Send + Sync>,
    routing_table_service: Arc<dyn RoutingTableService + Send + Sync>,
}

impl WorkerServiceDefault {
    pub fn new(
        worker_executor_clients: Arc<dyn WorkerExecutorClients + Send + Sync>,
        template_service: Arc<dyn TemplateService + Send + Sync>,
        routing_table_service: Arc<dyn RoutingTableService + Send + Sync>,
    ) -> Self {
        Self {
            worker_executor_clients,
            template_service,
            routing_table_service,
        }
    }

    async fn try_get_template_for_worker(
        &self,
        worker_id: &WorkerId,
    ) -> Result<Template, WorkerError> {
        match self.get_metadata(worker_id).await {
            Ok(metadata) => {
                let template_version = metadata.template_version;
                let template_details = self
                    .template_service
                    .get_by_version(&worker_id.template_id, template_version)
                    .await?
                    .ok_or_else(|| {
                        WorkerError::VersionedTemplateIdNotFound(VersionedTemplateId {
                            template_id: worker_id.template_id.clone(),
                            version: template_version,
                        })
                    })?;

                Ok(template_details)
            }
            Err(WorkerError::WorkerNotFound(_)) => Ok(self
                .template_service
                .get_latest(&worker_id.template_id)
                .await?),
            Err(WorkerError::Golem(GolemError::WorkerNotFound(_))) => Ok(self
                .template_service
                .get_latest(&worker_id.template_id)
                .await?),
            Err(other) => Err(other),
        }
    }

    async fn get_worker_executor_client(
        &self,
        worker_id: &WorkerId,
    ) -> Result<Option<WorkerExecutorClient<Channel>>, WorkerError> {
        let routing_table = self.routing_table_service.get_routing_table().await?;
        match routing_table.lookup(worker_id) {
            None => Ok(None),
            Some(pod) => {
                let worker_executor_client = self
                    .worker_executor_clients
                    .lookup(pod.clone())
                    .await
                    .map_err(|err| {
                        WorkerError::Internal(format!(
                            "No client for pod {:?} derived from ShardId {} of {:?}. {}",
                            pod,
                            ShardId::from_worker_id(
                                &worker_id.clone().into(),
                                routing_table.number_of_shards.value,
                            ),
                            worker_id,
                            err
                        ))
                    })?;
                Ok(Some(worker_executor_client))
            }
        }
    }

    async fn retry_on_invalid_shard_id<F, In, Out>(
        &self,
        worker_id: &WorkerId,
        i: &In,
        f: F,
    ) -> Result<Out, WorkerError>
    where
        F: for<'b> Fn(
            &'b mut WorkerExecutorClient<Channel>,
            &'b In,
        )
            -> Pin<Box<dyn Future<Output = Result<Out, GolemError>> + 'b + Send>>,
    {
        loop {
            match self.get_worker_executor_client(worker_id).await {
                Ok(Some(mut worker_executor_client)) => {
                    match f(&mut worker_executor_client, i).await {
                        Ok(result) => return Ok(result),
                        Err(GolemError::InvalidShardId(GolemErrorInvalidShardId {
                            shard_id,
                            shard_ids,
                        })) => {
                            info!("InvalidShardId: {} not in {:?}", shard_id, shard_ids);
                            info!("Invalidating routing table");
                            self.routing_table_service.invalidate_routing_table().await;
                            sleep(Duration::from_secs(1)).await;
                        }
                        Err(GolemError::RuntimeError(GolemErrorRuntimeError { details }))
                            if details.contains("UNAVAILABLE")
                                || details.contains("CHANNEL CLOSED")
                                || details.contains("transport error") =>
                        {
                            info!("Worker executor unavailable");
                            info!("Invalidating routing table");
                            self.routing_table_service.invalidate_routing_table().await;
                            sleep(Duration::from_secs(1)).await;
                        }
                        Err(other) => {
                            debug!("Got {:?}, not retrying", other);
                            return Err(WorkerError::Golem(other));
                        }
                    }
                }
                Ok(None) => {
                    info!("No active shards");
                    info!("Invalidating routing table");
                    self.routing_table_service.invalidate_routing_table().await;
                    sleep(Duration::from_secs(1)).await;
                }
                Err(WorkerError::Internal { 0: details })
                    if details.contains("transport error") =>
                {
                    info!("Shard manager unavailable");
                    info!("Invalidating routing table");
                    self.routing_table_service.invalidate_routing_table().await;
                    sleep(Duration::from_secs(1)).await;
                }
                Err(other) => {
                    debug!("Got {}, not retrying", other);
                    return Err(other);
                }
            }
        }
    }
}

#[async_trait]
impl WorkerService for WorkerServiceDefault {
    async fn get_by_id(&self, worker_id: &WorkerId) -> Result<VersionedWorkerId, WorkerError> {
        Ok(VersionedWorkerId {
            worker_id: worker_id.clone(),
            template_version_used: 0,
        })
    }

    async fn create(
        &self,
        worker_id: &WorkerId,
        template_version: i32,
        arguments: Vec<String>,
        environment_variables: HashMap<String, String>,
    ) -> Result<VersionedWorkerId, WorkerError> {
        self.retry_on_invalid_shard_id(
            &worker_id.clone(),
            &(worker_id.clone(), template_version, arguments, environment_variables),
            |worker_executor_client, (worker_id, template_version, args, env)| {
                Box::pin(async move {
                    let response: tonic::Response<workerexecutor::CreateWorkerResponse> = worker_executor_client
                        .create_worker(
                            CreateWorkerRequest {
                                worker_id: Some(worker_id.clone().into()),
                                template_version: *template_version,
                                args: args.clone(),
                                env: env.clone(),
                                account_id: Some(golem_api_grpc::proto::golem::common::AccountId {
                                    name: "-1".to_string()
                                }),
                                account_limits: None, //FIXME
                            }
                        )
                        .await
                        .map_err(|err| {
                            GolemError::RuntimeError(GolemErrorRuntimeError {
                                details: err.to_string(),
                            })
                        })?;

                    match response.into_inner() {
                        workerexecutor::CreateWorkerResponse {
                            result:
                            Some(workerexecutor::create_worker_response::Result::Success(_))
                        } => Ok(()),
                        workerexecutor::CreateWorkerResponse {
                            result:
                            Some(workerexecutor::create_worker_response::Result::Failure(err)),
                        } => Err(err.try_into().unwrap()),
                        workerexecutor::CreateWorkerResponse { .. } => Err(GolemError::Unknown(GolemErrorUnknown {
                            details: "Empty response".to_string(),
                        }))
                    }
                })
            }).await?;

        Ok(VersionedWorkerId {
            worker_id: worker_id.clone(),
            template_version_used: template_version,
        })
    }

    async fn connect(&self, worker_id: &WorkerId) -> Result<ConnectWorkerStream, WorkerError> {
        match self.get_worker_executor_client(worker_id).await? {
            Some(mut worker_executor_client) => {
                let response = match worker_executor_client
                    .connect_worker(ConnectWorkerRequest {
                        worker_id: Some(worker_id.clone().into()),
                        account_id: Some(golem_api_grpc::proto::golem::common::AccountId {
                            name: "-1".to_string(),
                        }),
                        account_limits: None,
                    })
                    .await
                {
                    Ok(response) => Ok(response),
                    Err(status) => {
                        if status.code() == tonic::Code::NotFound {
                            Err(WorkerError::WorkerNotFound(worker_id.clone()))
                        } else {
                            Err(WorkerError::Internal(status.message().to_string()))
                        }
                    }
                }?;
                Ok(ConnectWorkerStream::new(response.into_inner()))
            }
            None => Err(WorkerError::WorkerNotFound(worker_id.clone())),
        }
    }

    async fn delete(&self, worker_id: &WorkerId) -> Result<(), WorkerError> {
        self.retry_on_invalid_shard_id(
            worker_id,
            worker_id,
            |worker_executor_client, worker_id| {
                Box::pin(async move {
                    let response = worker_executor_client
                        .delete_worker(golem_api_grpc::proto::golem::worker::WorkerId::from(
                            worker_id.clone(),
                        ))
                        .await
                        .map_err(|err| {
                            GolemError::RuntimeError(GolemErrorRuntimeError {
                                details: err.to_string(),
                            })
                        })?;
                    match response.into_inner() {
                        workerexecutor::DeleteWorkerResponse {
                            result: Some(workerexecutor::delete_worker_response::Result::Success(_)),
                        } => Ok(()),
                        workerexecutor::DeleteWorkerResponse {
                            result: Some(workerexecutor::delete_worker_response::Result::Failure(err)),
                        } => Err(err.try_into().unwrap()),
                        workerexecutor::DeleteWorkerResponse { .. } => {
                            Err(GolemError::Unknown(GolemErrorUnknown {
                                details: "Empty response".to_string(),
                            }))
                        }
                    }
                })
            },
        )
            .await?;
        Ok(())
    }

    async fn get_invocation_key(&self, worker_id: &WorkerId) -> Result<InvocationKey, WorkerError> {
        let invocation_key = self
            .retry_on_invalid_shard_id(worker_id, worker_id, |worker_executor_client, worker_id| {
                Box::pin(async move {
                    let response = worker_executor_client
                        .get_invocation_key(GetInvocationKeyRequest {
                            worker_id: Some(worker_id.clone().into()),
                        })
                        .await
                        .map_err(|err| {
                            GolemError::RuntimeError(GolemErrorRuntimeError {
                                details: err.to_string(),
                            })
                        })?;
                    match response.into_inner() {
                        workerexecutor::GetInvocationKeyResponse {
                            result:
                            Some(workerexecutor::get_invocation_key_response::Result::Success(
                                     workerexecutor::GetInvocationKeySuccess {
                                         invocation_key: Some(invocation_key),
                                     },
                                 )),
                        } => Ok(invocation_key.into()),
                        workerexecutor::GetInvocationKeyResponse {
                            result:
                            Some(workerexecutor::get_invocation_key_response::Result::Failure(err)),
                        } => Err(err.try_into().unwrap()),
                        workerexecutor::GetInvocationKeyResponse { .. } => {
                            Err(GolemError::Unknown(GolemErrorUnknown {
                                details: "Empty response".to_string(),
                            }))
                        }
                    }
                })
            })
            .await?;
        Ok(invocation_key)
    }

    async fn invoke_and_await_function(
        &self,
        worker_id: &WorkerId,
        function_name: String,
        invocation_key: &InvocationKey,
        params: Value,
        calling_convention: &CallingConvention,
    ) -> Result<Value, WorkerError> {
        let template_details = self.try_get_template_for_worker(worker_id).await?;

        let function_type = template_details
            .metadata
            .function_by_name(&function_name)
            .ok_or_else(|| {
                WorkerError::TypeCheckerError("Failed to find the function".to_string())
            })?;
        let params_val = params
            .validate_function_parameters(
                function_type
                    .parameters
                    .into_iter()
                    .map(|parameter| parameter.into())
                    .collect(),
                calling_convention.clone(),
            )
            .map_err(|err| WorkerError::TypeCheckerError(err.join(", ")))?;
        let results_val = self
            .invoke_and_await_function_proto(
                worker_id,
                function_name,
                invocation_key,
                params_val,
                calling_convention,
            )
            .await?;

        let function_results: Vec<AnalysedFunctionResult> = function_type
            .results
            .iter()
            .map(|x| x.clone().into())
            .collect();

        let invoke_response_json = results_val
            .result
            .validate_function_result(function_results, calling_convention.clone())
            .map_err(|err| WorkerError::TypeCheckerError(err.join(", ")))?;
        Ok(invoke_response_json)
    }

    async fn invoke_and_await_function_proto(
        &self,
        worker_id: &WorkerId,
        function_name: String,
        invocation_key: &InvocationKey,
        params: Vec<ProtoVal>,
        calling_convention: &CallingConvention,
    ) -> Result<ProtoInvokeResult, WorkerError> {
        let template_details = self.try_get_template_for_worker(worker_id).await?;
        let function_type = template_details
            .metadata
            .function_by_name(&function_name)
            .ok_or_else(|| {
                WorkerError::TypeCheckerError("Failed to find the function".to_string())
            })?;
        let params_val = params
            .validate_function_parameters(
                function_type
                    .parameters
                    .into_iter()
                    .map(|parameter| parameter.into())
                    .collect(),
                calling_convention.clone(),
            )
            .map_err(|err| WorkerError::TypeCheckerError(err.join(", ")))?;

        let invoke_response = self.retry_on_invalid_shard_id(
            worker_id,
            &(worker_id.clone(), function_name, params_val, invocation_key.clone(), calling_convention.clone()),
            |worker_executor_client, (worker_id, function_name, params_val, invocation_key, calling_convention)| {
                Box::pin(async move {
                    let response = worker_executor_client.invoke_and_await_worker(
                        InvokeAndAwaitWorkerRequest {
                            worker_id: Some(worker_id.clone().into()),
                            name: function_name.clone(),
                            input: params_val.clone(),
                            invocation_key: Some(invocation_key.clone().into()),
                            calling_convention: calling_convention.clone().into(),
                            account_id: Some(golem_api_grpc::proto::golem::common::AccountId {
                                name: "-1".to_string(),
                            }),
                            account_limits: None,
                        }
                    ).await.map_err(|err| {
                        GolemError::RuntimeError(GolemErrorRuntimeError {
                            details: err.to_string(),
                        })
                    })?;
                    match response.into_inner() {
                        workerexecutor::InvokeAndAwaitWorkerResponse {
                            result:
                            Some(workerexecutor::invoke_and_await_worker_response::Result::Success(
                                     workerexecutor::InvokeAndAwaitWorkerSuccess {
                                         output,
                                     },
                                 )),
                        } => Ok(ProtoInvokeResult { result: output }),
                        workerexecutor::InvokeAndAwaitWorkerResponse {
                            result:
                            Some(workerexecutor::invoke_and_await_worker_response::Result::Failure(err)),
                        } => Err(err.try_into().unwrap()),
                        workerexecutor::InvokeAndAwaitWorkerResponse { .. } => {
                            Err(GolemError::Unknown(GolemErrorUnknown {
                                details: "Empty response".to_string(),
                            }))
                        }
                    }
                })
            },
        ).await?;
        Ok(invoke_response)
    }

    async fn invoke_function(
        &self,
        worker_id: &WorkerId,
        function_name: String,
        params: Value,
    ) -> Result<(), WorkerError> {
        let template_details = self.try_get_template_for_worker(worker_id).await?;
        let function_type = template_details
            .metadata
            .function_by_name(&function_name)
            .ok_or_else(|| {
                WorkerError::TypeCheckerError("Failed to find the function".to_string())
            })?;
        let params_val = params
            .validate_function_parameters(
                function_type
                    .parameters
                    .into_iter()
                    .map(|parameter| parameter.into())
                    .collect(),
                CallingConvention::Component,
            )
            .map_err(|err| WorkerError::TypeCheckerError(err.join(", ")))?;
        self.invoke_fn_proto(worker_id, function_name.clone(), params_val)
            .await?;
        Ok(())
    }

    async fn invoke_fn_proto(
        &self,
        worker_id: &WorkerId,
        function_name: String,
        params: Vec<ProtoVal>,
    ) -> Result<(), WorkerError> {
        let template_details = self.try_get_template_for_worker(worker_id).await?;
        let function_type = template_details
            .metadata
            .function_by_name(&function_name)
            .ok_or_else(|| {
                WorkerError::TypeCheckerError("Failed to find the function".to_string())
            })?;
        let params_val = params
            .validate_function_parameters(
                function_type
                    .parameters
                    .into_iter()
                    .map(|parameter| parameter.into())
                    .collect(),
                CallingConvention::Component,
            )
            .map_err(|err| WorkerError::TypeCheckerError(err.join(", ")))?;
        self.retry_on_invalid_shard_id(
            worker_id,
            &(
                worker_id.clone(),
                function_name,
                params_val,
            ),
            |worker_executor_client,
             (worker_id, function_name, params_val)| {
                Box::pin(async move {
                    let response = worker_executor_client
                        .invoke_worker(InvokeWorkerRequest {
                            worker_id: Some(worker_id.clone().into()),
                            name: function_name.clone(),
                            input: params_val.clone(),
                            account_id: Some(golem_api_grpc::proto::golem::common::AccountId {
                                name: "-1".to_string(),
                            }), // FIXME
                            account_limits: None, // FIXME
                        })
                        .await
                        .map_err(|err| {
                            GolemError::RuntimeError(GolemErrorRuntimeError {
                                details: err.to_string(),
                            })
                        })?;
                    match response.into_inner() {
                        workerexecutor::InvokeWorkerResponse {
                            result: Some(workerexecutor::invoke_worker_response::Result::Success(_)),
                        } => Ok(()),
                        workerexecutor::InvokeWorkerResponse {
                            result: Some(workerexecutor::invoke_worker_response::Result::Failure(err)),
                        } => Err(err.try_into().unwrap()),
                        workerexecutor::InvokeWorkerResponse { .. } => {
                            Err(GolemError::Unknown(GolemErrorUnknown {
                                details: "Empty response".to_string(),
                            }))
                        }
                    }
                })
            },
        )
            .await?;
        Ok(())
    }

    async fn complete_promise(
        &self,
        worker_id: &WorkerId,
        oplog_id: i32,
        data: Vec<u8>,
    ) -> Result<bool, WorkerError> {
        let promise_id = PromiseId {
            worker_id: worker_id.clone(),
            oplog_idx: oplog_id,
        };
        let result = self
            .retry_on_invalid_shard_id(
                worker_id,
                &(promise_id, data),
                |worker_executor_client, (promise_id, data)| {
                    Box::pin(async move {
                        let response = worker_executor_client
                            .complete_promise(CompletePromiseRequest {
                                promise_id: Some(promise_id.clone().into()),
                                data: data.clone(),
                            })
                            .await
                            .map_err(|err| {
                                GolemError::RuntimeError(GolemErrorRuntimeError {
                                    details: err.to_string(),
                                })
                            })?;
                        match response.into_inner() {
                            workerexecutor::CompletePromiseResponse {
                                result:
                                    Some(workerexecutor::complete_promise_response::Result::Success(
                                        success,
                                    )),
                            } => Ok(success.completed),
                            workerexecutor::CompletePromiseResponse {
                                result:
                                    Some(workerexecutor::complete_promise_response::Result::Failure(
                                        err,
                                    )),
                            } => Err(err.try_into().unwrap()),
                            workerexecutor::CompletePromiseResponse { .. } => {
                                Err(GolemError::Unknown(GolemErrorUnknown {
                                    details: "Empty response".to_string(),
                                }))
                            }
                        }
                    })
                },
            )
            .await?;
        Ok(result)
    }

    async fn interrupt(
        &self,
        worker_id: &WorkerId,
        recover_immediately: bool,
    ) -> Result<(), WorkerError> {
        self.retry_on_invalid_shard_id(
            worker_id,
            worker_id,
            |worker_executor_client, worker_id| {
                Box::pin(async move {
                    let response = worker_executor_client
                        .interrupt_worker(InterruptWorkerRequest {
                            worker_id: Some(worker_id.clone().into()),
                            recover_immediately,
                        })
                        .await
                        .map_err(|err| {
                            GolemError::RuntimeError(GolemErrorRuntimeError {
                                details: err.to_string(),
                            })
                        })?;
                    match response.into_inner() {
                        workerexecutor::InterruptWorkerResponse {
                            result: Some(workerexecutor::interrupt_worker_response::Result::Success(_)),
                        } => Ok(()),
                        workerexecutor::InterruptWorkerResponse {
                            result: Some(workerexecutor::interrupt_worker_response::Result::Failure(err)),
                        } => Err(err.try_into().unwrap()),
                        workerexecutor::InterruptWorkerResponse { .. } => {
                            Err(GolemError::Unknown(GolemErrorUnknown {
                                details: "Empty response".to_string(),
                            }))
                        }
                    }
                })
            },
        )
            .await?;
        Ok(())
    }

    async fn get_metadata(&self, worker_id: &WorkerId) -> Result<WorkerMetadata, WorkerError> {
        let metadata = self.retry_on_invalid_shard_id(
            worker_id,
            worker_id,
            |worker_executor_client, worker_id| {
                Box::pin(async move {
                    let response = worker_executor_client.get_worker_metadata(
                        golem_api_grpc::proto::golem::worker::WorkerId::from(worker_id.clone())
                    ).await.map_err(|err| {
                        GolemError::RuntimeError(GolemErrorRuntimeError {
                            details: err.to_string(),
                        })
                    })?;
                    match response.into_inner() {
                        workerexecutor::GetWorkerMetadataResponse {
                            result:
                            Some(workerexecutor::get_worker_metadata_response::Result::Success(metadata)),
                        } => Ok(metadata.try_into().unwrap()),
                        workerexecutor::GetWorkerMetadataResponse {
                            result:
                            Some(workerexecutor::get_worker_metadata_response::Result::Failure(err)),
                        } => Err(err.try_into().unwrap()),
                        workerexecutor::GetWorkerMetadataResponse { .. } => {
                            Err(GolemError::Unknown(GolemErrorUnknown {
                                details: "Empty response".to_string(),
                            }))
                        }
                    }
                })
            },
        ).await?;
        Ok(metadata)
    }

    async fn resume(&self, worker_id: &WorkerId) -> Result<(), WorkerError> {
        self.retry_on_invalid_shard_id(
            worker_id,
            worker_id,
            |worker_executor_client, worker_id| {
                Box::pin(async move {
                    let response = worker_executor_client
                        .resume_worker(ResumeWorkerRequest {
                            worker_id: Some(worker_id.clone().into()),
                        })
                        .await
                        .map_err(|err| {
                            GolemError::RuntimeError(GolemErrorRuntimeError {
                                details: err.to_string(),
                            })
                        })?;
                    match response.into_inner() {
                        workerexecutor::ResumeWorkerResponse {
                            result: Some(workerexecutor::resume_worker_response::Result::Success(_)),
                        } => Ok(()),
                        workerexecutor::ResumeWorkerResponse {
                            result: Some(workerexecutor::resume_worker_response::Result::Failure(err)),
                        } => Err(err.try_into().unwrap()),
                        workerexecutor::ResumeWorkerResponse { .. } => {
                            Err(GolemError::Unknown(GolemErrorUnknown {
                                details: "Empty response".to_string(),
                            }))
                        }
                    }
                })
            },
        )
            .await?;
        Ok(())
    }
}

#[derive(Default)]
pub struct WorkerServiceNoOp {}

#[async_trait]
impl WorkerService for WorkerServiceNoOp {
    async fn get_by_id(&self, worker_id: &WorkerId) -> Result<VersionedWorkerId, WorkerError> {
        Ok(VersionedWorkerId {
            worker_id: worker_id.clone(),
            template_version_used: 0,
        })
    }

    async fn create(
        &self,
        worker_id: &WorkerId,
        _template_version: i32,
        _arguments: Vec<String>,
        _environment_variables: HashMap<String, String>,
    ) -> Result<VersionedWorkerId, WorkerError> {
        Ok(VersionedWorkerId {
            worker_id: worker_id.clone(),
            template_version_used: 0,
        })
    }

    async fn connect(&self, _worker_id: &WorkerId) -> Result<ConnectWorkerStream, WorkerError> {
        Err(WorkerError::Internal("Not supported".to_string()))
    }

    async fn delete(&self, _worker_id: &WorkerId) -> Result<(), WorkerError> {
        Ok(())
    }

    async fn get_invocation_key(
        &self,
        _worker_id: &WorkerId,
    ) -> Result<InvocationKey, WorkerError> {
        Ok(InvocationKey {
            value: "".to_string(),
        })
    }

    async fn invoke_and_await_function(
        &self,
        _worker_id: &WorkerId,
        _function_name: String,
        _invocation_key: &InvocationKey,
        _params: Value,
        _calling_convention: &CallingConvention,
    ) -> Result<Value, WorkerError> {
        Ok(Value::Null)
    }

    async fn invoke_and_await_function_proto(
        &self,
        _worker_id: &WorkerId,
        _function_name: String,
        _invocation_key: &InvocationKey,
        _params: Vec<ProtoVal>,
        _calling_convention: &CallingConvention,
    ) -> Result<ProtoInvokeResult, WorkerError> {
        Ok(ProtoInvokeResult { result: vec![] })
    }

    async fn invoke_function(
        &self,
        _worker_id: &WorkerId,
        _function_name: String,
        _params: Value,
    ) -> Result<(), WorkerError> {
        Ok(())
    }

    async fn invoke_fn_proto(
        &self,
        _worker_id: &WorkerId,
        _function_name: String,
        _params: Vec<ProtoVal>,
    ) -> Result<(), WorkerError> {
        Ok(())
    }

    async fn complete_promise(
        &self,
        _worker_id: &WorkerId,
        _oplog_id: i32,
        _data: Vec<u8>,
    ) -> Result<bool, WorkerError> {
        Ok(true)
    }

    async fn interrupt(
        &self,
        _worker_id: &WorkerId,
        _recover_immediately: bool,
    ) -> Result<(), WorkerError> {
        Ok(())
    }

    async fn get_metadata(&self, worker_id: &WorkerId) -> Result<WorkerMetadata, WorkerError> {
        Ok(WorkerMetadata {
            worker_id: worker_id.clone(),
            args: vec![],
            env: Default::default(),
            status: WorkerStatus::Running,
            template_version: 0,
            retry_count: 0,
        })
    }

    async fn resume(&self, _worker_id: &WorkerId) -> Result<(), WorkerError> {
        Ok(())
    }
}
