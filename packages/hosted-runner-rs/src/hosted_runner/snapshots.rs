use super::manifests::*;
use super::*;

pub(super) async fn write_snapshot_manifest(
    shared: &SharedRunner,
    input: &DrainRequest,
) -> HostedResult<(PathBuf, serde_json::Value)> {
    let session_file = shared.message_executor.flush_session()?;
    let replay_file = session_file
        .as_ref()
        .map(|session_file| session_file.with_extension("replay.json"))
        .filter(|replay_file| replay_file.is_file());
    let root = shared.config.snapshot_root.clone().unwrap_or_else(|| {
        shared
            .config
            .workspace_root
            .join(".maestro/runner-snapshots")
    });
    tokio::fs::create_dir_all(&root).await.map_err(|error| {
        HostedError::new(HostedRunnerErrorCode::RuntimeFailed, error.to_string())
    })?;
    let filename = format!(
        "{}-{}.json",
        safe_manifest_component(&shared.config.runner_session_id),
        Utc::now().timestamp_millis()
    );
    let path = root.join(filename);
    let (maestro_session_id, snapshot) = {
        let state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.session_id.clone(), shared.public_snapshot(&state))
    };
    let has_runtime_activity = snapshot.cursor > 0;
    let export_paths = input
        .export_paths
        .clone()
        .unwrap_or_else(|| vec![".".to_string()]);
    let mut workspace_export_paths = Vec::with_capacity(export_paths.len());
    for export_path in &export_paths {
        let resolved_path = resolve_workspace_path(
            &shared.config.workspace_root,
            None,
            Some(export_path.as_str()),
        )?;
        let metadata = tokio::fs::metadata(&resolved_path).await.ok();
        let path_type = metadata
            .as_ref()
            .map(|metadata| {
                if metadata.is_dir() {
                    "directory"
                } else if metadata.is_file() {
                    "file"
                } else {
                    "other"
                }
            })
            .unwrap_or("missing");
        let relative_path = resolved_path
            .strip_prefix(&shared.config.workspace_root)
            .ok()
            .and_then(|path| {
                if path.as_os_str().is_empty() {
                    Some(".".to_string())
                } else {
                    path.to_str().map(ToOwned::to_owned)
                }
            })
            .unwrap_or_else(|| export_path.clone());
        workspace_export_paths.push(WorkspaceExportPathManifest {
            input: export_path.clone(),
            path: resolved_path,
            relative_path,
            path_type: path_type.to_string(),
        });
    }
    if let Some(replay_file) = &replay_file {
        let canonical_workspace =
            dunce::canonicalize(&shared.config.workspace_root).map_err(|error| {
                HostedError::new(
                    HostedRunnerErrorCode::RuntimeFailed,
                    format!("failed to canonicalize workspace for replay sidecar: {error}"),
                )
            })?;
        let canonical_replay_file = dunce::canonicalize(replay_file).map_err(|error| {
            HostedError::new(
                HostedRunnerErrorCode::RuntimeFailed,
                format!("failed to canonicalize runtime replay sidecar: {error}"),
            )
        })?;
        let relative_path = canonical_replay_file
            .strip_prefix(&canonical_workspace)
            .map_err(|_| {
                HostedError::new(
                    HostedRunnerErrorCode::RuntimeFailed,
                    format!(
                        "runtime replay sidecar is outside the workspace: {} not under {}",
                        canonical_replay_file.display(),
                        canonical_workspace.display()
                    ),
                )
            })?
            .to_str()
            .ok_or_else(|| {
                HostedError::new(
                    HostedRunnerErrorCode::RuntimeFailed,
                    "runtime replay sidecar path is not valid UTF-8",
                )
            })?
            .to_owned();
        if !workspace_export_paths
            .iter()
            .any(|path| path.relative_path == relative_path)
        {
            workspace_export_paths.push(WorkspaceExportPathManifest {
                input: relative_path.clone(),
                path: replay_file.clone(),
                relative_path,
                path_type: "file".to_owned(),
            });
        }
    }
    let created_at = Utc::now().to_rfc3339();
    let runtime = RuntimeFlushManifest {
        flush_status: if has_runtime_activity {
            RuntimeFlushStatus::Completed
        } else {
            RuntimeFlushStatus::Skipped
        },
        error: None,
        session_id: maestro_session_id.clone(),
        session_file,
        replay_file,
        protocol_version: has_runtime_activity.then(|| HEADLESS_PROTOCOL_VERSION.to_string()),
        cursor: has_runtime_activity.then_some(snapshot.cursor),
    };
    let work_continuity = default_work_continuity_manifest(&snapshot);
    let retention_policy = default_retention_policy_manifest();
    let runtime_continuity = default_runtime_continuity_manifest(RuntimeContinuityManifestInput {
        config: &shared.config,
        maestro_session_id: &maestro_session_id,
        manifest_path: &path,
        runtime: &runtime,
    });
    let platform_evidence = default_platform_evidence_manifest(PlatformEvidenceManifestInput {
        config: &shared.config,
        maestro_session_id: &maestro_session_id,
        created_at: &created_at,
        manifest_path: &path,
        runtime: &runtime,
        runtime_continuity: &runtime_continuity,
        work_continuity: &work_continuity,
        retention_policy: &retention_policy,
        reason: input.reason.as_deref(),
        requested_by: input.requested_by.as_deref(),
    });
    let manifest = SnapshotManifest {
        protocol_version: HOSTED_RUNNER_SNAPSHOT_MANIFEST_VERSION.to_string(),
        runner_session_id: shared.config.runner_session_id.clone(),
        workspace_id: shared.config.workspace_id.clone(),
        agent_run_id: shared.config.agent_run_id.clone(),
        maestro_session_id: maestro_session_id.clone(),
        reason: input.reason.clone(),
        requested_by: input.requested_by.clone(),
        created_at,
        workspace_root: shared.config.workspace_root.clone(),
        runtime,
        workspace_export: WorkspaceExportManifest {
            mode: "local_path_contract".to_string(),
            paths: workspace_export_paths,
        },
        runtime_continuity: Some(runtime_continuity),
        work_continuity: Some(work_continuity),
        platform_evidence: Some(platform_evidence),
        snapshot,
        retention_policy: Some(retention_policy),
    };
    let body_bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| {
        HostedError::new(HostedRunnerErrorCode::RuntimeFailed, error.to_string())
    })?;
    let manifest = parse_snapshot_manifest_bytes(&body_bytes, &shared.config.workspace_root)
        .map_err(|error| HostedError::new(HostedRunnerErrorCode::RuntimeFailed, error.message))?;
    let body = serde_json::to_value(&manifest).map_err(|error| {
        HostedError::new(HostedRunnerErrorCode::RuntimeFailed, error.to_string())
    })?;
    tokio::fs::write(&path, body_bytes).await.map_err(|error| {
        HostedError::new(HostedRunnerErrorCode::RuntimeFailed, error.to_string())
    })?;
    Ok((path, body))
}

pub(super) fn parse_snapshot_manifest_bytes(
    bytes: &[u8],
    workspace_root: &Path,
) -> HostedResult<SnapshotManifest> {
    let manifest = serde_json::from_slice::<SnapshotManifest>(bytes).map_err(|error| {
        HostedError::new(
            HostedRunnerErrorCode::InvalidSnapshotManifest,
            format!("invalid snapshot manifest json: {error}"),
        )
    })?;
    let workspace_root = dunce::canonicalize(workspace_root).map_err(|error| {
        HostedError::new(
            HostedRunnerErrorCode::InvalidSnapshotManifest,
            format!("invalid restore workspace root: {error}"),
        )
    })?;
    manifest.validate_for_workspace(&workspace_root)?;
    Ok(manifest)
}

pub(super) async fn load_restore_manifest(
    config: &HostedRunnerConfig,
) -> io::Result<Option<SnapshotManifest>> {
    let Some(path) = &config.restore_manifest_path else {
        return Ok(None);
    };
    let path = if path.is_absolute() {
        path.clone()
    } else {
        config.workspace_root.join(path)
    };
    let bytes = tokio::fs::read(&path).await.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to read hosted runner restore manifest: {error}"),
        )
    })?;
    parse_snapshot_manifest_bytes(&bytes, &config.workspace_root)
        .and_then(|manifest| {
            manifest.validate_for_restore(config)?;
            Ok(manifest)
        })
        .map(Some)
        .map_err(hosted_error_to_io)
}

pub(super) fn hosted_error_to_io(error: HostedError) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {}", error.code.as_str(), error.message),
    )
}

fn safe_manifest_component(value: &str) -> String {
    let component = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if component.is_empty() {
        "runner".to_string()
    } else {
        component
    }
}
