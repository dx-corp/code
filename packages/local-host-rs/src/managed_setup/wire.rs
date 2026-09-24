//! Public client policy conversion rejects unknown values rather than widening access.
use super::ManagedSetupError;
#[cfg(test)]
pub(super) use crate::public_protocol::{
    ClientMcpPolicy as McpPolicy, ClientMcpServer as McpServerRef, ClientRule as ManagedRule,
    ClientSkill as ManagedSkillRef,
};
pub(super) use crate::public_protocol::{
    GetClientSetupRequest as GetManagedSetupRequest, GetClientSetupResponse as ManagedSetup,
};

impl ManagedSetup {
    pub(super) fn try_into_domain(self) -> Result<super::ManagedSetup, ManagedSetupError> {
        let scope = self
            .scope
            .ok_or_else(|| ManagedSetupError::Decode("missing client setup scope".into()))?;
        let issued_at = self
            .issued_at
            .map(|value| {
                if !(-62_135_596_800..=253_402_300_799).contains(&value.seconds)
                    || !(0..1_000_000_000).contains(&value.nanos)
                {
                    return Err(ManagedSetupError::Decode(
                        "invalid managed setup timestamp".to_owned(),
                    ));
                }
                Ok(super::ManagedSetupTimestamp {
                    seconds: value.seconds,
                    nanos: value.nanos,
                })
            })
            .transpose()?;
        let rules = self
            .rules
            .into_iter()
            .map(|rule| {
                let scope = match rule.scope {
                    0 | 1 => super::RuleScope::Organization,
                    2 => super::RuleScope::Workspace,
                    _ => {
                        return Err(ManagedSetupError::Decode(
                            "unknown managed rule scope".to_owned(),
                        ));
                    }
                };
                Ok(super::ManagedRule {
                    id: rule.id,
                    title: rule.title,
                    body_markdown: rule.body_markdown,
                    scope,
                })
            })
            .collect::<Result<Vec<_>, ManagedSetupError>>()?;
        let mcp = self.mcp.unwrap_or_default();
        let mode = match mcp.mode {
            0 => super::McpPolicyMode::Unspecified,
            1 => super::McpPolicyMode::Open,
            2 => super::McpPolicyMode::Allowlist,
            3 => super::McpPolicyMode::Denylist,
            _ => {
                return Err(ManagedSetupError::Decode(
                    "unknown managed MCP policy mode".to_owned(),
                ));
            }
        };
        Ok(super::ManagedSetup {
            version: self.version,
            issued_at,
            organization_id: scope.organization_id,
            workspace_id: scope.workspace_id,
            rules,
            skills: self
                .skills
                .into_iter()
                .map(|skill| super::ManagedSkillRef {
                    id: skill.id,
                    source: skill.source,
                    version: skill.version,
                    required: skill.required,
                })
                .collect(),
            mcp: super::McpPolicy {
                mode,
                servers: mcp
                    .servers
                    .into_iter()
                    .map(|server| super::McpServerRef {
                        name: server.name,
                        url_pattern: server.url_pattern,
                        transport: server.transport,
                    })
                    .collect(),
            },
            sandbox_policy_toml: self.sandbox_policy_toml,
        })
    }
}
