//! Native `maestro connections` credential and subscription UX.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use fd_lock::RwLock as FileLock;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use serde::Serialize;

use crate::plugins::{ConnectionTypeDefinition, ConnectionTypeManifest, PluginRegistry};
use crate::service_connections::{
    ConnectionAuthKind, ConnectionBroker, ConnectionOwner, ConnectionPlacement,
    ConnectionSecretRef, ConnectionState, ConnectionStore, KeyringSecretBackend, SecretBackend,
    ServiceConnection, keyring_secret_ref, now_ms,
};

#[path = "connections_one_password.rs"]
mod one_password;

#[derive(Debug, Default)]
struct Args {
    command: Option<String>,
    positionals: Vec<String>,
    label: Option<String>,
    from_env: Option<String>,
    from_file: Option<PathBuf>,
    from_one_password: Option<String>,
    delegated_profile: Option<String>,
    secret_stdin: bool,
    default: bool,
    json: bool,
    local: bool,
    workspace: Option<PathBuf>,
    help: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionTypeReport {
    #[serde(flatten)]
    definition: ConnectionTypeDefinition,
    source: String,
}

pub fn run_connections(args: &[String]) -> Result<i32> {
    let parsed = parse_args(args)?;
    if parsed.help || parsed.command.as_deref() == Some("help") {
        print_help();
        return Ok(0);
    }
    if parsed.command.is_none()
        && !parsed.json
        && parsed.workspace.is_none()
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
    {
        return run_dashboard(None);
    }
    let command = parsed.command.as_deref().unwrap_or("list");
    match command {
        "ui" | "dashboard" => {
            if parsed.json {
                bail!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("deixic-code connections ui does not support --json", &[])
                )
            }
            if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "deixic-code connections ui requires an interactive terminal",
                        &[]
                    )
                )
            }
            run_dashboard(parsed.workspace.as_deref())
        }
        "1password" => {
            if parsed.json || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                bail!(
                    "{}",
                    crate::localization::cli_locale()
                        .translate("1Password capability setup requires an interactive terminal")
                );
            }
            one_password::run(parsed.workspace.as_deref()).map(|()| 0)
        }
        "types" => run_types(&parsed),
        "list" | "ls" => {
            if parsed.local {
                return run_list(parsed.json);
            }
            if let Some(session) = current_platform_session() {
                return run_list_platform(&session, parsed.json);
            }
            run_list(parsed.json)
        }
        "add"
            if parsed.positionals.is_empty()
                && !parsed.json
                && !has_explicit_add_options(&parsed)
                && io::stdin().is_terminal()
                && io::stdout().is_terminal() =>
        {
            run_add_wizard(parsed.workspace.as_deref()).map(|()| 0)
        }
        "add" => {
            if current_platform_session().is_some()
                && !matches!(
                    parsed.positionals.first().map(String::as_str),
                    Some(
                        "codex-subscription"
                            | "openai-codex"
                            | "claude-subscription"
                            | "copilot-subscription"
                    )
                )
            {
                return run_add_platform(&parsed);
            }
            run_add(&parsed)
        }
        "status" | "check" => {
            if !local_subscription_connection(&parsed)? {
                if let Some(session) = current_platform_session() {
                    return run_status_platform(&session, &parsed);
                }
            }
            run_status(&parsed)
        }
        "use" | "default" => {
            if !local_subscription_connection(&parsed)? && current_platform_session().is_some() {
                return run_use_platform(&parsed);
            }
            run_use(&parsed)
        }
        "rotate" => run_rotate(&parsed),
        "remove" | "rm" | "revoke" => {
            if !local_subscription_connection(&parsed)? {
                if let Some(session) = current_platform_session() {
                    return run_remove_platform(&session, &parsed);
                }
            }
            run_remove(&parsed)
        }
        other => bail!(
            "{}",
            crate::localization::cli_locale().format(
                "unknown connections subcommand: {0}",
                &[(other).to_string()]
            )
        ),
    }
}

fn has_explicit_add_options(args: &Args) -> bool {
    args.label.is_some()
        || args.default
        || args.from_env.is_some()
        || args.from_file.is_some()
        || args.from_one_password.is_some()
        || args.delegated_profile.is_some()
        || args.secret_stdin
}

fn local_subscription_connection(args: &Args) -> Result<bool> {
    local_subscription_connection_at(args, &ConnectionStore::default_path()?)
}

fn local_subscription_connection_at(args: &Args, path: &Path) -> Result<bool> {
    let Some(id) = args.positionals.first() else {
        return Ok(false);
    };
    // The local store is optional for Platform connections. Its corruption
    // must not prevent unrelated Platform status, use, or removal.
    Ok(ConnectionStore::load(path)
        .ok()
        .and_then(|store| {
            store.get(id).map(|connection| {
                matches!(
                    connection.type_id.as_str(),
                    "codex-subscription" | "claude-subscription" | "copilot-subscription"
                )
            })
        })
        .unwrap_or(false))
}

fn run_types(args: &Args) -> Result<i32> {
    let reports = connection_types(args.workspace.as_deref())?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
    } else {
        println!(
            "{}",
            crate::localization::cli_locale().format("Connection types", &[])
        );
        for report in reports {
            println!(
                "- {} — {} [{}; {:?}; source={}]",
                report.definition.id,
                localized_connection_type_name(&report, crate::localization::cli_locale()),
                report.definition.provider_id,
                report.definition.auth_kind,
                report.source
            );
        }
    }
    Ok(0)
}

fn run_list(json: bool) -> Result<i32> {
    let store = load_store()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&store.connections)?);
    } else if store.connections.is_empty() {
        println!("{}", crate::localization::cli_locale().format("No managed connections. Run `deixic-code connections types` to see available types.", &[]));
    } else {
        println!(
            "{}",
            crate::localization::cli_locale().format("Connections", &[])
        );
        for connection in store.connections {
            println!(
                "- {} — {} [{}; {:?}; generation={}; {}{}]",
                connection.id,
                connection.label,
                connection.provider_id,
                connection.state,
                connection.generation,
                if connection.is_default {
                    "default"
                } else {
                    "available"
                },
                if connection.owner.is_some() {
                    "; Deixic user bound"
                } else {
                    ""
                }
            );
        }
    }
    Ok(0)
}

fn run_add(args: &Args) -> Result<i32> {
    let type_id = required_position(
        args,
        0,
        crate::localization::cli_locale()
            .translate("Usage: deixic-code connections add <type> <id> [source]"),
    )?;
    let id = required_position(
        args,
        1,
        crate::localization::cli_locale()
            .translate("Usage: deixic-code connections add <type> <id> [source]"),
    )?;
    let definitions = connection_types(args.workspace.as_deref())?;
    let definition = resolve_connection_type(&definitions, type_id)?;
    let codex_profile = if definition.id == "codex-subscription" {
        let workspace = std::env::current_dir()?;
        let identity = maestro_local_host::codex_identity::resolve_codex_identity(
            args.delegated_profile.as_deref(),
            &workspace,
        )?;
        maestro_local_host::codex_subscription::check_chatgpt_profile(
            Some(&identity.profile_name),
            &workspace,
        )?;
        Some(identity.profile_name)
    } else {
        None
    };
    let owner = if matches!(
        definition.id.as_str(),
        "codex-subscription" | "claude-subscription" | "copilot-subscription"
    ) {
        let session = crate::credential_mode::verified_current_identity_session()?;
        Some(ConnectionOwner {
            organization_id: session.organization_id,
            user_id: session
                .user_id
                .context("verified Identity user is missing")?,
        })
    } else {
        None
    };
    let path = ConnectionStore::default_path()?;
    let backend = KeyringSecretBackend;
    let connection = with_locked_store(&path, |store| {
        if store.get(id).is_some() {
            bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("connection already exists: {0}", &[(id).to_string()])
            );
        }
        let (secret_ref, stored_key) =
            source_for_add(args, &definition, id, &backend, codex_profile.as_deref())?;
        let timestamp = now_ms();
        let provider_id = definition.provider_id.clone();
        let is_default = if args.from_one_password.is_some() {
            args.default
        } else {
            should_be_default(store, &provider_id, args.default)
        };
        let connection = ServiceConnection {
            id: id.to_owned(),
            type_id: definition.id.clone(),
            provider_id,
            label: args.label.clone().unwrap_or_else(|| id.to_owned()),
            auth_kind: definition.auth_kind,
            env_var: definition.env_var.clone(),
            secret_ref,
            placement: definition.placement,
            state: ConnectionState::Active,
            capabilities: definition.capabilities.clone(),
            mcp_binding: None,
            owner: owner.clone(),
            generation: 1,
            is_default,
            created_at_ms: timestamp,
            updated_at_ms: timestamp,
        };
        if let Err(error) = store
            .upsert(connection.clone())
            .and_then(|()| store.save(&path))
        {
            if let Some((service, account)) = stored_key {
                let _ = backend.delete(&service, &account);
            }
            return Err(error);
        }
        Ok(connection)
    })?;
    print_connection(&connection, args.json)?;
    Ok(0)
}

/// Keep company onboarding on the same verified user and provider-owned
/// profile as the Codex runtime. An existing owned connection is reusable.
pub(crate) fn ensure_codex_subscription_connection(profile: &str) -> Result<()> {
    let session = crate::credential_mode::verified_current_identity_session()?;
    let owner = ConnectionOwner {
        organization_id: session.organization_id,
        user_id: session
            .user_id
            .context("verified Identity user is missing")?,
    };
    let path = ConnectionStore::default_path()?;
    let store = ConnectionStore::load(&path)?;
    if let Some(connection) = owned_codex_connection(&store, &owner, profile) {
        let id = connection.id.clone();
        with_locked_store(&path, |store| {
            let current = store
                .get(&id)
                .context("Codex connection changed during setup")?;
            if current.owner.as_ref() != Some(&owner) || current.state != ConnectionState::Active {
                bail!("Codex connection changed during setup");
            }
            if !current.is_default {
                store.set_default(&id)?;
                store.save(&path)?;
            }
            Ok(())
        })?;
        println!("Using your existing Codex subscription connection: {id}");
        return Ok(());
    }
    let id = (1..=100)
        .map(|index| {
            if index == 1 {
                "codex-work".to_owned()
            } else {
                format!("codex-work-{index}")
            }
        })
        .find(|candidate| store.get(candidate).is_none())
        .context("No available local Codex connection ID")?;
    run_add(&Args {
        command: Some("add".to_owned()),
        positionals: vec!["codex-subscription".to_owned(), id.clone()],
        delegated_profile: Some(profile.to_owned()),
        default: true,
        ..Args::default()
    })?;
    let saved = ConnectionStore::load(&path)?;
    let connection = saved.get(&id).context("Codex connection was not saved")?;
    if connection.owner.as_ref() != Some(&owner) {
        bail!("Codex connection is not bound to the verified Deixic user");
    }
    Ok(())
}

pub(crate) fn ensure_cli_subscription_connection(provider: &str) -> Result<()> {
    if std::env::var("MAESTRO_CONNECTION")
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
    {
        bail!("Unset MAESTRO_CONNECTION before linking a personal subscription");
    }
    let (type_id, base_id) = match provider {
        "claude-code" => ("claude-subscription", "claude-work"),
        "github-copilot" => ("copilot-subscription", "copilot-work"),
        _ => bail!("unsupported subscription provider"),
    };
    let session = crate::credential_mode::verified_current_identity_session()?;
    let owner = ConnectionOwner {
        organization_id: session.organization_id,
        user_id: session
            .user_id
            .context("verified Identity user is missing")?,
    };
    let path = ConnectionStore::default_path()?;
    let store = ConnectionStore::load(&path)?;
    if let Some(existing) = store.connections.iter().find(|connection| {
        connection.provider_id == provider
            && connection.type_id == type_id
            && connection.state == ConnectionState::Active
            && connection.owner.as_ref() == Some(&owner)
    }) {
        let id = existing.id.clone();
        with_locked_store(&path, |store| {
            let current = store
                .get(&id)
                .context("Subscription connection changed during setup")?;
            if current.owner.as_ref() != Some(&owner) || current.state != ConnectionState::Active {
                bail!("Subscription connection changed during setup");
            }
            if !current.is_default {
                store.set_default(&id)?;
                store.save(&path)?;
            }
            Ok(())
        })?;
        println!("Using your existing {provider} subscription connection: {id}");
        return Ok(());
    }
    let id = (1..=100)
        .map(|index| {
            if index == 1 {
                base_id.to_owned()
            } else {
                format!("{base_id}-{index}")
            }
        })
        .find(|candidate| store.get(candidate).is_none())
        .context("No available local subscription connection ID")?;
    run_add(&Args {
        command: Some("add".to_owned()),
        positionals: vec![type_id.to_owned(), id.clone()],
        default: true,
        ..Args::default()
    })?;
    let saved = ConnectionStore::load(&path)?;
    let connection = saved
        .get(&id)
        .context("Subscription connection was not saved")?;
    if connection.owner.as_ref() != Some(&owner) {
        bail!("Subscription connection is not bound to the verified Deixic user");
    }
    Ok(())
}

fn owned_codex_connection<'a>(
    store: &'a ConnectionStore,
    owner: &ConnectionOwner,
    profile: &str,
) -> Option<&'a ServiceConnection> {
    store
        .connections
        .iter()
        .filter(|connection| {
            connection.provider_id == "openai-codex"
                && connection.type_id == "codex-subscription"
                && connection.state == ConnectionState::Active
                && connection.owner.as_ref() == Some(owner)
                && matches!(&connection.secret_ref, ConnectionSecretRef::Delegated { profile: saved, .. } if saved.as_deref().unwrap_or("default") == profile)
        })
        .max_by_key(|connection| (connection.is_default, connection.updated_at_ms))
}

fn should_be_default(store: &ConnectionStore, provider_id: &str, requested: bool) -> bool {
    requested
        || !store.connections.iter().any(|connection| {
            connection.provider_id == provider_id
                && connection.state == ConnectionState::Active
                && connection.is_default
        })
}

fn run_status(args: &Args) -> Result<i32> {
    let id = required_position(
        args,
        0,
        crate::localization::cli_locale().translate("Usage: deixic-code connections status <id>"),
    )?;
    let store = load_store()?;
    let connection = store
        .get(id)
        .with_context(|| format!("connection not found: {id}"))?;
    let broker = ConnectionBroker::new(store.clone(), KeyringSecretBackend);
    let env = std::env::vars().collect::<HashMap<_, _>>();
    let result = broker.check(id, &env);
    let status = if connection.state == ConnectionState::Revoked {
        "revoked"
    } else if result.is_err() {
        "unavailable"
    } else if connection.mcp_binding.is_some() {
        "managed_unverified"
    } else if connection.provider_id == "github-copilot" {
        // The CLI has no read-only account-status command. A model turn is the
        // first complete proof that its stored Copilot login can serve requests.
        "linked_unverified"
    } else if matches!(connection.secret_ref, ConnectionSecretRef::Delegated { .. }) {
        "ready_delegated"
    } else {
        "ready"
    };
    let managed_detail = (result.is_ok() && connection.mcp_binding.is_some())
        .then(|| crate::orb_connection::managed_mcp_health_detail(connection).ok())
        .flatten();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "id": id,
                "status": status,
                "providerId": connection.provider_id,
                "generation": connection.generation,
                "identityBound": connection.owner.is_some(),
                "detail": managed_detail.or_else(|| {
                    result.as_ref().err().map(std::string::ToString::to_string)
                }),
            }))?
        );
    } else {
        println!("{id}: {status}");
        if let Err(error) = result {
            eprintln!("{error}");
        }
    }
    Ok(i32::from(status == "unavailable" || status == "revoked"))
}

fn run_use(args: &Args) -> Result<i32> {
    let id = required_position(
        args,
        0,
        crate::localization::cli_locale().translate("Usage: deixic-code connections use <id>"),
    )?;
    let path = ConnectionStore::default_path()?;
    with_locked_store(&path, |store| {
        if let Some(owner) = store
            .get(id)
            .and_then(|connection| connection.owner.as_ref())
        {
            owner.require_current_user()?;
        }
        store.set_default(id)?;
        store.save(&path)
    })?;
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "{0}: default connection for its provider",
            &[(id).to_string()]
        )
    );
    Ok(0)
}

fn run_rotate(args: &Args) -> Result<i32> {
    let id = required_position(
        args,
        0,
        crate::localization::cli_locale()
            .translate("Usage: deixic-code connections rotate <id> [--secret-stdin]"),
    )?;
    let path = ConnectionStore::default_path()?;
    let backend = KeyringSecretBackend;
    let replacement = read_secret(args.secret_stdin)?;
    let generation = with_locked_store(&path, |store| {
        let (old_service, old_account, next_generation) = match &store
            .get(id)
            .with_context(|| format!("connection not found: {id}"))?
        {
            ServiceConnection {
                secret_ref: ConnectionSecretRef::Keyring { service, account },
                generation,
                ..
            } => (
                service.clone(),
                account.clone(),
                generation
                    .checked_add(1)
                    .context("connection generation overflow")?,
            ),
            _ => bail!("{}", crate::localization::cli_locale().format("only OS-credential-store connections can be rotated; update the referenced source instead", &[])),
        };
        let new_secret_ref = keyring_secret_ref(id, next_generation);
        let (new_service, new_account) = match &new_secret_ref {
            ConnectionSecretRef::Keyring { service, account } => (service.clone(), account.clone()),
            _ => unreachable!("keyring_secret_ref always returns a keyring reference"),
        };
        backend.set(&new_service, &new_account, &replacement)?;
        let connection = store
            .connections
            .iter_mut()
            .find(|connection| connection.id == id)
            .expect("connection checked above");
        connection.generation = next_generation;
        connection.secret_ref = new_secret_ref;
        connection.state = ConnectionState::Active;
        connection.updated_at_ms = now_ms();
        let generation = connection.generation;
        if let Err(error) = store.save(&path) {
            let _ = backend.delete(&new_service, &new_account);
            return Err(error);
        }
        if let Err(error) = backend.delete(&old_service, &old_account) {
            eprintln!("{}", crate::localization::cli_locale().format("warning: rotated connection is active, but the obsolete generation could not be deleted: {0}", &[(error).to_string()]));
        }
        Ok(generation)
    })?;
    println!("{id}: rotated; generation={generation}");
    Ok(0)
}

fn run_remove(args: &Args) -> Result<i32> {
    let id = required_position(
        args,
        0,
        crate::localization::cli_locale().translate("Usage: deixic-code connections remove <id>"),
    )?;
    let path = ConnectionStore::default_path()?;
    let backend = KeyringSecretBackend;
    remove_connection(&path, id, &backend)?;
    println!(
        "{}",
        crate::localization::cli_locale().format("{0}: removed and revoked", &[(id).to_string()])
    );
    Ok(0)
}

fn remove_connection(path: &Path, id: &str, backend: &impl SecretBackend) -> Result<()> {
    with_locked_store(path, |store| {
        if let Some(owner) = store
            .get(id)
            .and_then(|connection| connection.owner.as_ref())
        {
            owner.require_current_user()?;
        }
        let connection = store
            .remove(id)
            .with_context(|| format!("connection not found: {id}"))?;
        store.save(path)?;
        if let ConnectionSecretRef::Keyring { service, account } = &connection.secret_ref {
            if let Err(error) = backend.delete(service, account) {
                eprintln!("{}", crate::localization::cli_locale().format("warning: connection metadata was removed, but the obsolete credential could not be deleted: {0}", &[(error).to_string()]));
            }
        }
        Ok(())
    })
}

fn source_for_add(
    args: &Args,
    definition: &ConnectionTypeDefinition,
    id: &str,
    backend: &impl SecretBackend,
    codex_profile: Option<&str>,
) -> Result<(ConnectionSecretRef, Option<(String, String)>)> {
    if definition.placement == ConnectionPlacement::Platform {
        bail!("{}", crate::localization::cli_locale().format("platform-only connection types must be configured by the Platform credential broker", &[]));
    }
    let configured = usize::from(args.from_env.is_some())
        + usize::from(args.from_file.is_some())
        + usize::from(args.from_one_password.is_some())
        + usize::from(args.delegated_profile.is_some())
        + usize::from(args.secret_stdin);
    if configured > 1 {
        bail!(
            "{}",
            crate::localization::cli_locale().format("choose exactly one credential source", &[])
        );
    }
    if definition.auth_kind != ConnectionAuthKind::ApiKey {
        if configured > 0 && args.delegated_profile.is_none() {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "subscription and OAuth connection types use delegated authentication",
                    &[]
                )
            );
        }
        if !matches!(
            definition.provider_id.as_str(),
            "openai-codex" | "claude-code" | "github-copilot"
        ) {
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "no verified delegated authentication transport is available for provider {0}",
                    std::slice::from_ref(&(definition.provider_id))
                )
            );
        }
        if definition.provider_id != "openai-codex" && args.delegated_profile.is_some() {
            bail!(
                "Claude and Copilot CLI subscriptions use their signed-in default account, not a Maestro profile"
            );
        }
        return Ok((
            ConnectionSecretRef::Delegated {
                provider: definition.provider_id.clone(),
                profile: codex_profile
                    .map(str::to_owned)
                    .or_else(|| args.delegated_profile.clone()),
            },
            None,
        ));
    }
    if args.delegated_profile.is_some() {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "--delegated-profile is only valid for subscription and OAuth connection types",
                &[]
            )
        );
    }
    if let Some(name) = &args.from_env {
        return Ok((
            ConnectionSecretRef::Environment { name: name.clone() },
            None,
        ));
    }
    if let Some(path) = &args.from_file {
        let path = dunce::canonicalize(path).with_context(|| {
            format!(
                "failed to resolve connection source file {}",
                path.display()
            )
        })?;
        if !path.is_file() {
            bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("connection source file is not a regular file", &[])
            );
        }
        return Ok((ConnectionSecretRef::File { path }, None));
    }
    if let Some(reference) = &args.from_one_password {
        crate::ai::op_secret::validate_reference(reference)?;
        if definition.capabilities != ["models.invoke"]
            || crate::ai::ProviderRegistry::descriptor(&definition.provider_id)
                .and_then(|provider| provider.default_base_url)
                .is_none_or(|url| !url.starts_with("https://"))
        {
            bail!(
                "{}",
                crate::localization::cli_locale().translate(
                    "1Password supports native model capabilities with fixed HTTPS destinations"
                )
            );
        }
        return Ok((
            ConnectionSecretRef::OnePassword {
                reference: reference.clone(),
            },
            None,
        ));
    }
    let secret = read_secret(args.secret_stdin)?;
    let secret_ref = keyring_secret_ref(id, 1);
    let ConnectionSecretRef::Keyring { service, account } = &secret_ref else {
        unreachable!("keyring_secret_ref always returns a keyring reference");
    };
    backend.set(service, account, &secret)?;
    let stored_key = Some((service.clone(), account.clone()));
    Ok((secret_ref, stored_key))
}

fn read_secret(force_stdin: bool) -> Result<zeroize::Zeroizing<String>> {
    let value = if force_stdin || !io::stdin().is_terminal() {
        let mut value = String::new();
        io::stdin().read_to_string(&mut value)?;
        value
    } else {
        rpassword::prompt_password("Credential (stored in OS credential store): ")?
    };
    let value = value.trim().to_owned();
    if value.is_empty() {
        bail!(
            "{}",
            crate::localization::cli_locale().format("credential value must not be empty", &[])
        );
    }
    Ok(zeroize::Zeroizing::new(value))
}

fn load_store() -> Result<ConnectionStore> {
    ConnectionStore::load(&ConnectionStore::default_path()?)
}

fn with_locked_store<T>(
    path: &Path,
    operation: impl FnOnce(&mut ConnectionStore) -> Result<T>,
) -> Result<T> {
    let parent = path
        .parent()
        .context("connection store path has no parent directory")?;
    fs::create_dir_all(parent)?;
    let mut lock_name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    lock_name.push(".lock");
    let lock_path = path.with_file_name(lock_name);
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&lock_path).with_context(|| {
        format!(
            "failed to open connection store lock {}",
            lock_path.display()
        )
    })?;
    let mut lock = FileLock::new(file);
    let _guard = lock
        .write()
        .with_context(|| format!("failed to lock connection store {}", path.display()))?;
    let mut store = ConnectionStore::load(path)?;
    operation(&mut store)
}

fn print_connection(connection: &ServiceConnection, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(connection)?);
    } else {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "{0}: added {1} connection for {2} (generation={3})",
                &[
                    (connection.id).clone(),
                    (connection.type_id).clone(),
                    (connection.provider_id).clone(),
                    (connection.generation).to_string()
                ]
            )
        );
    }
    Ok(())
}

fn required_position<'a>(args: &'a Args, index: usize, usage: &str) -> Result<&'a str> {
    args.positionals
        .get(index)
        .map(String::as_str)
        .context(usage.to_owned())
}

fn resolve_connection_type(
    definitions: &[ConnectionTypeReport],
    type_id: &str,
) -> Result<ConnectionTypeDefinition> {
    let needle = type_id.trim();
    if needle.is_empty() {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "unknown or untrusted connection type: {0}",
                &[(type_id).to_string()]
            )
        );
    }
    if let Some(report) = definitions
        .iter()
        .find(|report| report.definition.id == needle)
    {
        return Ok(report.definition.clone());
    }
    let aliased = definitions
        .iter()
        .filter(|report| report.definition.provider_id == needle)
        .collect::<Vec<_>>();
    match aliased.as_slice() {
        [report] => Ok(report.definition.clone()),
        [] => bail!(
            "{}",
            crate::localization::cli_locale().format(
                "unknown or untrusted connection type: {0}",
                &[(needle).to_string()]
            )
        ),
        many => {
            let mut ids = many
                .iter()
                .map(|report| report.definition.id.as_str())
                .collect::<Vec<_>>();
            ids.sort_unstable();
            bail!(
                "{}",
                crate::localization::cli_locale().format(
                    "ambiguous connection type {0}; specify one of: {1}",
                    &[(needle).to_string(), (ids.join(", ")).clone()]
                )
            )
        }
    }
}

fn connection_types(workspace: Option<&Path>) -> Result<Vec<ConnectionTypeReport>> {
    let mut definitions = BTreeMap::<String, ConnectionTypeReport>::new();
    for definition in builtin_connection_types() {
        definitions.insert(
            definition.id.clone(),
            ConnectionTypeReport {
                definition,
                source: "maestro".to_owned(),
            },
        );
    }
    let registry = match workspace {
        Some(workspace) => PluginRegistry::discover_for_workspace(workspace),
        None => PluginRegistry::discover(),
    };
    for plugin in registry.plugins() {
        let Some(path) = &plugin.components.connections_path else {
            continue;
        };
        for definition in ConnectionTypeManifest::load(path)?.connection_types {
            if definition.provider_id == crate::orb_connection::HOSTED_ORB_PROVIDER_ID
                && definition.mcp_binding.is_some()
            {
                crate::orb_connection::validate_hosted_orb_definition(&definition)?;
            }
            if definitions.contains_key(&definition.id) {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format(
                        "plugin {0} connection type {1} conflicts with an existing type",
                        &[(plugin.name).clone(), (definition.id).clone()]
                    )
                );
            }
            definitions.insert(
                definition.id.clone(),
                ConnectionTypeReport {
                    definition,
                    source: format!("plugin:{}", plugin.name),
                },
            );
        }
    }
    Ok(definitions.into_values().collect())
}

fn localized_connection_type_name(
    report: &ConnectionTypeReport,
    locale: crate::localization::Locale,
) -> String {
    if report.source == "maestro" {
        if let Some(provider) = report.definition.display_name.strip_suffix(" API key") {
            return locale.format("{0} API key", &[provider.to_owned()]);
        }
        return locale.translate(&report.definition.display_name).to_owned();
    }
    report.definition.display_name.clone()
}

fn builtin_connection_types() -> Vec<ConnectionTypeDefinition> {
    let mut values = Vec::new();
    for (provider_id, display_name) in [
        ("anthropic", "Anthropic"),
        ("dashscope", "DashScope"),
        ("deepseek", "DeepSeek"),
        ("google", "Google Gemini"),
        ("groq", "Groq"),
        ("minimax", "MiniMax"),
        ("mistral", "Mistral"),
        ("moonshot", "Moonshot/Kimi"),
        ("openai", "OpenAI"),
        ("openrouter", "OpenRouter"),
        ("zai", "Z.ai"),
    ] {
        let Some(provider) = crate::ai::ProviderRegistry::descriptor(provider_id) else {
            continue;
        };
        let Some(env_var) = provider.auth_env.first() else {
            continue;
        };
        values.push(ConnectionTypeDefinition {
            id: format!("{provider_id}-api-key"),
            display_name: format!("{display_name} API key"),
            provider_id: provider.id.to_owned(),
            auth_kind: ConnectionAuthKind::ApiKey,
            placement: ConnectionPlacement::Either,
            env_var: Some((*env_var).to_owned()),
            capabilities: vec!["models.invoke".to_owned()],
            documentation_url: None,
            mcp_binding: None,
        });
    }
    values.push(ConnectionTypeDefinition {
        id: "codex-subscription".into(),
        display_name: "ChatGPT/Codex subscription".into(),
        provider_id: "openai-codex".into(),
        auth_kind: ConnectionAuthKind::Subscription,
        placement: ConnectionPlacement::Local,
        env_var: None,
        capabilities: vec!["models.invoke".into()],
        documentation_url: None,
        mcp_binding: None,
    });
    for (id, name, provider) in [
        (
            "claude-subscription",
            "Claude Code subscription",
            "claude-code",
        ),
        (
            "copilot-subscription",
            "GitHub Copilot subscription",
            "github-copilot",
        ),
    ] {
        values.push(ConnectionTypeDefinition {
            id: id.into(),
            display_name: name.into(),
            provider_id: provider.into(),
            auth_kind: ConnectionAuthKind::Subscription,
            placement: ConnectionPlacement::Local,
            env_var: None,
            capabilities: vec!["models.invoke".into()],
            documentation_url: None,
            mcp_binding: None,
        });
    }
    values.sort_by(|left, right| left.id.cmp(&right.id));
    values
}

fn parse_args(args: &[String]) -> Result<Args> {
    let mut parsed = Args::default();
    let mut index = 0;
    while index < args.len() {
        let value = &args[index];
        let take_value = |index: &mut usize| -> Result<String> {
            *index += 1;
            args.get(*index)
                .filter(|value| !value.starts_with('-'))
                .cloned()
                .with_context(|| format!("{} requires a value", args[*index - 1]))
        };
        match value.as_str() {
            "--json" => parsed.json = true,
            "--local" => parsed.local = true,
            "--default" => parsed.default = true,
            "--secret-stdin" => parsed.secret_stdin = true,
            "--help" | "-h" => parsed.help = true,
            "--label" => parsed.label = Some(take_value(&mut index)?),
            "--from-env" => parsed.from_env = Some(take_value(&mut index)?),
            "--from-file" => parsed.from_file = Some(PathBuf::from(take_value(&mut index)?)),
            "--from-1password" => parsed.from_one_password = Some(take_value(&mut index)?),
            "--delegated-profile" => parsed.delegated_profile = Some(take_value(&mut index)?),
            "--workspace" | "--cwd" => {
                parsed.workspace = Some(PathBuf::from(take_value(&mut index)?));
            }
            unknown if unknown.starts_with('-') => bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("unknown connections option: {0}", &[(unknown).to_string()])
            ),
            command if parsed.command.is_none() => parsed.command = Some(command.to_owned()),
            positional => parsed.positionals.push(positional.to_owned()),
        }
        index += 1;
    }
    Ok(parsed)
}

fn current_platform_session() -> Option<crate::credential_mode::PlatformSession> {
    let env = std::env::vars().collect();
    platform_session_from_env_or_snapshot(&env, || {
        crate::init_cli::load_evalops_snapshot().ok().flatten()
    })
}

fn platform_session_from_env_or_snapshot(
    env: &HashMap<String, String>,
    load_snapshot: impl FnOnce() -> Option<crate::init_cli::EvalOpsCredentialSnapshot>,
) -> Option<crate::credential_mode::PlatformSession> {
    // Hosted runners and managed local sessions already receive the complete
    // platform identity through environment variables. Resolve that source
    // before touching the OS credential store so read-only connection commands
    // cannot trigger a Keychain prompt they do not need.
    if let Some(session) = crate::credential_mode::platform_session_from(None, env) {
        return Some(session);
    }
    let snapshot = load_snapshot();
    crate::credential_mode::platform_session_from(snapshot.as_ref(), env)
}

fn run_list_platform(
    _session: &crate::credential_mode::PlatformSession,
    json: bool,
) -> Result<i32> {
    let store = crate::platform_provider_refs::load_default_store().unwrap_or_default();
    if json {
        println!("{}", serde_json::to_string_pretty(&store.refs)?);
    } else if store.refs.is_empty() {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "No org provider refs stored. Run `deixic-code connections add` to upload a key.",
                &[]
            )
        );
    } else {
        println!(
            "{}",
            crate::localization::cli_locale()
                .format("Org provider refs (secrets live in EvalOps keys)", &[])
        );
        for item in store.refs {
            println!(
                "- {} — {}/{}/{}{}",
                item.id,
                item.provider,
                item.environment,
                item.credential_name,
                if item.is_default { " [default]" } else { "" }
            );
        }
    }
    Ok(0)
}

fn run_add_platform(args: &Args) -> Result<i32> {
    let session = current_platform_session()
        .context("EvalOps session expired; run deixic-code evalops login")?;
    let type_id = required_position(
        args,
        0,
        crate::localization::cli_locale()
            .translate("Usage: deixic-code connections add <type> <id> [source]"),
    )?;
    let id = required_position(
        args,
        1,
        crate::localization::cli_locale()
            .translate("Usage: deixic-code connections add <type> <id> [source]"),
    )?;
    let definitions = connection_types(args.workspace.as_deref())?;
    let definition = resolve_connection_type(&definitions, type_id)?;
    if definition.auth_kind != crate::service_connections::ConnectionAuthKind::ApiKey {
        bail!(
            "{}",
            crate::localization::cli_locale().format(
                "Platform mode only uploads API-key connections to EvalOps keys",
                &[]
            )
        );
    }
    let secret = read_secret(args.secret_stdin)?;
    let stored = crate::platform_provider_refs::upsert_org_provider_ref(
        &session,
        crate::platform_provider_refs::UpsertRequest {
            id: id.to_owned(),
            provider: definition.provider_id.clone(),
            environment: crate::platform_provider_refs::default_environment(),
            credential_name: crate::platform_provider_refs::default_credential_name(id),
            team_id: None,
            api_key: secret.to_string(),
            make_default: args.default
                || crate::platform_provider_refs::load_default_store()
                    .ok()
                    .is_none_or(|store| {
                        !store
                            .refs
                            .iter()
                            .any(|item| item.provider == definition.provider_id && item.is_default)
                    }),
        },
    )?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&stored)?);
    } else {
        println!(
            "{}",
            crate::localization::cli_locale().format(
                "{0}: uploaded {1} provider ref to EvalOps keys (secret not stored locally)",
                &[(stored.id).clone(), (stored.provider).clone()]
            )
        );
    }
    Ok(0)
}

fn run_status_platform(
    session: &crate::credential_mode::PlatformSession,
    args: &Args,
) -> Result<i32> {
    let id = required_position(
        args,
        0,
        crate::localization::cli_locale().translate("Usage: deixic-code connections status <id>"),
    )?;
    let store = crate::platform_provider_refs::load_default_store()?;
    let stored = store
        .get(id)
        .cloned()
        .with_context(|| format!("provider ref not found: {id}"))?;
    match crate::platform_provider_refs::check_org_provider_ref(session, &stored) {
        Ok(_) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({ "id": id, "status": "ready", "placement": "platform" })
                );
            } else {
                println!(
                    "{}",
                    crate::localization::cli_locale()
                        .format("{0}: ready (org provider ref)", &[(id).to_string()])
                );
            }
            Ok(0)
        }
        Err(error) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "id": id,
                        "status": "unavailable",
                        "placement": "platform",
                        "detail": error.to_string()
                    })
                );
            } else {
                println!("{id}: unavailable");
                eprintln!("{error:#}");
            }
            Ok(1)
        }
    }
}

fn run_use_platform(args: &Args) -> Result<i32> {
    let id = required_position(
        args,
        0,
        crate::localization::cli_locale().translate("Usage: deixic-code connections use <id>"),
    )?;
    let selected = crate::platform_provider_refs::select_default(id)?;
    println!(
        "{}",
        crate::localization::cli_locale().format(
            "{0}: default org provider ref for {1}",
            &[(selected.id).clone(), (selected.provider).clone()]
        )
    );
    Ok(0)
}

fn run_remove_platform(
    session: &crate::credential_mode::PlatformSession,
    args: &Args,
) -> Result<i32> {
    let id = required_position(
        args,
        0,
        crate::localization::cli_locale().translate("Usage: deixic-code connections remove <id>"),
    )?;
    let store = crate::platform_provider_refs::load_default_store()?;
    let stored = store
        .get(id)
        .cloned()
        .with_context(|| format!("provider ref not found: {id}"))?;
    crate::platform_provider_refs::delete_org_provider_ref(session, &stored)?;
    println!(
        "{}",
        crate::localization::cli_locale()
            .format("{0}: revoked in EvalOps keys", &[(id).to_string()])
    );
    Ok(0)
}

fn print_help() {
    println!("{}", crate::localization::cli_locale().format("deixic-code connections <command> [options]\n\nCommands:\nui                              Open the interactive connection manager\ntypes [--json]                 List built-in and trusted-plugin connection types\nlist [--json] [--local]        List non-secret connection metadata\nadd [<type> <id>] [source]     Add an API key or delegated account\nType may be a type id (anthropic-api-key)\nor a provider id (anthropic)\nstatus <id> [--json]           Validate that the credential source is available\nuse <id>                       Select the provider's default connection\nrotate <id> [--secret-stdin]   Replace a keyring credential and revoke old leases\nremove <id>                    Delete metadata and keyring credential\n\nCredential sources for add:\n--from-env NAME                Resolve from an existing environment variable\n--from-file PATH               Resolve from an operator-owned file\n--from-1password op://...      Resolve with the 1Password CLI\n--secret-stdin                 Read a literal key from stdin into the OS credential store\n--delegated-profile NAME       Name a vendor-owned subscription/OAuth profile\n\nLiteral keys are never accepted as command-line arguments or written to connections.json.", &[]));
    println!(
        "\n1password                       {}",
        crate::localization::cli_locale().translate("Add a native 1Password capability")
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionHealth {
    Unknown,
    Ready,
    ReadyDelegated,
    ManagedUnverified,
    Unavailable,
    Revoked,
}

impl ConnectionHealth {
    fn label(self) -> &'static str {
        match self {
            Self::Unknown => crate::localization::cli_locale().translate("Not checked"),
            Self::Ready => crate::localization::cli_locale().translate("Ready"),
            Self::ReadyDelegated => {
                crate::localization::cli_locale().translate("Ready via sign-in")
            }
            Self::ManagedUnverified => {
                crate::localization::cli_locale().translate("Managed (remote health not probed)")
            }
            Self::Unavailable => crate::localization::cli_locale().translate("Unavailable"),
            Self::Revoked => crate::localization::cli_locale().translate("Revoked"),
        }
    }

    const fn color(self, theme: maestro_ui::UiTheme) -> Color {
        match self {
            Self::Unknown => theme.muted,
            Self::Ready | Self::ReadyDelegated => theme.success,
            Self::ManagedUnverified => theme.attention,
            Self::Unavailable | Self::Revoked => theme.error,
        }
    }
}

#[derive(Debug)]
struct DashboardState {
    store: ConnectionStore,
    selected: usize,
    list_state: ListState,
    health: BTreeMap<String, ConnectionHealth>,
    message: Option<String>,
    remove_confirmation: bool,
}

impl DashboardState {
    fn load() -> Result<Self> {
        let store = load_store()?;
        let mut state = Self {
            store,
            selected: 0,
            list_state: ListState::default(),
            health: BTreeMap::new(),
            message: None,
            remove_confirmation: false,
        };
        state.sync_selection();
        Ok(state)
    }

    fn selected_connection(&self) -> Option<&ServiceConnection> {
        self.store.connections.get(self.selected)
    }

    fn selected_id(&self) -> Option<String> {
        self.selected_connection()
            .map(|connection| connection.id.clone())
    }

    fn sync_selection(&mut self) {
        if self.store.connections.is_empty() {
            self.selected = 0;
            self.list_state.select(None);
        } else {
            self.selected = self.selected.min(self.store.connections.len() - 1);
            self.list_state.select(Some(self.selected));
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.store.connections.len();
        if len == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(len as isize) as usize;
        self.sync_selection();
        self.remove_confirmation = false;
    }

    fn refresh(&mut self) -> Result<()> {
        let selected_id = self.selected_id();
        self.store = load_store()?;
        self.selected = selected_id
            .as_deref()
            .and_then(|id| {
                self.store
                    .connections
                    .iter()
                    .position(|connection| connection.id == id)
            })
            .unwrap_or(0);
        self.health.retain(|id, _| self.store.get(id).is_some());
        self.remove_confirmation = false;
        self.sync_selection();
        self.message = Some(
            crate::localization::cli_locale()
                .translate("Connection list refreshed.")
                .to_owned(),
        );
        Ok(())
    }

    fn make_selected_default(&mut self) -> Result<()> {
        let Some(id) = self.selected_id() else {
            self.message = Some(
                crate::localization::cli_locale()
                    .translate("Select a connection before setting a default.")
                    .to_owned(),
            );
            return Ok(());
        };
        let path = ConnectionStore::default_path()?;
        with_locked_store(&path, |store| {
            store.set_default(&id)?;
            store.save(&path)
        })?;
        self.refresh()?;
        self.message = Some(crate::localization::cli_locale().format(
            "{0} is now the default for its provider.",
            std::slice::from_ref(&id),
        ));
        Ok(())
    }

    fn remove_selected(&mut self) -> Result<()> {
        let Some(id) = self.selected_id() else {
            return Ok(());
        };
        self.remove_confirmation = false;
        let path = ConnectionStore::default_path()?;
        remove_connection(&path, &id, &KeyringSecretBackend)?;
        self.refresh()?;
        self.message = Some(
            crate::localization::cli_locale().format("{0} was removed.", std::slice::from_ref(&id)),
        );
        Ok(())
    }
}

fn check_connection_health(store: &ConnectionStore, id: &str) -> ConnectionHealth {
    let Some(connection) = store.get(id) else {
        return ConnectionHealth::Unavailable;
    };
    if connection.state == ConnectionState::Revoked {
        return ConnectionHealth::Revoked;
    }
    let broker = ConnectionBroker::new(store.clone(), KeyringSecretBackend);
    let env = std::env::vars().collect::<HashMap<_, _>>();
    if broker.check(id, &env).is_err() {
        ConnectionHealth::Unavailable
    } else if connection.mcp_binding.is_some() {
        ConnectionHealth::ManagedUnverified
    } else if matches!(connection.secret_ref, ConnectionSecretRef::Delegated { .. }) {
        ConnectionHealth::ReadyDelegated
    } else {
        ConnectionHealth::Ready
    }
}

fn credential_source_label(connection: &ServiceConnection) -> String {
    match &connection.secret_ref {
        ConnectionSecretRef::Keyring { .. } => crate::localization::cli_locale()
            .translate("OS credential store")
            .to_owned(),
        ConnectionSecretRef::Environment { name } => crate::localization::cli_locale()
            .format("Environment variable: {0}", std::slice::from_ref(name)),
        ConnectionSecretRef::File { .. } => crate::localization::cli_locale()
            .translate("Credential file")
            .to_owned(),
        ConnectionSecretRef::OnePassword { .. } => crate::localization::cli_locale()
            .translate("1Password reference")
            .to_owned(),
        ConnectionSecretRef::Delegated { profile, .. } => profile.as_deref().map_or_else(
            || {
                crate::localization::cli_locale()
                    .translate("Provider sign-in")
                    .to_owned()
            },
            |profile| {
                crate::localization::cli_locale()
                    .format("Provider sign-in: {0}", &[(profile).to_string()])
            },
        ),
    }
}

fn placement_label(placement: ConnectionPlacement) -> &'static str {
    match placement {
        ConnectionPlacement::Local => crate::localization::cli_locale().translate("Local"),
        ConnectionPlacement::Platform => crate::localization::cli_locale().translate("Platform"),
        ConnectionPlacement::Either => {
            crate::localization::cli_locale().translate("Local or Platform")
        }
    }
}

fn auth_kind_label(auth_kind: ConnectionAuthKind) -> &'static str {
    match auth_kind {
        ConnectionAuthKind::ApiKey => crate::localization::cli_locale().translate("API key"),
        ConnectionAuthKind::Subscription => {
            crate::localization::cli_locale().translate("Subscription")
        }
        ConnectionAuthKind::OAuth => "OAuth",
        ConnectionAuthKind::WorkloadIdentity => {
            crate::localization::cli_locale().translate("Workload identity")
        }
    }
}

fn run_dashboard(workspace: Option<&Path>) -> Result<i32> {
    let mut state = DashboardState::load()?;
    let mut terminal = DashboardTerminal::enter()?;
    let result = dashboard_loop(&mut terminal, &mut state, workspace);
    let restore_result = terminal.restore();
    result?;
    restore_result?;
    Ok(0)
}

fn dashboard_loop(
    terminal: &mut DashboardTerminal,
    state: &mut DashboardState,
    workspace: Option<&Path>,
) -> Result<()> {
    loop {
        terminal.draw(state)?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Char('q') => return Ok(()),
            KeyCode::Esc if state.remove_confirmation => state.remove_confirmation = false,
            KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('p') => state.move_selection(-1),
            KeyCode::Down | KeyCode::Char('n') => state.move_selection(1),
            KeyCode::Char('r') => {
                if let Err(error) = state.refresh() {
                    state.message = Some(
                        crate::localization::cli_locale()
                            .format("Could not refresh connections: {0}", &[(error).to_string()]),
                    );
                }
            }
            KeyCode::Char('t') => {
                run_dashboard_check(terminal, state)?;
            }
            KeyCode::Char('d') => {
                if let Err(error) = state.make_selected_default() {
                    state.message = Some(
                        crate::localization::cli_locale()
                            .format("Could not set the default: {0}", &[(error).to_string()]),
                    );
                }
            }
            KeyCode::Char('x') if state.selected_connection().is_some() => {
                state.remove_confirmation = true;
            }
            KeyCode::Char('y') if state.remove_confirmation => {
                if let Err(error) = state.remove_selected() {
                    state.message = Some(crate::localization::cli_locale().format(
                        "Could not remove the connection: {0}",
                        &[(error).to_string()],
                    ));
                }
            }
            KeyCode::Char('1') => {
                let workspace = workspace.map(Path::to_path_buf);
                run_dashboard_prompt(
                    terminal,
                    state,
                    || one_password::run(workspace.as_deref()),
                    crate::localization::cli_locale().translate("1Password capability added. Select it explicitly or set it as the default."),
                )?;
            }
            KeyCode::Char('a') => {
                let workspace = workspace.map(Path::to_path_buf);
                run_dashboard_prompt(
                    terminal,
                    state,
                    || run_add_wizard(workspace.as_deref()),
                    crate::localization::cli_locale().translate("Connection added."),
                )?;
            }
            KeyCode::Char('k') => {
                let Some(id) = state.selected_id() else {
                    state.message = Some(
                        crate::localization::cli_locale()
                            .translate("Select a connection before rotating a key.")
                            .to_owned(),
                    );
                    continue;
                };
                if !matches!(
                    state
                        .selected_connection()
                        .map(|connection| &connection.secret_ref),
                    Some(ConnectionSecretRef::Keyring { .. })
                ) {
                    state.message = Some(
                        crate::localization::cli_locale()
                            .translate("Only OS credential store connections can be rotated.")
                            .to_owned(),
                    );
                    continue;
                }
                run_dashboard_prompt(
                    terminal,
                    state,
                    || {
                        run_rotate(&Args {
                            command: Some("rotate".to_owned()),
                            positionals: vec![id.clone()],
                            ..Args::default()
                        })
                        .map(|_| ())
                    },
                    crate::localization::cli_locale().translate("Key rotated."),
                )?;
            }
            _ => {}
        }
    }
}

fn run_dashboard_check(terminal: &mut DashboardTerminal, state: &mut DashboardState) -> Result<()> {
    let Some(id) = state.selected_id() else {
        state.message = Some(
            crate::localization::cli_locale()
                .translate("Add a connection before checking its credential source.")
                .to_owned(),
        );
        return Ok(());
    };
    let store = state.store.clone();
    terminal.suspend()?;
    let health = check_connection_health(&store, &id);
    terminal.resume()?;
    state.health.insert(id.clone(), health);
    state.message = Some(format!("{id}: {}.", health.label()));
    Ok(())
}

fn run_dashboard_prompt(
    terminal: &mut DashboardTerminal,
    state: &mut DashboardState,
    operation: impl FnOnce() -> Result<()>,
    success_message: &str,
) -> Result<()> {
    let result = terminal.suspend().and_then(|()| operation());
    let resume_result = terminal.resume();
    resume_result?;
    match result {
        Ok(()) => match state.refresh() {
            Ok(()) => state.message = Some(success_message.to_owned()),
            Err(error) => {
                state.message = Some(crate::localization::cli_locale().format(
                    "{0} Refresh failed: {1}",
                    &[(success_message).to_string(), (error).to_string()],
                ));
            }
        },
        Err(error) => state.message = Some(format!("{error}")),
    }
    Ok(())
}

struct DashboardTerminal {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    active: bool,
}

impl DashboardTerminal {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;
        Ok(Self {
            terminal,
            active: true,
        })
    }

    fn draw(&mut self, state: &mut DashboardState) -> Result<()> {
        self.terminal.draw(|frame| render_dashboard(frame, state))?;
        Ok(())
    }

    fn suspend(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        self.terminal.show_cursor()?;
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        self.active = false;
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        if self.active {
            return Ok(());
        }
        enable_raw_mode()?;
        if let Err(error) = execute!(self.terminal.backend_mut(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        self.terminal.clear()?;
        self.active = true;
        Ok(())
    }

    fn restore(&mut self) -> Result<()> {
        self.suspend()
    }
}

impl Drop for DashboardTerminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn render_dashboard(frame: &mut Frame, state: &mut DashboardState) {
    render_dashboard_with_theme(frame, state, crate::themes::current_ui_theme());
}

fn render_dashboard_with_theme(
    frame: &mut Frame,
    state: &mut DashboardState,
    theme: maestro_ui::UiTheme,
) {
    let area = frame.area();
    frame.buffer_mut().set_style(area, theme.text_style());
    let layout = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(2),
    ])
    .split(area);
    let header = Paragraph::new(vec![
        Line::from(Span::styled(
            crate::localization::cli_locale().translate("Connections & access"),
            Style::default()
                .fg(theme.focus)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(
            crate::localization::cli_locale()
                .translate("Provider credentials and sign-ins used by Deixic Code."),
        ),
    ]);
    frame.render_widget(header, layout[0]);

    let panes = Layout::horizontal([Constraint::Percentage(44), Constraint::Percentage(56)])
        .split(layout[1]);
    let items = if state.store.connections.is_empty() {
        vec![ListItem::new(vec![
            Line::styled(
                crate::localization::cli_locale().translate("No managed connections"),
                Style::default().fg(theme.muted),
            ),
            Line::styled(
                crate::localization::cli_locale().translate("Press a to add a connection."),
                Style::default().fg(theme.focus),
            ),
        ])]
    } else {
        state
            .store
            .connections
            .iter()
            .map(|connection| {
                let health = state
                    .health
                    .get(&connection.id)
                    .copied()
                    .unwrap_or(ConnectionHealth::Unknown);
                let default = if connection.is_default {
                    crate::localization::cli_locale().translate("  default")
                } else {
                    ""
                };
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(connection.label.clone(), Style::default().fg(theme.text)),
                        Span::styled(default, Style::default().fg(theme.attention)),
                    ]),
                    Line::from(vec![
                        Span::styled(
                            format!("{}  ", connection.provider_id),
                            Style::default().fg(theme.muted),
                        ),
                        Span::styled(health.label(), Style::default().fg(health.color(theme))),
                    ]),
                ])
            })
            .collect()
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border))
                .title(crate::localization::cli_locale().translate("Connections")),
        )
        .highlight_symbol("› ")
        .style(theme.text_style())
        .highlight_style(theme.selection_style());
    frame.render_stateful_widget(list, panes[0], &mut state.list_state);

    let detail = state.selected_connection().map_or_else(
        || {
            vec![
                Line::from(crate::localization::cli_locale().translate("No connection selected.")),
                Line::from(
                    crate::localization::cli_locale()
                        .translate("Press a to add a provider connection."),
                ),
            ]
        },
        |connection| {
            let health = state
                .health
                .get(&connection.id)
                .copied()
                .unwrap_or(ConnectionHealth::Unknown);
            vec![
                detail_line(
                    crate::localization::cli_locale().translate("Name"),
                    &connection.label,
                    theme,
                ),
                detail_line(
                    crate::localization::cli_locale().translate("Provider"),
                    &connection.provider_id,
                    theme,
                ),
                detail_line(
                    crate::localization::cli_locale().translate("Authentication"),
                    auth_kind_label(connection.auth_kind),
                    theme,
                ),
                detail_line(
                    crate::localization::cli_locale().translate("Access"),
                    &connection.capabilities.join(", "),
                    theme,
                ),
                detail_line(
                    crate::localization::cli_locale().translate("Runs in"),
                    placement_label(connection.placement),
                    theme,
                ),
                detail_line(
                    crate::localization::cli_locale().translate("Credential source"),
                    &credential_source_label(connection),
                    theme,
                ),
                detail_line(
                    crate::localization::cli_locale().translate("Generation"),
                    &connection.generation.to_string(),
                    theme,
                ),
                detail_line(
                    crate::localization::cli_locale().translate("Default"),
                    if connection.is_default {
                        crate::localization::cli_locale().translate("Yes")
                    } else {
                        crate::localization::cli_locale().translate("No")
                    },
                    theme,
                ),
                Line::from(vec![
                    Span::styled(
                        crate::localization::cli_locale().translate("Status: "),
                        Style::default().fg(theme.muted),
                    ),
                    Span::styled(health.label(), Style::default().fg(health.color(theme))),
                ]),
            ]
        },
    );
    let detail = Paragraph::new(detail)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(crate::localization::cli_locale().translate("Connection details"))
                .border_style(Style::default().fg(theme.border)),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(detail, panes[1]);

    let default_shortcuts = format!(
        "1 1Password   {}",
        crate::localization::cli_locale().translate(
            "a Add   t Check source   d Set default   k Rotate key   x Remove   r Refresh   q Close"
        )
    );
    let footer = if state.remove_confirmation {
        Paragraph::new(
            crate::localization::cli_locale()
                .translate("Remove this connection? Press y to remove it, or Esc to cancel."),
        )
        .style(Style::default().fg(theme.error))
    } else {
        Paragraph::new(state.message.as_deref().unwrap_or(&default_shortcuts))
            .style(Style::default().fg(theme.muted))
    };
    frame.render_widget(footer, layout[2]);
}

fn detail_line(label: &str, value: &str, theme: maestro_ui::UiTheme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), Style::default().fg(theme.muted)),
        Span::raw(value.to_owned()),
    ])
}

/// Store a local API key in the connections keyring and mark it default.
pub(crate) fn save_local_api_key(provider_id: &str, secret: &str) -> Result<String> {
    let secret = secret.trim();
    if secret.is_empty() {
        bail!(
            "{}",
            crate::localization::cli_locale().format("credential value must not be empty", &[])
        );
    }
    let definition = connection_types(None)?
        .into_iter()
        .find(|report| {
            report.definition.provider_id == provider_id
                && report.definition.auth_kind == ConnectionAuthKind::ApiKey
        })
        .map(|report| report.definition)
        .with_context(|| format!("no API key connection type for {provider_id}"))?;
    let path = ConnectionStore::default_path()?;
    let backend = KeyringSecretBackend;
    let id = format!("{provider_id}-local");
    with_locked_store(&path, |store| {
        if store.get(&id).is_some() {
            let replacement = zeroize::Zeroizing::new(secret.to_owned());
            let connection = store
                .get(&id)
                .with_context(|| format!("connection not found: {id}"))?;
            let next = connection.generation.saturating_add(1);
            let next_ref = keyring_secret_ref(&id, next);
            let ConnectionSecretRef::Keyring { service, account } = &next_ref else {
                bail!(
                    "{}",
                    crate::localization::cli_locale().format("expected keyring secret", &[])
                );
            };
            backend.set(service, account, &replacement)?;
            let old_ref = connection.secret_ref.clone();
            let mut updated = connection.clone();
            updated.generation = next;
            updated.secret_ref = next_ref;
            updated.is_default = true;
            updated.updated_at_ms = now_ms();
            store.upsert(updated)?;
            store.set_default(&id)?;
            store.save(&path)?;
            if let ConnectionSecretRef::Keyring { service, account } = old_ref {
                let _ = backend.delete(&service, &account);
            }
            return Ok(id);
        }
        let secret_ref = keyring_secret_ref(&id, 1);
        let (service, account) = match &secret_ref {
            ConnectionSecretRef::Keyring { service, account } => (service.clone(), account.clone()),
            _ => bail!(
                "{}",
                crate::localization::cli_locale().format("expected keyring secret", &[])
            ),
        };
        backend.set(&service, &account, secret)?;
        let timestamp = now_ms();
        let connection = ServiceConnection {
            id: id.clone(),
            type_id: definition.id.clone(),
            provider_id: definition.provider_id.clone(),
            label: format!("{} local", definition.display_name),
            auth_kind: definition.auth_kind,
            env_var: definition.env_var.clone(),
            secret_ref,
            placement: definition.placement,
            state: ConnectionState::Active,
            capabilities: definition.capabilities.clone(),
            mcp_binding: None,
            owner: None,
            generation: 1,
            is_default: true,
            created_at_ms: timestamp,
            updated_at_ms: timestamp,
        };
        if let Err(error) = store.upsert(connection).and_then(|()| store.save(&path)) {
            let _ = backend.delete(&service, &account);
            return Err(error);
        }
        Ok(id)
    })
}

pub(crate) fn run_add_wizard(workspace: Option<&Path>) -> Result<()> {
    let types = connection_types(workspace)?;
    println!(
        "{}",
        crate::localization::cli_locale().format("Add connection", &[])
    );
    for (index, report) in types.iter().enumerate() {
        println!(
            "  {}. {} ({})",
            index + 1,
            localized_connection_type_name(report, crate::localization::cli_locale()),
            report.source
        );
    }
    let selection = prompt_required("Connection type number")?;
    let selection = selection
        .parse::<usize>()
        .context("connection type number must be an integer")?;
    let definition = connection_type_by_number(&types, selection)?;
    let id = prompt_required("Connection ID")?;
    let label = prompt_optional("Connection label")?;
    let default = prompt_yes_no("Make this the default for this provider", true)?;
    let mut args = Args {
        command: Some("add".to_owned()),
        positionals: vec![definition.definition.id.clone(), id],
        label,
        default,
        workspace: workspace.map(Path::to_path_buf),
        ..Args::default()
    };
    if definition.definition.auth_kind == ConnectionAuthKind::ApiKey {
        println!(
            "{}",
            crate::localization::cli_locale().format("Credential source", &[])
        );
        println!(
            "{}",
            crate::localization::cli_locale().format("  1. OS credential store", &[])
        );
        println!(
            "{}",
            crate::localization::cli_locale().format("  2. Environment variable", &[])
        );
        println!(
            "{}",
            crate::localization::cli_locale().format("  3. Credential file", &[])
        );
        println!(
            "{}",
            crate::localization::cli_locale().format("  4. 1Password reference", &[])
        );
        match prompt_required("Credential source number")?.as_str() {
            "1" => {}
            "2" => args.from_env = Some(prompt_required("Environment variable")?),
            "3" => args.from_file = Some(PathBuf::from(prompt_required("Credential file path")?)),
            "4" => return one_password::run(workspace),
            _ => bail!(
                "{}",
                crate::localization::cli_locale()
                    .format("credential source number must be 1, 2, 3, or 4", &[])
            ),
        }
    } else {
        args.delegated_profile = prompt_optional("Provider profile")?;
    }
    run_add(&args)?;
    Ok(())
}

fn connection_type_by_number(
    types: &[ConnectionTypeReport],
    selection: usize,
) -> Result<&ConnectionTypeReport> {
    selection
        .checked_sub(1)
        .and_then(|index| types.get(index))
        .context("connection type number is out of range")
}

fn prompt_required(label: &str) -> Result<String> {
    prompt_optional(label)?
        .filter(|value| !value.is_empty())
        .context(format!("{label} is required"))
}

fn prompt_optional(label: &str) -> Result<Option<String>> {
    print!("{label}: ");
    io::stdout().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    let value = value.trim().to_owned();
    Ok((!value.is_empty()).then_some(value))
}

fn prompt_yes_no(label: &str, default: bool) -> Result<bool> {
    let suffix = if default { "Y/n" } else { "y/N" };
    let response = prompt_optional(&format!("{label} [{suffix}]"))?;
    match response.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None => Ok(default),
        Some("y" | "yes") => Ok(true),
        Some("n" | "no") => Ok(false),
        _ => bail!("enter y or n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn company_setup_reuses_only_an_active_connection_owned_by_this_user() {
        let owner = ConnectionOwner {
            organization_id: "org-one".into(),
            user_id: "user-one".into(),
        };
        let mut own = test_connection("codex-own");
        own.type_id = "codex-subscription".into();
        own.provider_id = "openai-codex".into();
        own.secret_ref = ConnectionSecretRef::Delegated {
            provider: "openai-codex".into(),
            profile: Some("default".into()),
        };
        own.owner = Some(owner.clone());
        let mut other = own.clone();
        other.id = "codex-other".into();
        other.owner = Some(ConnectionOwner {
            organization_id: "org-one".into(),
            user_id: "user-two".into(),
        });
        other.is_default = true;
        let store = ConnectionStore {
            schema_version: 1,
            connections: vec![other, own.clone()],
        };
        assert_eq!(
            owned_codex_connection(&store, &owner, "default").map(|c| c.id.as_str()),
            Some("codex-own")
        );
        assert!(owned_codex_connection(&store, &owner, "other").is_none());
        own.state = ConnectionState::Revoked;
        let revoked = ConnectionStore {
            schema_version: 1,
            connections: vec![own],
        };
        assert!(owned_codex_connection(&revoked, &owner, "default").is_none());
    }

    struct DeleteFails;

    impl SecretBackend for DeleteFails {
        fn get(&self, _service: &str, _account: &str) -> Result<zeroize::Zeroizing<String>> {
            bail!("unused")
        }

        fn set(&self, _service: &str, _account: &str, _value: &str) -> Result<()> {
            bail!("unused")
        }

        fn delete(&self, _service: &str, _account: &str) -> Result<()> {
            bail!("simulated credential cleanup failure")
        }
    }

    fn test_connection(id: &str) -> ServiceConnection {
        ServiceConnection {
            id: id.into(),
            type_id: "openai-api-key".into(),
            provider_id: "openai".into(),
            label: id.into(),
            auth_kind: ConnectionAuthKind::ApiKey,
            env_var: Some("OPENAI_API_KEY".into()),
            secret_ref: keyring_secret_ref(id, 1),
            placement: ConnectionPlacement::Local,
            state: ConnectionState::Active,
            capabilities: vec!["models.invoke".into()],
            mcp_binding: None,
            owner: None,
            generation: 1,
            is_default: false,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn codex_subscription_id_routes_to_local_store_even_in_platform_mode() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("connections.json");
        let mut codex = test_connection("codex-personal");
        codex.type_id = "codex-subscription".into();
        codex.provider_id = "openai-codex".into();
        codex.auth_kind = ConnectionAuthKind::Subscription;
        codex.env_var = None;
        codex.secret_ref = ConnectionSecretRef::Delegated {
            provider: "openai-codex".into(),
            profile: None,
        };
        ConnectionStore {
            schema_version: 1,
            connections: vec![codex],
        }
        .save(&path)
        .unwrap();
        let args = Args {
            positionals: vec!["codex-personal".into()],
            ..Default::default()
        };
        assert!(local_subscription_connection_at(&args, &path).unwrap());
        let other = Args {
            positionals: vec!["unknown".into()],
            ..Default::default()
        };
        assert!(!local_subscription_connection_at(&other, &path).unwrap());
        std::fs::write(&path, "not-json").unwrap();
        assert!(!local_subscription_connection_at(&args, &path).unwrap());
    }

    #[test]
    fn complete_platform_env_skips_the_keychain_backed_snapshot() {
        let env = HashMap::from([
            (
                crate::credential_mode::ACCESS_TOKEN_ENV.to_owned(),
                "managed-access-token".to_owned(),
            ),
            (
                crate::credential_mode::ORG_ID_ENV.to_owned(),
                "org_evalops".to_owned(),
            ),
        ]);

        let session = platform_session_from_env_or_snapshot(&env, || {
            panic!("complete platform environment must not read the credential store")
        })
        .expect("platform session");

        assert_eq!(session.organization_id, "org_evalops");
        assert_eq!(session.access_token, "managed-access-token");
    }

    #[test]
    fn incomplete_platform_env_falls_back_to_the_snapshot_source() {
        let loaded = Cell::new(false);
        let session = platform_session_from_env_or_snapshot(&HashMap::new(), || {
            loaded.set(true);
            None
        });

        assert!(loaded.get());
        assert!(session.is_none());
    }

    #[test]
    fn parses_reference_without_accepting_literal_secret_flag() {
        let parsed = parse_args(&[
            "add".into(),
            "openai-api-key".into(),
            "work".into(),
            "--from-env".into(),
            "WORK_OPENAI_KEY".into(),
            "--default".into(),
        ])
        .unwrap();
        assert_eq!(parsed.from_env.as_deref(), Some("WORK_OPENAI_KEY"));
        assert!(parsed.default);
        assert!(parse_args(&["add".into(), "--api-key".into(), "secret".into()]).is_err());
    }

    #[test]
    fn guided_add_does_not_discard_explicit_options() {
        let options = [
            Args {
                label: Some("work".into()),
                ..Args::default()
            },
            Args {
                default: true,
                ..Args::default()
            },
            Args {
                from_env: Some("OPENAI_API_KEY".into()),
                ..Args::default()
            },
            Args {
                from_file: Some(PathBuf::from("credential.txt")),
                ..Args::default()
            },
            Args {
                from_one_password: Some("op://vault/item".into()),
                ..Args::default()
            },
            Args {
                delegated_profile: Some("work".into()),
                ..Args::default()
            },
            Args {
                secret_stdin: true,
                ..Args::default()
            },
        ];

        assert!(options.iter().all(has_explicit_add_options));
        assert!(!has_explicit_add_options(&Args::default()));
    }

    #[test]
    fn guided_add_rejects_zero_connection_type_number() {
        let types = vec![ConnectionTypeReport {
            definition: builtin_connection_types().into_iter().next().unwrap(),
            source: "built-in".into(),
        }];

        assert!(connection_type_by_number(&types, 0).is_err());
        assert_eq!(
            connection_type_by_number(&types, 1)
                .unwrap()
                .definition
                .id
                .as_str(),
            types[0].definition.id.as_str()
        );
    }

    #[test]
    fn builtins_cover_api_keys_and_delegated_subscriptions() {
        let types = builtin_connection_types();
        assert!(types.iter().any(|item| item.id == "openai-api-key"));
        assert!(types.iter().any(|item| item.id == "anthropic-api-key"));
        assert!(types.iter().any(|item| item.id == "codex-subscription"));
        assert!(types.iter().any(|item| item.id == "claude-subscription"));
        assert!(types.iter().any(|item| item.id == "copilot-subscription"));
        assert!(types.iter().all(|item| item.provider_id != "orb"));
        assert!(
            types
                .iter()
                .find(|item| item.id == "codex-subscription")
                .is_some_and(|item| item.auth_kind == ConnectionAuthKind::Subscription)
        );
    }

    #[test]
    fn codex_connection_stores_resolved_profile_instead_of_workspace_default() {
        let definition = builtin_connection_types()
            .into_iter()
            .find(|item| item.id == "codex-subscription")
            .unwrap();
        let (secret_ref, stored_key) = source_for_add(
            &Args::default(),
            &definition,
            "codex-work",
            &KeyringSecretBackend,
            Some("company"),
        )
        .unwrap();
        assert_eq!(stored_key, None);
        assert_eq!(
            secret_ref,
            ConnectionSecretRef::Delegated {
                provider: "openai-codex".into(),
                profile: Some("company".into()),
            }
        );
    }

    #[test]
    fn local_cli_rejects_platform_only_connection_types() {
        let definition = ConnectionTypeDefinition {
            id: "platform-api-key".into(),
            display_name: "Platform API key".into(),
            provider_id: "openai".into(),
            auth_kind: ConnectionAuthKind::ApiKey,
            placement: ConnectionPlacement::Platform,
            env_var: Some("OPENAI_API_KEY".into()),
            capabilities: vec!["models.invoke".into()],
            documentation_url: None,
            mcp_binding: None,
        };
        let error = source_for_add(
            &Args::default(),
            &definition,
            "platform-only",
            &KeyringSecretBackend,
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("Platform credential broker"));
    }

    #[test]
    fn local_cli_rejects_unverified_delegated_connection_types() {
        let definition = ConnectionTypeDefinition {
            id: "vendor-oauth".into(),
            display_name: "Vendor OAuth".into(),
            provider_id: "vendor-plugin".into(),
            auth_kind: ConnectionAuthKind::OAuth,
            placement: ConnectionPlacement::Local,
            env_var: None,
            capabilities: vec!["models.invoke".into()],
            documentation_url: None,
            mcp_binding: None,
        };
        let error = source_for_add(
            &Args::default(),
            &definition,
            "vendor-work",
            &KeyringSecretBackend,
            None,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no verified delegated authentication transport")
        );
    }

    #[test]
    fn api_key_connection_rejects_a_delegated_profile_without_reading_a_secret() {
        let definition = builtin_connection_types()
            .into_iter()
            .find(|item| item.id == "openai-api-key")
            .unwrap();
        let args = Args {
            delegated_profile: Some("work".into()),
            ..Args::default()
        };

        let error =
            source_for_add(&args, &definition, "work", &KeyringSecretBackend, None).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("only valid for subscription and OAuth")
        );
    }

    #[test]
    fn new_connection_defaults_when_no_active_default_exists() {
        let mut revoked_default = test_connection("revoked");
        revoked_default.state = ConnectionState::Revoked;
        let store = ConnectionStore {
            schema_version: 1,
            connections: vec![revoked_default],
        };
        assert!(should_be_default(&store, "openai", false));

        let mut active_default = test_connection("active");
        active_default.is_default = true;
        let store = ConnectionStore {
            schema_version: 1,
            connections: vec![active_default],
        };
        assert!(!should_be_default(&store, "openai", false));
        assert!(should_be_default(&store, "openai", true));
    }

    #[test]
    fn store_lock_serializes_read_modify_write_transactions() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("connections.json");
        let first_path = path.clone();
        let second_path = path.clone();
        let (first_acquired_tx, first_acquired_rx) = mpsc::channel();
        let (release_first_tx, release_first_rx) = mpsc::channel();
        let (second_acquired_tx, second_acquired_rx) = mpsc::channel();

        let first = std::thread::spawn(move || {
            with_locked_store(&first_path, |store| {
                store.upsert(test_connection("first"))?;
                first_acquired_tx.send(()).unwrap();
                release_first_rx.recv().unwrap();
                store.save(&first_path)
            })
            .unwrap();
        });
        first_acquired_rx.recv().unwrap();

        let second = std::thread::spawn(move || {
            with_locked_store(&second_path, |store| {
                second_acquired_tx.send(()).unwrap();
                store.upsert(test_connection("second"))?;
                store.save(&second_path)
            })
            .unwrap();
        });
        assert!(
            second_acquired_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );
        release_first_tx.send(()).unwrap();
        first.join().unwrap();
        second_acquired_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        second.join().unwrap();

        let store = ConnectionStore::load(&path).unwrap();
        assert!(store.get("first").is_some());
        assert!(store.get("second").is_some());
    }

    #[test]
    fn removal_is_durable_before_credential_cleanup() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("connections.json");
        ConnectionStore {
            schema_version: 1,
            connections: vec![test_connection("obsolete")],
        }
        .save(&path)
        .unwrap();

        remove_connection(&path, "obsolete", &DeleteFails).unwrap();

        assert!(
            ConnectionStore::load(&path)
                .unwrap()
                .get("obsolete")
                .is_none()
        );
    }

    #[test]
    fn dashboard_shows_connection_metadata_without_secret_references() {
        let mut connection = test_connection("work");
        connection.label = "OpenAI work".into();
        connection.is_default = true;
        connection.secret_ref = ConnectionSecretRef::OnePassword {
            reference: "op://engineering/openai/credential".into(),
        };
        let mut state = DashboardState {
            store: ConnectionStore {
                schema_version: 1,
                connections: vec![connection],
            },
            selected: 0,
            list_state: ListState::default(),
            health: BTreeMap::from([("work".into(), ConnectionHealth::Ready)]),
            message: None,
            remove_confirmation: false,
        };
        state.sync_selection();

        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_dashboard(frame, &mut state))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("Connections & access"));
        assert!(rendered.contains("OpenAI work"));
        assert!(rendered.contains("1Password reference"));
        assert!(rendered.contains("models.invoke"));
        assert!(!rendered.contains("op://engineering/openai/credential"));
    }

    #[test]
    fn dashboard_uses_shared_theme() {
        for theme in crate::components::theme_test::palettes() {
            let mut connection = test_connection("work");
            connection.label = "OpenAI work".into();
            connection.is_default = true;
            connection.secret_ref = ConnectionSecretRef::OnePassword {
                reference: "op://engineering/openai/credential".into(),
            };
            let mut state = DashboardState {
                store: ConnectionStore {
                    schema_version: 1,
                    connections: vec![connection],
                },
                selected: 0,
                list_state: ListState::default(),
                health: BTreeMap::from([("work".into(), ConnectionHealth::Ready)]),
                message: None,
                remove_confirmation: false,
            };
            state.sync_selection();

            let backend = ratatui::backend::TestBackend::new(100, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| render_dashboard_with_theme(frame, &mut state, theme))
                .unwrap();
            let rendered = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();

            assert!(rendered.contains("Connections & access"));
            assert!(rendered.contains("OpenAI work"));
            assert!(rendered.contains("1Password reference"));
            assert!(rendered.contains("models.invoke"));
            assert!(!rendered.contains("op://engineering/openai/credential"));
            crate::components::theme_test::assert_palette(terminal.backend().buffer(), theme);
            crate::components::theme_test::assert_label(
                terminal.backend().buffer(),
                ConnectionHealth::Ready.label(),
                theme.success,
                theme.selection.unwrap(),
            );
        }
    }

    #[test]
    fn resolve_connection_type_accepts_provider_id() {
        let types = connection_types(None).expect("types");
        let by_id = resolve_connection_type(&types, "anthropic-api-key").expect("type id");
        let by_provider = resolve_connection_type(&types, "anthropic").expect("provider id");
        assert_eq!(by_id.id, "anthropic-api-key");
        assert_eq!(by_provider.id, by_id.id);
        assert_eq!(by_provider.provider_id, "anthropic");
        let err = resolve_connection_type(&types, "not-a-provider").expect_err("unknown type");
        assert!(
            err.to_string()
                .contains("unknown or untrusted connection type: not-a-provider"),
            "{err:#}"
        );
    }

    #[test]
    fn resolve_connection_type_reports_ambiguous_provider() {
        let types = vec![
            ConnectionTypeReport {
                definition: ConnectionTypeDefinition {
                    id: "anthropic-api-key".into(),
                    display_name: "Anthropic API key".into(),
                    provider_id: "anthropic".into(),
                    auth_kind: ConnectionAuthKind::ApiKey,
                    placement: ConnectionPlacement::Either,
                    env_var: Some("ANTHROPIC_API_KEY".into()),
                    capabilities: vec![],
                    documentation_url: None,
                    mcp_binding: None,
                },
                source: "maestro".into(),
            },
            ConnectionTypeReport {
                definition: ConnectionTypeDefinition {
                    id: "anthropic-plugin".into(),
                    display_name: "Anthropic plugin".into(),
                    provider_id: "anthropic".into(),
                    auth_kind: ConnectionAuthKind::ApiKey,
                    placement: ConnectionPlacement::Either,
                    env_var: None,
                    capabilities: vec![],
                    documentation_url: None,
                    mcp_binding: None,
                },
                source: "plugin:extra".into(),
            },
        ];
        let err = resolve_connection_type(&types, "anthropic").expect_err("ambiguous");
        let message = err.to_string();
        assert!(
            message.contains("ambiguous connection type anthropic"),
            "{message}"
        );
        assert!(message.contains("anthropic-api-key"), "{message}");
        assert!(message.contains("anthropic-plugin"), "{message}");
    }
}
