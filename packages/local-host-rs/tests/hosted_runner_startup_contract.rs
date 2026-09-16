//! Executable form of `docs/protocols/hosted-runner-contract.md`.
//!
//! The startup matrix lives in `fixtures/hosted-runner-startup-contract.json`
//! so the documented flag/environment coordinates, the CLI resolver, and the
//! canary that launches real pods share one table. Every case runs through
//! `resolve_hosted_runner_launch_config`, the same entrypoint
//! `deixic-code hosted-runner` uses; a second test drives the full CLI runtime
//! (stub headless child, identity readiness, drain manifest) through
//! `start_hosted_runner_cli_runtime`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use maestro_local_host::hosted_runner_cli::{
    HostedRunnerLaunchConfig, resolve_hosted_runner_launch_config,
};
use serde::Deserialize;
use serde_json::Value;
use tempfile::TempDir;

const FIXTURE: &str = include_str!("fixtures/hosted-runner-startup-contract.json");
const CONTRACT_DOC: &str = include_str!("../../../docs/protocols/hosted-runner-contract.md");
const SCHEMA_VERSION: &str = "evalops.maestro.hosted-runner-startup-contract.v1";

#[derive(Debug, Deserialize)]
struct Fixture {
    schema_version: String,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    #[serde(default)]
    files: BTreeMap<String, String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    expect: Expect,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Expect {
    Accept(BTreeMap<String, Value>),
    Reject(String),
}

struct Scratch {
    workspace: TempDir,
    files: TempDir,
    agent: PathBuf,
}

impl Scratch {
    fn new(case: &Case) -> Self {
        let workspace = TempDir::new().expect("workspace tempdir");
        let files = TempDir::new().expect("files tempdir");
        for (name, content) in &case.files {
            fs::write(files.path().join(name), content).expect("fixture file");
        }
        let agent = write_stub_agent(workspace.path());
        Self {
            workspace,
            files,
            agent,
        }
    }

    fn expand(&self, raw: &str) -> String {
        let mut value = raw
            .replace("${workspace}", &self.workspace.path().to_string_lossy())
            .replace("${agent}", &self.agent.to_string_lossy());
        while let Some(start) = value.find("${file:") {
            let end = value[start..]
                .find('}')
                .map(|offset| start + offset)
                .expect("unterminated ${file:} placeholder");
            let name = &value[start + "${file:".len()..end];
            let path = self.files.path().join(name);
            assert!(
                path.is_file(),
                "fixture file {name} is not declared in `files`"
            );
            value.replace_range(start..=end, &path.to_string_lossy());
        }
        value
    }
}

/// A headless child that announces readiness and then idles until stdin closes.
fn write_stub_agent(dir: &Path) -> PathBuf {
    let agent = dir.join("stub-maestro-headless.sh");
    fs::write(
        &agent,
        "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"ready\",\"model\":\"gpt-5.5\",\"provider\":\"test\"}'\nwhile IFS= read -r line; do :; done\n",
    )
    .expect("stub agent");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&agent).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&agent, permissions).expect("chmod");
    }
    agent
}

fn load_fixture() -> Fixture {
    let fixture: Fixture = serde_json::from_str(FIXTURE).expect("fixture parses");
    assert_eq!(fixture.schema_version, SCHEMA_VERSION);
    assert!(!fixture.cases.is_empty());
    fixture
}

fn resolve(case: &Case, scratch: &Scratch) -> anyhow::Result<HostedRunnerLaunchConfig> {
    let args = std::iter::once("deixic-code hosted-runner".to_string())
        .chain(case.args.iter().map(|arg| scratch.expand(arg)))
        .collect::<Vec<_>>();
    let env = case
        .env
        .iter()
        .map(|(key, value)| (key.clone(), scratch.expand(value)))
        .collect::<HashMap<_, _>>();
    resolve_hosted_runner_launch_config(args, &env)
}

fn observed(config: &HostedRunnerLaunchConfig, field: &str) -> Value {
    let runner = &config.runner;
    match field {
        "runner_session_id" => runner.runner_session_id.clone().into(),
        "owner_instance_id" => runner.owner_instance_id.clone().into(),
        "runtime_generation" => runner.runtime_generation.into(),
        "workspace_id" => runner.workspace_id.clone().into(),
        "agent_run_id" => runner.agent_run_id.clone().into(),
        "maestro_session_id" => runner.maestro_session_id.clone().into(),
        "attach_audience" => runner.attach_audience.clone().into(),
        "bind_addr" => runner.bind_addr.to_string().into(),
        "auth_token_present" => runner.auth_token.is_some().into(),
        "workload_identity_present" => runner.workload_identity.is_some().into(),
        "agent_cli_path" => config.supervisor.transport.cli_path.clone().into(),
        other => panic!("fixture asserts unknown field {other}"),
    }
}

#[test]
fn every_documented_startup_case_accepts_or_fails_closed() {
    for case in load_fixture().cases {
        let scratch = Scratch::new(&case);
        let result = resolve(&case, &scratch);
        match &case.expect {
            Expect::Accept(fields) => {
                let config = result.unwrap_or_else(|error| {
                    panic!("case `{}` must be accepted, got: {error:#}", case.name)
                });
                for (field, expected) in fields {
                    let expected = match expected {
                        Value::String(raw) => Value::String(scratch.expand(raw)),
                        other => other.clone(),
                    };
                    assert_eq!(
                        observed(&config, field),
                        expected,
                        "case `{}` field `{field}`",
                        case.name
                    );
                }
            }
            Expect::Reject(fragment) => {
                let error = match result {
                    Ok(_) => panic!("case `{}` must fail closed", case.name),
                    Err(error) => format!("{error:#}"),
                };
                assert!(
                    error.contains(fragment),
                    "case `{}` rejected for another reason: {error}",
                    case.name
                );
            }
        }
    }
}

#[test]
fn fixture_case_names_are_unique() {
    let fixture = load_fixture();
    let mut names = fixture
        .cases
        .iter()
        .map(|case| case.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), fixture.cases.len(), "duplicate case names");
}

fn is_documented(name: &str) -> bool {
    CONTRACT_DOC.contains(&format!("`{name}`")) || CONTRACT_DOC.contains(&format!("`{name}="))
}

/// Every coordinate the executable matrix exercises must be documented in the
/// prose contract, so a fixture case cannot introduce an undocumented input.
#[test]
fn every_exercised_coordinate_is_documented() {
    let fixture = load_fixture();
    let mut exercised = fixture
        .cases
        .iter()
        .flat_map(|case| case.env.keys().cloned())
        .chain(fixture.cases.iter().flat_map(|case| {
            case.args
                .iter()
                .filter(|arg| arg.starts_with("--"))
                .cloned()
        }))
        .collect::<Vec<_>>();
    exercised.sort_unstable();
    exercised.dedup();

    let missing_from_doc = exercised
        .iter()
        .filter(|name| !is_documented(name))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        missing_from_doc.is_empty(),
        "fixture coordinates absent from hosted-runner-contract.md: {missing_from_doc:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cli_runtime_reports_identity_and_drains_a_manifest() {
    use maestro_local_host::hosted_runner_cli::{
        HostedRunnerShutdownSignal, start_hosted_runner_cli_runtime,
    };

    let workspace = TempDir::new().expect("workspace");
    let agent = write_stub_agent(workspace.path());
    let env = HashMap::from([
        ("MAESTRO_WEB_REQUIRE_KEY".to_string(), "0".to_string()),
        ("MAESTRO_MODEL".to_string(), "gpt-5.5".to_string()),
    ]);
    // `--listen` rejects port 0, so reserve an ephemeral port and retry on the
    // rare race where another process takes it between release and bind.
    let mut runtime = None;
    for _ in 0..5 {
        let listen = {
            let probe = TcpListener::bind("127.0.0.1:0").expect("ephemeral port probe");
            probe.local_addr().expect("probe addr").to_string()
        };
        match start_hosted_runner_cli_runtime(
            [
                "deixic-code hosted-runner",
                "--runner-session-id",
                "mrs_contract",
                "--owner-instance-id",
                "owner_contract",
                "--workspace-root",
                workspace.path().to_str().expect("workspace path"),
                "--listen",
                listen.as_str(),
                "--agent-cli-path",
                agent.to_str().expect("agent path"),
            ],
            &env,
        )
        .await
        {
            Ok(started) => {
                runtime = Some(started);
                break;
            }
            Err(error) if error.to_string().contains("address") => continue,
            Err(error) => panic!("hosted runner failed to start: {error}"),
        }
    }
    let runtime = runtime.expect("hosted runner starts behind the stub headless child");

    let client = reqwest::Client::new();
    let identity: Value = client
        .get(format!(
            "{}/.well-known/evalops/remote-runner/identity",
            runtime.base_url()
        ))
        .send()
        .await
        .expect("identity response")
        .json()
        .await
        .expect("identity json");
    assert_eq!(
        identity["protocol_version"],
        "evalops.remote-runner.identity.v1"
    );
    assert_eq!(identity["runner_session_id"], "mrs_contract");
    assert_eq!(identity["owner_instance_id"], "owner_contract");
    assert_eq!(identity["ready"], true);
    assert_eq!(identity["draining"], false);
    for hidden in ["workspace_id", "agent_run_id", "maestro_session_id"] {
        assert!(
            identity.get(hidden).is_none(),
            "identity must stay sparse; leaked {hidden}"
        );
    }

    let drain = runtime
        .drain_for_shutdown(HostedRunnerShutdownSignal::Terminate)
        .await
        .expect("drain succeeds");
    assert_eq!(drain["status"], "drained");
    let manifest_path = drain["manifest_path"]
        .as_str()
        .unwrap_or_else(|| panic!("drain response carries a manifest path: {drain}"));
    assert_eq!(
        drain["manifest"]["workspace_export"]["mode"],
        "local_path_contract"
    );
    assert!(
        Path::new(manifest_path).is_file(),
        "drain manifest must be written under the workspace: {manifest_path}"
    );
    let canonical_manifest =
        dunce::canonicalize(manifest_path).expect("drain manifest path is canonicalizable");
    let canonical_workspace =
        dunce::canonicalize(workspace.path()).expect("workspace path is canonicalizable");
    assert!(
        canonical_manifest.starts_with(&canonical_workspace),
        "drain manifest must stay under the workspace root: {} is not under {}",
        canonical_manifest.display(),
        canonical_workspace.display()
    );

    let identity: Value = client
        .get(format!(
            "{}/.well-known/evalops/remote-runner/identity",
            runtime.base_url()
        ))
        .send()
        .await
        .expect("identity response after drain")
        .json()
        .await
        .expect("identity json after drain");
    assert_eq!(identity["draining"], true);
    assert_eq!(identity["ready"], false);

    runtime.shutdown().await;
}
