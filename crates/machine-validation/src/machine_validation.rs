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
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::Path;

use carbide_uuid::machine::MachineId;
use chrono::Utc;
use forge_tls::client_config::ClientCert;
use rpc::forge_tls_client;
use rpc::forge_tls_client::{ApiConfig, ForgeClientConfig};
use serde::{Deserialize, Serialize};
use tracing::{error, info, trace, warn};
use utils::cmd::TokioCmd;

use crate::{
    IMAGE_LIST_FILE, MACHINE_VALIDATION_IMAGE_FILE, MACHINE_VALIDATION_IMAGE_PATH,
    MACHINE_VALIDATION_RUNNER_BASE_PATH, MACHINE_VALIDATION_RUNNER_TAG, MACHINE_VALIDATION_SERVER,
    MachineValidation, MachineValidationError, MachineValidationManager, MachineValidationRunParams,
    SCHME,
};
pub const MAX_STRING_STD_SIZE: usize = 1024 * 1024; // 1MB in bytes;
pub const DEFAULT_TIMEOUT: u64 = 3600;

/// Split `args` the way a shell would for word boundaries, without invoking a shell.
fn split_test_args(args: &str) -> Vec<String> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        Vec::new()
    } else {
        shlex::split(trimmed).unwrap_or_else(|| vec![args.to_string()])
    }
}

/// Extra `ctr run` flags from DB must be individual long options only (`--a` or `--a=b`), no shell metacharacters.
fn parse_container_arg_tokens(container_arg: &str) -> Result<Vec<String>, String> {
    let s = container_arg.trim();
    if s.is_empty() {
        return Ok(vec![]);
    }
    if s.chars().any(|ch| {
        matches!(
            ch,
            ';' | '|' | '&' | '#' | '`' | '$' | '\n' | '\r' | '"' | '\'' | '(' | ')'
        )
    }) {
        return Err(
            "container_arg contains disallowed characters (shell/runtime injection risk)"
                .to_string(),
        );
    }
    let tokens: Vec<&str> = s.split_whitespace().collect();
    for tok in &tokens {
        let body = tok.strip_prefix("--").ok_or_else(|| {
            "each container_arg token must be one ctr long flag (--name or --name=value)"
                .to_string()
        })?;
        if body.is_empty() {
            return Err("empty flag in container_arg".to_string());
        }
        if let Some((name, val)) = body.split_once('=') {
            if name.is_empty() || val.is_empty() {
                return Err("invalid --name=value in container_arg".to_string());
            }
            if !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Err("invalid flag name in container_arg".to_string());
            }
        } else if !body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err("invalid flag in container_arg".to_string());
        }
    }
    Ok(tokens.iter().map(|t| (*t).to_string()).collect())
}

fn validate_image_ref(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("img_name is empty".to_string());
    }
    if name.chars().any(|ch| {
        ch.is_control()
            || matches!(
                ch,
                ';' | '|' | '&' | '#' | '`' | '$' | '"' | '\'' | ' ' | '\t'
            )
    }) {
        return Err("img_name contains disallowed characters".to_string());
    }
    Ok(())
}

/// Interpreter basenames we refuse as the test program (blocks `bash -c`, `sh -c`, `env bash -c`, …).
const DENIED_PROGRAM_BASENAMES: &[&str] =
    &["sh", "bash", "dash", "zsh", "ksh", "csh", "tcsh", "fish"];

fn program_basename(program: &str) -> String {
    Path::new(program.trim())
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_else(|| program.trim())
        .to_ascii_lowercase()
}

fn is_denied_shell_basename(name: &str) -> bool {
    DENIED_PROGRAM_BASENAMES.contains(&name)
}

/// Rejects empty command, direct shell interpreters, and `env <shell> …` (common `-c` injection bypass).
fn validate_test_invocation(command: &str, args: &[String]) -> Result<(), String> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err("command is empty".to_string());
    }
    let base = program_basename(trimmed);
    if is_denied_shell_basename(&base) {
        return Err(
            "command cannot be a shell interpreter (sh/bash/…); use a direct binary and argv"
                .to_string(),
        );
    }
    if base == "env" && let Some(first) = args.first() {
        let b = program_basename(first);
        if is_denied_shell_basename(&b) {
            return Err(
                "cannot invoke a shell via env; use a direct binary and argv".to_string(),
            );
        }
    }
    Ok(())
}

fn validation_timeout_secs(timeout: Option<i64>) -> u64 {
    const DEFAULT: u64 = 7200;
    const MAX: u64 = 86400 * 14; // 14 days cap
    let Some(t) = timeout else {
        return DEFAULT;
    };
    if t <= 0 {
        return DEFAULT;
    }
    match u64::try_from(t) {
        Ok(secs) => secs.min(MAX),
        Err(_) => DEFAULT,
    }
}

impl MachineValidation {
    pub(crate) async fn get_container_auth_config(self) -> Result<(), MachineValidationError> {
        let file_name = "/root/.docker/config.json".to_string();
        match self
            .get_external_config(file_name.clone(), Some("container_auth".to_string()))
            .await
        {
            Ok(()) => trace!("Fetched {} config", file_name),
            Err(e) => trace!("Error - {}", e.to_string()),
        }
        Ok(())
    }
    pub(crate) async fn get_external_config(
        self,
        external_config_file: String,
        external_config_name: Option<String>,
    ) -> Result<(), MachineValidationError> {
        tracing::info!("{}", external_config_file);

        let name = if let Some(name) = external_config_name {
            name
        } else {
            let path = Path::new(&external_config_file);
            path.file_name().unwrap().to_str().unwrap().to_string()
        };

        let mut client = self.create_forge_client().await?;

        let request =
            tonic::Request::new(rpc::forge::GetMachineValidationExternalConfigRequest { name });
        let response = match client.get_machine_validation_external_config(request).await {
            Ok(res) => res,
            Err(e) => {
                return Err(MachineValidationError::ApiClient(
                    "get_external_config".to_owned(),
                    e.to_string(),
                ));
            }
        };
        let config = response.into_inner().config.unwrap().config;
        let mut file = File::create(external_config_file.clone()).map_err(|e| {
            MachineValidationError::File(external_config_file.clone(), e.to_string())
        })?;
        let s = String::from_utf8(config)
            .map_err(|e| MachineValidationError::Generic(e.to_string()))?;
        file.write_all(s.as_bytes()).map_err(|e| {
            MachineValidationError::File(external_config_file.clone(), e.to_string())
        })?;
        Ok(())
    }
    pub(crate) async fn create_forge_client(
        self,
    ) -> Result<forge_tls_client::ForgeClientT, MachineValidationError> {
        let client_config = ForgeClientConfig::new(
            self.options.root_ca,
            Some(ClientCert {
                cert_path: self.options.client_cert,
                key_path: self.options.client_key,
            }),
        );
        let api_config = ApiConfig::new(&self.options.api, &client_config);

        let client = forge_tls_client::ForgeTlsClient::retry_build(&api_config)
            .await
            .map_err(|err| MachineValidationError::Generic(err.to_string()))?;
        Ok(client)
    }

    pub(crate) async fn persist(
        self,
        data: Option<rpc::forge::MachineValidationResult>,
    ) -> Result<(), MachineValidationError> {
        tracing::info!("{}", data.clone().unwrap().name);
        let mut client = self.create_forge_client().await?;
        let request =
            tonic::Request::new(rpc::forge::MachineValidationResultPostRequest { result: data });
        client
            .persist_validation_result(request)
            .await
            .map_err(|e| {
                MachineValidationError::ApiClient(
                    "persist_validation_result".to_owned(),
                    e.to_string(),
                )
            })?;
        Ok(())
    }

    pub(crate) async fn get_machine_validation_tests(
        self,
        test_request: rpc::forge::MachineValidationTestsGetRequest,
    ) -> Result<Vec<rpc::forge::MachineValidationTest>, MachineValidationError> {
        tracing::info!("{:?}", test_request);
        let mut client = self.create_forge_client().await?;
        let request = tonic::Request::new(test_request);
        let response = client
            .get_machine_validation_tests(request)
            .await
            .map_err(|e| {
                MachineValidationError::ApiClient(
                    "get_machine_validation_tests".to_owned(),
                    e.to_string(),
                )
            })?
            .into_inner();

        Ok(response.tests)
    }

    pub async fn get_container_images() -> Result<(), MachineValidationError> {
        let url: String = format!(
            "{}://{}{}{}",
            SCHME, MACHINE_VALIDATION_SERVER, MACHINE_VALIDATION_IMAGE_PATH, "list.json"
        );
        tracing::info!(url);
        MachineValidationManager::download_file(&url, IMAGE_LIST_FILE).await?;

        let json_file_path = Path::new("/tmp/list.json");
        let reader = BufReader::new(File::open(json_file_path).map_err(|e| {
            MachineValidationError::File(
                format!(
                    "File {} open error",
                    json_file_path.to_str().unwrap_or_default()
                ),
                e.to_string(),
            )
        })?);

        #[derive(Debug, Serialize, Deserialize)]
        struct ImageList {
            images: Vec<String>,
        }

        let list: ImageList = serde_json::from_reader(reader)
            .map_err(|e| MachineValidationError::Generic(format!("Json read error: {e}")))?;
        for image_name in list.images {
            match Self::import_container(&image_name, MACHINE_VALIDATION_RUNNER_TAG).await {
                Ok(data) => {
                    trace!("Import successfull '{}'", data)
                }
                Err(e) => error!("Failed to import '{}'", e.to_string()),
            };
        }
        Ok(())
    }

    pub async fn import_container(
        image_name: &str,
        image_tag: &str,
    ) -> Result<String, MachineValidationError> {
        tracing::info!(image_name);
        let url: String = format!(
            "{SCHME}://{MACHINE_VALIDATION_SERVER}{MACHINE_VALIDATION_IMAGE_PATH}{image_name}.tar"
        );
        tracing::info!(url);
        MachineValidationManager::download_file(&url, MACHINE_VALIDATION_IMAGE_FILE).await?;

        info!(
            "Executing ctr images import {}",
            MACHINE_VALIDATION_IMAGE_FILE
        );
        TokioCmd::new("ctr")
            .args(["images", "import", MACHINE_VALIDATION_IMAGE_FILE])
            .timeout(DEFAULT_TIMEOUT)
            .output_with_timeout()
            .await
            .map_err(|e| MachineValidationError::Generic(e.to_string()))?;
        Ok(format!(
            "{MACHINE_VALIDATION_RUNNER_BASE_PATH}{image_name}:{image_tag}"
        ))
    }

    pub async fn pull_container(image_name: &str) {
        tracing::info!(image_name);
        match TokioCmd::new("nerdctl")
            .args(["-n", "default", "pull", image_name])
            .timeout(DEFAULT_TIMEOUT)
            .output_with_timeout()
            .await
        {
            Ok(result) => info!("pulled: {}", result.stdout),
            Err(e) => error!("Failed to image pull '{}' {}", image_name, e),
        }
    }
    async fn execute_machinevalidation_command(
        self,
        machine_id: &MachineId,
        test: &rpc::forge::MachineValidationTest,
        in_context: String,
        uuid: rpc::common::Uuid,
    ) -> Option<rpc::forge::MachineValidationResult> {
        let mut mc_result = rpc::forge::MachineValidationResult {
            test_id: Some(test.test_id.clone()),
            name: test.name.clone(),
            description: test.description.clone().unwrap_or_default(),
            command: test.command.clone(),
            args: test.args.clone(),
            context: in_context.clone(),
            validation_id: Some(uuid.clone()),
            ..rpc::forge::MachineValidationResult::default()
        };
        if test.external_config_file.is_some() {
            let file_name = test.external_config_file.clone().unwrap_or_default();
            match self.get_external_config(file_name.clone(), None).await {
                Ok(()) => trace!("Fetched {} config", file_name),
                Err(e) => {
                    mc_result.start_time = Some(Utc::now().into());
                    mc_result.end_time = Some(Utc::now().into());
                    mc_result.std_err = format!("Error {e}");
                    mc_result.std_out = format!("Skipped: Error {e}");
                    mc_result.exit_code = 0;
                    return Some(mc_result);
                }
            }
        }

        // Check pre_condition
        if test.pre_condition.is_some() {
            let pre = test.pre_condition.clone().unwrap_or("/bin/true".to_owned());
            let pre_trim = pre.trim();
            if pre_trim.is_empty() {
                mc_result.start_time = Some(Utc::now().into());
                mc_result.end_time = Some(Utc::now().into());
                mc_result.std_err = "pre_condition is empty".to_owned();
                mc_result.std_out = "Skipped : Pre condition failed".to_owned();
                mc_result.exit_code = 0;
                return Some(mc_result);
            }
            if is_denied_shell_basename(&program_basename(pre_trim)) {
                mc_result.start_time = Some(Utc::now().into());
                mc_result.end_time = Some(Utc::now().into());
                mc_result.std_err = "pre_condition cannot be a shell interpreter".to_owned();
                mc_result.std_out = "Skipped : Pre condition failed".to_owned();
                mc_result.exit_code = 0;
                return Some(mc_result);
            }
            match TokioCmd::new(pre)
                .timeout(DEFAULT_TIMEOUT)
                .env("CONTEXT".to_owned(), in_context.clone())
                .env("MACHINE_VALIDATION_RUN_ID".to_owned(), uuid.to_string())
                .env("MACHINE_ID".to_owned(), machine_id.to_string())
                .output_with_timeout()
                .await
            {
                Ok(result) => {
                    let exit_code = result.exit_code;
                    if exit_code != 0 {
                        mc_result.start_time = Some(result.start_time.into());
                        mc_result.end_time = Some(result.end_time.into());
                        mc_result.std_err = result.stderr;
                        mc_result.std_out = "Skipped : Pre condition failed".to_owned();
                        mc_result.exit_code = 0;
                        return Some(mc_result);
                    }
                }
                Err(e) => {
                    mc_result.start_time = Some(Utc::now().into());
                    mc_result.end_time = Some(Utc::now().into());
                    mc_result.std_err = e.to_string();
                    mc_result.std_out = "Skipped : Pre condition failed".to_owned();
                    mc_result.exit_code = 0;
                    return Some(mc_result);
                }
            }
        }
        // Execute command without a shell (argv only).
        let inner_args = split_test_args(&test.args);
        if let Err(e) = validate_test_invocation(&test.command, &inner_args) {
            mc_result.start_time = Some(Utc::now().into());
            mc_result.end_time = Some(Utc::now().into());
            mc_result.std_err = e.clone();
            mc_result.std_out = format!("Skipped: invalid command: {e}");
            mc_result.exit_code = 0;
            return Some(mc_result);
        }
        let timeout_secs = validation_timeout_secs(test.timeout);

        let _ = std::fs::remove_file("/tmp/forge_env_variables");
        match File::create("/tmp/forge_env_variables") {
            Ok(mut file) => {
                let mut envs = HashMap::new();
                envs.insert("CONTEXT".to_owned(), in_context.clone());
                envs.insert("MACHINE_VALIDATION_RUN_ID".to_owned(), uuid.to_string());
                envs.insert("MACHINE_ID".to_owned(), machine_id.to_string());
                let env_vars = envs
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<String>>()
                    .join("\n");
                file.write_all(env_vars.as_bytes()).expect("write failed");
            }
            Err(_) => error!("Failed to create file"),
        }

        let cmd_output = if let Some(ref img) = test.img_name {
            if let Err(e) = validate_image_ref(img) {
                mc_result.start_time = Some(Utc::now().into());
                mc_result.end_time = Some(Utc::now().into());
                mc_result.std_err = e.clone();
                mc_result.std_out = format!("Skipped: invalid img_name: {e}");
                mc_result.exit_code = 0;
                return Some(mc_result);
            }
            let ctr_extra =
                match parse_container_arg_tokens(test.container_arg.as_deref().unwrap_or("")) {
                    Ok(v) => v,
                    Err(e) => {
                        mc_result.start_time = Some(Utc::now().into());
                        mc_result.end_time = Some(Utc::now().into());
                        mc_result.std_err = e.clone();
                        mc_result.std_out = format!("Skipped: invalid container_arg: {e}");
                        mc_result.exit_code = 0;
                        return Some(mc_result);
                    }
                };
            Self::pull_container(img).await;
            let mut argv: Vec<String> = vec![
                "run".into(),
                "--rm".into(),
                "--privileged".into(),
                "--no-pivot".into(),
                "--mount".into(),
                "type=bind,src=/,dst=/host,options=rbind:rw".into(),
            ];
            argv.extend(ctr_extra);
            argv.push(img.clone());
            argv.push("runner".into());
            if test.execute_in_host.unwrap_or(false) {
                argv.push("chroot".into());
                argv.push("/host".into());
                argv.push(test.command.clone());
                argv.extend(inner_args);
            } else {
                argv.push(test.command.clone());
                argv.extend(inner_args);
            }
            info!("Executing ctr {}", argv.join(" "));
            TokioCmd::new("ctr")
                .args(argv)
                .timeout(timeout_secs)
                .env("CONTEXT".to_owned(), in_context.clone())
                .env("MACHINE_VALIDATION_RUN_ID".to_owned(), uuid.to_string())
                .env("MACHINE_ID".to_owned(), machine_id.to_string())
                .output_with_timeout()
                .await
        } else {
            info!("Executing {} {}", test.command, inner_args.join(" "));
            TokioCmd::new(&test.command)
                .args(inner_args)
                .timeout(timeout_secs)
                .env("CONTEXT".to_owned(), in_context.clone())
                .env("MACHINE_VALIDATION_RUN_ID".to_owned(), uuid.to_string())
                .env("MACHINE_ID".to_owned(), machine_id.to_string())
                .output_with_timeout()
                .await
        };

        match cmd_output {
            Ok(result) => {
                let mut stdout_str = result.stdout;
                let mut stderr_str = result.stderr;
                if test.extra_output_file.is_some() {
                    let message: String = match tokio::fs::read_to_string(
                        test.extra_output_file.clone().unwrap_or_default(),
                    )
                    .await
                    {
                        Ok(data) => data,
                        Err(_) => "".to_owned(),
                    };
                    stdout_str = stdout_str + &message;
                }
                if test.extra_err_file.is_some() {
                    let message: String = match tokio::fs::read_to_string(
                        test.extra_err_file.clone().unwrap_or_default(),
                    )
                    .await
                    {
                        Ok(data) => data,
                        Err(_) => "".to_owned(),
                    };
                    stderr_str = stderr_str + &message;
                }

                mc_result.start_time = Some(result.start_time.into());
                mc_result.end_time = Some(result.end_time.into());
                mc_result.std_err = if stderr_str.len() > MAX_STRING_STD_SIZE {
                    stderr_str[..MAX_STRING_STD_SIZE].to_string()
                } else {
                    stderr_str
                };
                mc_result.std_out = if stdout_str.len() > MAX_STRING_STD_SIZE {
                    stdout_str[..MAX_STRING_STD_SIZE].to_string()
                } else {
                    stdout_str
                };
                mc_result.exit_code = result.exit_code;
                Some(mc_result)
            }
            Err(e) => {
                mc_result.start_time = Some(Utc::now().into());
                mc_result.end_time = Some(Utc::now().into());
                mc_result.std_err = e.to_string();
                mc_result.std_out = e.to_string();
                mc_result.exit_code = -1;
                Some(mc_result)
            }
        }
    }

    pub(crate) async fn update_machine_validation_run(
        self,
        data: rpc::forge::MachineValidationRunRequest,
    ) -> Result<(), MachineValidationError> {
        tracing::info!("{:?}", data.clone());
        let mut client = self.create_forge_client().await?;
        let request = tonic::Request::new(data);
        let _response = client
            .update_machine_validation_run(request)
            .await
            .map_err(|e| {
                MachineValidationError::ApiClient(
                    "update_machine_validation_run".to_owned(),
                    e.to_string(),
                )
            })?;
        Ok(())
    }
    pub async fn run(
        self,
        machine_id: &MachineId,
        tests: Vec<rpc::forge::MachineValidationTest>,
        params: MachineValidationRunParams,
    ) -> Result<(), MachineValidationError> {
        let MachineValidationRunParams {
            context,
            uuid,
            execute_tests_sequentially,
            filter: machine_validation_filter,
            bypass_unverified_tests,
        } = params;

        self.clone().get_container_auth_config().await?;
        match Self::get_container_images().await {
            Ok(_) => info!("Successfully fetched container images"),
            Err(e) => error!("{}", e.to_string()),
        }
        if execute_tests_sequentially {
            for test in tests {
                if !bypass_unverified_tests && !test.verified {
                    warn!(
                        test_id = %test.test_id,
                        "Skipping unverified machine validation test (defense in depth)"
                    );
                    continue;
                }
                if !machine_validation_filter.allowed_tests.is_empty()
                    && !machine_validation_filter
                        .allowed_tests
                        .iter()
                        .any(|t| t.eq_ignore_ascii_case(&test.test_id))
                {
                    continue;
                }
                let result = self
                    .clone()
                    .execute_machinevalidation_command(
                        machine_id,
                        &test,
                        context.clone(),
                        rpc::common::Uuid {
                            value: uuid.clone(),
                        },
                    )
                    .await;
                match self.clone().persist(result).await {
                    Ok(_) => info!("Successfully sent to api server - {}", test.name),
                    Err(e) => error!("{}", e.to_string()),
                }
            }
        } else {
            info!("To be implemented");
        }
        Ok(())
    }
}
