// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use ::rpc::forge::forge_server::Forge;
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Response};
use axum::{Extension, Json};
use carbide_api_core::{Api, AuthContext};
use carbide_uuid::machine::MachineId;
use carbide_uuid::machine_validation::MachineValidationId;
use http::{StatusCode, header};
use rpc::forge::{self as rpc};
use serde::{Deserialize, Serialize};
use tonic::Status;

use crate::Base;
use crate::machine::ValidationRun;

fn run_view(
    run: rpc::MachineValidationRun,
    items: &[rpc::MachineValidationRunItem],
) -> ValidationRun {
    let active = run.end_time.is_none();
    let running: Vec<_> = items
        .iter()
        .filter(|item| active && item.state == "Running")
        .collect();
    ValidationRun {
        machine_id: run.machine_id.map(|id| id.to_string()).unwrap_or_default(),
        validation_id: run
            .validation_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        context: run.context.unwrap_or_default(),
        start_time: run
            .start_time
            .map(|time| time.to_string())
            .unwrap_or_default(),
        end_time: run
            .end_time
            .map(|time| time.to_string())
            .unwrap_or_else(|| "N/A".to_string()),
        status: run
            .status
            .as_ref()
            .and_then(|status| status.machine_validation_state.as_ref())
            .map(|state| format!("{state:?}"))
            .unwrap_or_else(|| "Unknown".to_string()),
        progress: run
            .status
            .as_ref()
            .map(|status| format!("{}/{}", status.completed_tests, status.total))
            .unwrap_or_default(),
        current_test: running
            .iter()
            .map(|item| item.test_id.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        current_attempt: running
            .iter()
            .map(|item| match &item.current_attempt_id {
                Some(id) => format!("{} ({})", item.attempt, id.value),
                None => item.attempt.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", "),
        active,
    }
}

async fn run_items(
    api: &Api,
    id: MachineValidationId,
) -> Result<Vec<rpc::MachineValidationRunItem>, Status> {
    Ok(
        db::machine_validation_execution::find_run_items_by_run_id(&api.database_connection, &id)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    )
}

pub(crate) async fn fetch_run_views(
    api: &Api,
    machine_id: Option<MachineId>,
) -> Result<Vec<ValidationRun>, Status> {
    let runs = api
        .get_machine_validation_runs(tonic::Request::new(
            rpc::MachineValidationRunListGetRequest {
                machine_id,
                include_history: false,
            },
        ))
        .await?
        .into_inner();
    let mut views = Vec::with_capacity(runs.runs.len());
    for run in runs.runs.into_iter().rev() {
        let items = match (run.end_time.is_none(), run.validation_id) {
            (true, Some(id)) => run_items(api, id).await?,
            _ => Vec::new(),
        };
        views.push(run_view(run, &items));
    }
    Ok(views)
}

pub(super) struct AttemptView {
    pub(super) id: String,
    pub(super) number: u32,
    pub(super) state: String,
}

pub(super) struct ItemView {
    pub(super) test_id: String,
    pub(super) state: String,
    pub(super) attempt: u32,
    pub(super) attempts: Vec<AttemptView>,
}

pub(super) struct RunPanel {
    pub(super) run: ValidationRun,
    pub(super) items: Vec<ItemView>,
}

pub(super) async fn load_panel(api: &Api, id: MachineValidationId) -> Result<RunPanel, Status> {
    let run = db::machine_validation::find_by_id(&api.database_connection, &id).await?;
    let items = run_items(api, id).await?;
    let view = run_view(run.into(), &items);
    let mut views = Vec::with_capacity(items.len());
    for item in items {
        let attempts = api
            .find_machine_validation_attempts(tonic::Request::new(
                rpc::MachineValidationAttemptSearchFilter {
                    run_item_id: item.run_item_id,
                },
            ))
            .await?
            .into_inner()
            .attempts;
        views.push(ItemView {
            test_id: item.test_id,
            state: item.state,
            attempt: item.attempt,
            attempts: attempts
                .into_iter()
                .map(|attempt| AttemptView {
                    id: attempt.attempt_id.map(|id| id.value).unwrap_or_default(),
                    number: attempt.attempt_number,
                    state: attempt.state,
                })
                .collect(),
        });
    }
    Ok(RunPanel {
        run: view,
        items: views,
    })
}

#[derive(Template)]
#[template(path = "validation_run_live.html")]
struct Snapshot {
    panel: RunPanel,
}

pub(crate) async fn snapshot(
    State(api): State<Arc<Api>>,
    Path(id): Path<MachineValidationId>,
) -> Response {
    match load_panel(&api, id).await {
        Ok(panel) => match (Snapshot { panel }).render() {
            Ok(html) => ([(header::CACHE_CONTROL, "no-store")], Html(html)).into_response(),
            Err(error) => {
                tracing::error!(%error, "Failed to render live validation run");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        },
        Err(status) => error_response(status),
    }
}

#[derive(Template)]
#[template(path = "validation_attempt_logs.html")]
struct AttemptLogs {
    attempt_id: uuid::Uuid,
}
impl Base for AttemptLogs {}

pub(crate) async fn attempt_logs(Path(attempt_id): Path<uuid::Uuid>) -> Response {
    match (AttemptLogs { attempt_id }).render() {
        Ok(html) => ([(header::CACHE_CONTROL, "no-store")], Html(html)).into_response(),
        Err(error) => {
            tracing::error!(%error, "Failed to render validation attempt logs");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct LogQuery {
    #[serde(default)]
    after_sequence: u32,
}

#[derive(Serialize)]
struct LogChunk {
    sequence: u32,
    stream: &'static str,
    content: String,
}

#[derive(Serialize)]
struct LogPage {
    enabled: bool,
    active: bool,
    has_more: bool,
    chunks: Vec<LogChunk>,
}

pub(crate) async fn attempt_logs_json(
    State(api): State<Arc<Api>>,
    Path(attempt_id): Path<uuid::Uuid>,
    Query(query): Query<LogQuery>,
    context: Option<Extension<AuthContext>>,
) -> Response {
    let make_request = || {
        let mut request = tonic::Request::new(rpc::MachineValidationAttemptLogGetRequest {
            attempt_id: Some(::rpc::common::Uuid {
                value: attempt_id.to_string(),
            }),
            after_sequence: query.after_sequence,
            limit: 100,
        });
        if let Some(Extension(context)) = &context {
            request.extensions_mut().insert(context.clone());
        }
        request
    };
    let load = async {
        // Use Core's authenticated, ownership-checked log API. A missing web
        // identity must never be replaced with a synthetic administrator here.
        let mut page = api
            .get_machine_validation_attempt_logs(make_request())
            .await?
            .into_inner();
        let enabled = api
            .runtime_config
            .machine_validation_config
            .attempt_logs
            .enabled;
        let active = if enabled {
            let attempt = api
                .get_machine_validation_attempt(tonic::Request::new(
                    rpc::MachineValidationAttemptGetRequest {
                        attempt_id: Some(::rpc::common::Uuid {
                            value: attempt_id.to_string(),
                        }),
                    },
                ))
                .await?
                .into_inner();
            matches!(attempt.state.as_str(), "Pending" | "Running")
        } else {
            false
        };
        if enabled && !active {
            // The final state is written after the output queue is drained.
            // Re-read after observing it so the browser receives the last chunks.
            page = api
                .get_machine_validation_attempt_logs(make_request())
                .await?
                .into_inner();
        }
        Ok::<_, Status>(LogPage {
            enabled,
            active,
            has_more: page.has_more,
            chunks: page
                .chunks
                .into_iter()
                .map(|chunk| LogChunk {
                    sequence: chunk.sequence,
                    stream: match rpc::MachineValidationAttemptLogStream::try_from(chunk.stream) {
                        Ok(rpc::MachineValidationAttemptLogStream::Stdout) => "stdout",
                        Ok(rpc::MachineValidationAttemptLogStream::Stderr) => "stderr",
                        _ => "unknown",
                    },
                    content: chunk.content,
                })
                .collect(),
        })
    }
    .await;
    match load {
        Ok(page) => ([(header::CACHE_CONTROL, "no-store")], Json(page)).into_response(),
        Err(status) => error_response(status),
    }
}

pub(super) fn error_response(status: Status) -> Response {
    let code = match status.code() {
        tonic::Code::InvalidArgument => StatusCode::BAD_REQUEST,
        tonic::Code::NotFound => StatusCode::NOT_FOUND,
        tonic::Code::Unauthenticated => StatusCode::UNAUTHORIZED,
        tonic::Code::PermissionDenied => StatusCode::FORBIDDEN,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    tracing::warn!(error = %status, "Failed to load machine validation view");
    (
        code,
        [(header::CACHE_CONTROL, "no-store")],
        "Unable to load validation data",
    )
        .into_response()
}
