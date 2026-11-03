/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: Apache-2.0
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 * http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use axum::body::Body;
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use hyper::http::StatusCode;
use model::machine::{MachineValidationContext, MachineValidationFilter};
use tower::ServiceExt;

use crate::tests::env::TestEnv;
use crate::tests::{make_test_app, web_request_builder};

#[crate::sqlx_test]
async fn validation_run_pages_display_optional_end_time(pool: sqlx::PgPool) {
    let env = TestEnv::new(pool).await;
    let app = make_test_app(&env.test_harness);
    let host = env.create_ready_managed_host(1).await.0;
    let machine_id = host.host.id;
    let mut txn = env.test_harness.db_txn().await;
    let run = db::machine_validation::create_new_run(
        txn.as_mut(),
        &machine_id.into(),
        MachineValidationContext::OnDemand,
        MachineValidationFilter::default(),
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();

    for (scenario, end_time, expected) in [
        ("unfinished", None, "N/A"),
        (
            "completed",
            Some("2026-10-09T13:00:00Z".parse::<DateTime<Utc>>().unwrap()),
            "2026-10-09T13:00:00Z",
        ),
        (
            "explicit epoch",
            Some(DateTime::UNIX_EPOCH),
            "1970-01-01T00:00:00Z",
        ),
    ] {
        let mut txn = env.test_harness.db_txn().await;
        sqlx::query("UPDATE machine_validation SET end_time = $1, state = $2 WHERE id = $3")
            .bind(end_time)
            .bind(if end_time.is_some() {
                "Success"
            } else {
                "Started"
            })
            .bind(run.id)
            .execute(txn.as_mut())
            .await
            .unwrap();
        txn.commit().await.unwrap();

        for route in [
            "/admin/machinevalidation".to_string(),
            format!("/admin/machine/{machine_id}"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    web_request_builder()
                        .uri(&route)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{scenario}: {route}");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let html = std::str::from_utf8(&body).unwrap();
            let row = html
                .split("<tr")
                .find(|row| row.contains(&run.id.to_string()))
                .expect("validation run row")
                .split("</tr>")
                .next()
                .unwrap();
            assert!(
                row.contains(&format!("data-field=\"end-time\">{expected}</td>")),
                "{scenario}: {route}: {row}"
            );
        }
    }
}

async fn live_fixture(
    harness: &carbide_test_harness::TestHarness,
    machine_id: carbide_uuid::machine::MachineId,
) -> (
    carbide_uuid::machine_validation::MachineValidationId,
    uuid::Uuid,
) {
    let mut txn = harness.db_txn().await;
    let run_id = carbide_uuid::machine_validation::MachineValidationId::new();
    sqlx::query("INSERT INTO machine_validation (id, machine_id, name, context, state, total, completed) VALUES ($1, $2, 'live-test', 'OnDemand', 'InProgress', 1, 0)")
        .bind(run_id).bind(machine_id).execute(txn.as_mut()).await.unwrap();
    // Host pages list the machine's current validation IDs, rather than
    // every historical row. Log-only fixtures do not need a machine row.
    sqlx::query("UPDATE machines SET on_demand_machine_validation_id = $1 WHERE id = $2")
        .bind(run_id)
        .bind(machine_id)
        .execute(txn.as_mut())
        .await
        .unwrap();
    let item_id = uuid::Uuid::new_v4();
    let attempt_id = uuid::Uuid::new_v4();
    let previous_attempt = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO machine_validation_run_items (id, run_id, test_id, display_name, context, state, order_index, attempt, max_attempts) VALUES ($1, $2, 'forge_live_test', 'live test', 'OnDemand', 'Running', 0, 2, 2)")
        .bind(item_id).bind(run_id).execute(txn.as_mut()).await.unwrap();
    sqlx::query("INSERT INTO machine_validation_attempts (id, run_item_id, attempt_number, state) VALUES ($1, $2, 1, 'Failed')")
        .bind(previous_attempt).bind(item_id).execute(txn.as_mut()).await.unwrap();
    sqlx::query("INSERT INTO machine_validation_attempts (id, run_item_id, attempt_number, state) VALUES ($1, $2, 2, 'Running')")
        .bind(attempt_id).bind(item_id).execute(txn.as_mut()).await.unwrap();
    txn.commit().await.unwrap();
    (run_id, attempt_id)
}

#[crate::sqlx_test]
async fn live_validation_pages_show_running_attempt_before_results(pool: sqlx::PgPool) {
    let env = TestEnv::new(pool).await;
    let host = env.create_ready_managed_host(1).await.0;
    let (id, attempt_id) = live_fixture(&env.test_harness, host.host.id.into()).await;
    let app = make_test_app(&env.test_harness);
    for route in [
        "/admin/machinevalidation".into(),
        format!("/admin/machine/{}", host.host.id),
        format!("/admin/machinevalidation/runs/{id}"),
        format!("/admin/machinevalidation/runs/{id}/live"),
    ] {
        let response = app
            .clone()
            .oneshot(
                web_request_builder()
                    .uri(&route)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{route}");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let html = std::str::from_utf8(&body).unwrap();
        for text in [
            "Completed/Total",
            "Current Test",
            "Current Attempt",
            "0/1",
            "forge_live_test",
            &attempt_id.to_string(),
        ] {
            assert!(html.contains(text), "{route}: missing {text}");
        }
        if route == format!("/admin/machinevalidation/runs/{id}") {
            assert!(html.contains("Terminal Results"));
            assert!(html.contains("Attempt 1 (Failed)"));
            assert!(html.contains("Attempt 2 (Running)"));
            assert!(html.contains(&format!("/attempts/{attempt_id}/logs")));
        }
    }
    let mut txn = env.test_harness.db_txn().await;
    sqlx::query("UPDATE machine_validation SET state = 'Success', completed = 1, end_time = NOW() WHERE id = $1")
        .bind(id).execute(txn.as_mut()).await.unwrap();
    txn.commit().await.unwrap();
    let response = app
        .oneshot(
            web_request_builder()
                .uri(format!("/admin/machinevalidation/runs/{id}/live"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let html = std::str::from_utf8(&body).unwrap();
    assert!(html.contains("1/1"));
    assert!(html.contains("data-active=\"false\""));
    assert!(html.contains("<dt>Current Test</dt><dd data-field=\"current-test\"></dd>"));
}

fn admin_context() -> carbide_api_core::AuthContext {
    let mut context = carbide_api_core::AuthContext::default();
    context
        .principals
        .push(carbide_authn::middleware::Principal::from_web_cookie(
            "test-admin".into(),
            "test-admins".into(),
        ));
    context
}

async fn log_harness(pool: sqlx::PgPool, enabled: bool) -> carbide_test_harness::TestHarness {
    let mut config = carbide_api_core::test_support::default_config::get();
    config.machine_validation_config.attempt_logs.enabled = enabled;
    carbide_test_harness::TestHarness::builder(pool)
        .with_api_builder_fn(move |builder| {
            builder.with_runtime_config(std::sync::Arc::new(config))
        })
        .build()
        .await
}

#[crate::sqlx_test]
async fn live_validation_logs_preserve_auth_cursor_and_final_output(pool: sqlx::PgPool) {
    let harness = log_harness(pool, true).await;
    let machine_id = "fm100ht038bg3qsho433vkg684heguv282qaggmrsh2ugn1qk096n2c6hcg"
        .parse()
        .unwrap();
    let (_, attempt_id) = live_fixture(&harness, machine_id).await;
    let app = make_test_app(&harness);
    let url = format!("/admin/machinevalidation/attempts/{attempt_id}/logs.json");
    let response = app
        .clone()
        .oneshot(web_request_builder().uri(&url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let mut other = carbide_api_core::AuthContext::default();
    other.principals.push(
        carbide_authn::middleware::Principal::SpiffeMachineIdentifier("another-machine".into()),
    );
    let response = app
        .clone()
        .oneshot(
            web_request_builder()
                .uri(&url)
                .extension(other)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let mut txn = harness.db_txn().await;
    sqlx::query("INSERT INTO machine_validation_attempt_logs (attempt_id, sequence, stream, content) VALUES ($1, 1, 'stdout', '<script>first</script>'), ($1, 2, 'stderr', 'second')")
        .bind(attempt_id).execute(txn.as_mut()).await.unwrap();
    txn.commit().await.unwrap();
    let response = app
        .clone()
        .oneshot(
            web_request_builder()
                .uri(&url)
                .extension(admin_context())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(page["active"], true);
    assert_eq!(page["chunks"][0]["stream"], "stdout");
    assert_eq!(page["chunks"][0]["content"], "<script>first</script>");
    assert_eq!(page["chunks"][1]["stream"], "stderr");
    let mut txn = harness.db_txn().await;
    sqlx::query("INSERT INTO machine_validation_attempt_logs (attempt_id, sequence, stream, content) VALUES ($1, 3, 'stdout', 'final')")
        .bind(attempt_id).execute(txn.as_mut()).await.unwrap();
    sqlx::query(
        "UPDATE machine_validation_attempts SET state = 'Success', ended_at = NOW() WHERE id = $1",
    )
    .bind(attempt_id)
    .execute(txn.as_mut())
    .await
    .unwrap();
    txn.commit().await.unwrap();
    let response = app
        .clone()
        .oneshot(
            web_request_builder()
                .uri(format!("{url}?after_sequence=2"))
                .extension(admin_context())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(page["active"], false);
    assert_eq!(page["chunks"].as_array().unwrap().len(), 1);
    assert_eq!(page["chunks"][0]["content"], "final");
    let response = app
        .oneshot(
            web_request_builder()
                .uri("/admin/machinevalidation/attempts/not-a-uuid/logs.json")
                .extension(admin_context())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[crate::sqlx_test]
async fn live_validation_logs_report_disabled_storage(pool: sqlx::PgPool) {
    let harness = log_harness(pool, false).await;
    let response = make_test_app(&harness)
        .oneshot(
            web_request_builder()
                .uri(format!(
                    "/admin/machinevalidation/attempts/{}/logs.json",
                    uuid::Uuid::new_v4()
                ))
                .extension(admin_context())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(page["enabled"], false);
    assert_eq!(page["active"], false);
    assert!(page["chunks"].as_array().unwrap().is_empty());
}
