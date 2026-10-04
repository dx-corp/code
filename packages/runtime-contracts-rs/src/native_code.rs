//! Immutable command identity shared by native ToolExecution and Runner owners.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeCodeCommand {
    pub argv: Vec<String>,
    pub cwd: String,
    pub timeout_seconds: u32,
    pub stdin_sha256: String,
}

impl NativeCodeCommand {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.argv.is_empty()
            || self.argv.len() > 64
            || self
                .argv
                .iter()
                .any(|arg| arg.len() > 65_536 || arg.contains('\0'))
            || self.argv.iter().map(String::len).sum::<usize>() > 262_144
        {
            return Err("argv must contain 1 to 64 bounded arguments without NUL bytes");
        }
        if self.cwd != "/workspace" {
            return Err("native command cwd must be /workspace");
        }
        if !(1..=3600).contains(&self.timeout_seconds) {
            return Err("timeoutSeconds must be between 1 and 3600");
        }
        if self.stdin_sha256.len() != 64
            || !self
                .stdin_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("stdinSha256 must be a lowercase SHA256 digest");
        }
        Ok(())
    }
}

/// SHA256(domain || canonical UTF8 JSON), with keys sorted lexically.
/// Callers validate before admitting or executing the command.
pub fn command_digest(command: &NativeCodeCommand) -> String {
    let fields = BTreeMap::from([
        ("argv", serde_json::json!(command.argv)),
        ("cwd", serde_json::json!(command.cwd)),
        ("stdinSha256", serde_json::json!(command.stdin_sha256)),
        ("timeoutSeconds", serde_json::json!(command.timeout_seconds)),
    ]);
    let mut digest = Sha256::new();
    digest.update(b"evalops.native-code-command.v1\0");
    digest.update(serde_json::to_vec(&fields).expect("command fields are JSON serializable"));
    format!("{:x}", digest.finalize())
}

pub fn tool_idempotency_key(
    org: &str,
    workspace: &str,
    admission_id: &str,
    step_id: &str,
) -> String {
    let mut hash = Sha256::new();
    hash.update(b"evalops.native-code-tool.v1\0");
    for value in [org, workspace, admission_id, step_id] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    format!("native-code-tool-{:x}", hash.finalize())
}

/// Account-admitted checkpoint lineage, derived by the execution owner.
pub fn checkpoint_key(org: &str, workspace: &str, admission_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"evalops.native-code-checkpoint.v1\0");
    for value in [org, workspace, admission_id] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    format!("native-code-checkpoint-{:x}", hash.finalize())
}

/// The complete executable snapshot wrapper becomes part of the approved argv.
pub fn effect_wrapper_command(key: &str, command: &str) -> String {
    fn quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
    format!(
        "mkdir -p -- '/workspace/.dex-home/.local/bin' && exec /opt/maestro/bin/maestro native-code-effect {} {}",
        quote(key),
        quote(command)
    )
}

/// Bind all original turn fields independently of JSON object insertion order.
/// Only the Platform-injected outer admission lookup coordinate is excluded.
pub fn turn_request_digest(request: &serde_json::Value) -> String {
    fn canonical(value: &serde_json::Value, output: &mut Vec<u8>) {
        match value {
            serde_json::Value::Object(fields) => {
                output.push(b'{');
                let sorted = fields.iter().collect::<BTreeMap<_, _>>();
                for (index, (key, value)) in sorted.into_iter().enumerate() {
                    if index != 0 {
                        output.push(b',');
                    }
                    output.extend(serde_json::to_vec(key).expect("JSON key"));
                    output.push(b':');
                    canonical(value, output);
                }
                output.push(b'}');
            }
            serde_json::Value::Array(values) => {
                output.push(b'[');
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        output.push(b',');
                    }
                    canonical(value, output);
                }
                output.push(b']');
            }
            _ => output.extend(serde_json::to_vec(value).expect("JSON value")),
        }
    }
    let mut original = request.clone();
    if let Some(fields) = original.as_object_mut() {
        fields.remove("platformAdmissionId");
    }
    let mut bytes = b"evalops.native-code-turn.v1\0".to_vec();
    canonical(&original, &mut bytes);
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_digest_is_recursive_and_excludes_only_outer_admission_coordinate() {
        let first: serde_json::Value = serde_json::from_str(r#"{"turnId":"turn","request":{"messages":[{"role":"user","content":"hi"}],"extra":{"b":2,"a":1}}}"#).unwrap();
        let second: serde_json::Value = serde_json::from_str(r#"{"request":{"extra":{"a":1,"b":2},"messages":[{"content":"hi","role":"user"}]},"platformAdmissionId":"owner","turnId":"turn"}"#).unwrap();
        assert_eq!(turn_request_digest(&first), turn_request_digest(&second));
        let canonical = b"evalops.native-code-turn.v1\0{\"request\":{\"extra\":{\"a\":1,\"b\":2},\"messages\":[{\"content\":\"hi\",\"role\":\"user\"}]},\"turnId\":\"turn\"}";
        assert_eq!(
            turn_request_digest(&first),
            format!("{:x}", Sha256::digest(canonical))
        );
        let mut changed = first.clone();
        changed["request"]["extra"]["platformAdmissionId"] = serde_json::json!("nested input");
        assert_ne!(turn_request_digest(&first), turn_request_digest(&changed));
        changed = first;
        changed["gatewayEpoch"] = serde_json::json!("other owner");
        assert_ne!(turn_request_digest(&second), turn_request_digest(&changed));
    }

    fn command() -> NativeCodeCommand {
        NativeCodeCommand {
            argv: vec!["/bin/sh".into(), "-c".into(), "printf hello".into()],
            cwd: "/workspace".into(),
            timeout_seconds: 60,
            stdin_sha256: format!("{:x}", Sha256::digest(b"")),
        }
    }

    #[test]
    fn digest_has_fixed_domain_and_canonical_bytes() {
        let command = command();
        command.validate().unwrap();
        let bytes = format!(
            "evalops.native-code-command.v1\0{{\"argv\":[\"/bin/sh\",\"-c\",\"printf hello\"],\"cwd\":\"/workspace\",\"stdinSha256\":\"{}\",\"timeoutSeconds\":60}}",
            command.stdin_sha256
        );
        assert_eq!(
            command_digest(&command),
            format!("{:x}", Sha256::digest(bytes.as_bytes()))
        );
        for changed in [
            NativeCodeCommand {
                argv: vec!["different".into()],
                ..command.clone()
            },
            NativeCodeCommand {
                cwd: "/elsewhere".into(),
                ..command.clone()
            },
            NativeCodeCommand {
                timeout_seconds: 59,
                ..command.clone()
            },
            NativeCodeCommand {
                stdin_sha256: "a".repeat(64),
                ..command.clone()
            },
        ] {
            assert_ne!(command_digest(&command), command_digest(&changed));
        }
    }

    #[test]
    fn wire_fields_are_mandatory_and_command_limits_are_enforced() {
        assert!(
            serde_json::from_value::<NativeCodeCommand>(serde_json::json!({"argv":[]})).is_err()
        );
        let mut command = command();
        command.cwd = "/tmp".into();
        assert!(command.validate().is_err());
        command = self::command();
        command.stdin_sha256 = "A".repeat(64);
        assert!(command.validate().is_err());
        command = self::command();
        command.argv[0].push('\0');
        assert!(command.validate().is_err());
    }

    #[test]
    fn tool_replay_key_has_unambiguous_tenant_admission_and_step_identity() {
        let key = tool_idempotency_key("org", "workspace", "admission", "step");
        assert_eq!(
            key,
            tool_idempotency_key("org", "workspace", "admission", "step")
        );
        assert_ne!(
            key,
            tool_idempotency_key("org", "workspace", "other", "step")
        );
        assert_ne!(
            tool_idempotency_key("a", "bc", "admission", "step"),
            tool_idempotency_key("ab", "c", "admission", "step")
        );
    }

    #[test]
    fn wrapper_uses_the_immutable_guest_binary_and_quotes_literal_command_arguments() {
        let wrapped = effect_wrapper_command("key'quote", "printf '$HOME'; touch 'file name'");
        assert!(wrapped.contains("exec /opt/maestro/bin/maestro native-code-effect"));
        assert!(wrapped.contains("'key'\"'\"'quote'"));
        assert!(wrapped.contains("$HOME"));
        assert!(!wrapped.contains("eval "));
    }

    #[test]
    fn checkpoints_are_bound_to_the_exact_account_admission() {
        let key = checkpoint_key("org", "workspace", "admission");
        assert_eq!(key, checkpoint_key("org", "workspace", "admission"));
        assert_ne!(key, checkpoint_key("other", "workspace", "admission"));
        assert_ne!(key, checkpoint_key("org", "workspace", "other"));
        assert_ne!(
            checkpoint_key("a", "bc", "d"),
            checkpoint_key("ab", "c", "d")
        );
    }
}
