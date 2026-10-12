//! Built-in connection recipes. Listing never starts a server or grants access.
//! Provider tools and credentials remain owned by the existing MCP runtime.
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "transport", rename_all = "lowercase")]
pub enum CatalogConnection {
    Http {
        url: &'static str,
    },
    Stdio {
        command: &'static str,
        args: &'static [&'static str],
    },
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct McpCatalogEntry {
    pub id: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    pub connection: CatalogConnection,
    pub documentation: &'static str,
}
impl McpCatalogEntry {
    pub fn configuration(&self) -> Value {
        match self.connection {
            CatalogConnection::Http { url } => json!({"transport": "http", "url": url}),
            CatalogConnection::Stdio { command, args } => {
                json!({"transport": "stdio", "command": command, "args": args})
            }
        }
    }
    pub fn destination(&self) -> String {
        match self.connection {
            CatalogConnection::Http { url } => url.to_string(),
            CatalogConnection::Stdio { command, args } => format!("{command} {}", args.join(" ")),
        }
    }
    pub fn matches_in(&self, locale: crate::localization::Locale, query: &str) -> bool {
        let text = format!(
            "{} {} {} {} {}",
            self.id,
            self.description,
            self.category,
            locale.translate(self.description),
            locale.translate(self.category)
        )
        .to_lowercase();
        query
            .split_whitespace()
            .all(|word| text.contains(&word.to_lowercase()))
    }

    pub fn matches(&self, query: &str) -> bool {
        let text = format!("{} {} {}", self.id, self.description, self.category).to_lowercase();
        query
            .split_whitespace()
            .all(|word| text.contains(&word.to_lowercase()))
    }
}
pub fn catalog_entries() -> &'static [McpCatalogEntry] {
    CATALOG
}
const CATALOG: &[McpCatalogEntry] = &[
    McpCatalogEntry {
        id: "context7",
        description: "Library documentation",
        category: "Research",
        connection: CatalogConnection::Stdio {
            command: "npx",
            args: &["-y", "@upstash/context7-mcp"],
        },
        documentation: "https://github.com/upstash/context7",
    },
    McpCatalogEntry {
        id: "playwright",
        description: "Browser automation",
        category: "Development",
        connection: CatalogConnection::Stdio {
            command: "npx",
            args: &["-y", "@playwright/mcp@latest"],
        },
        documentation: "https://github.com/microsoft/playwright-mcp",
    },
    McpCatalogEntry {
        id: "fetch",
        description: "Fetch web page content",
        category: "Research",
        connection: CatalogConnection::Stdio {
            command: "uvx",
            args: &["mcp-server-fetch"],
        },
        documentation: "https://github.com/modelcontextprotocol/servers/tree/main/src/fetch",
    },
    McpCatalogEntry {
        id: "time",
        description: "Time and timezone conversion",
        category: "Research",
        connection: CatalogConnection::Stdio {
            command: "uvx",
            args: &["mcp-server-time"],
        },
        documentation: "https://github.com/modelcontextprotocol/servers/tree/main/src/time",
    },
    McpCatalogEntry {
        id: "linear",
        description: "Issues and projects",
        category: "Work",
        connection: CatalogConnection::Http {
            url: "https://mcp.linear.app/mcp",
        },
        documentation: "https://factory.mintlify.app/cli/configuration/mcp",
    },
    McpCatalogEntry {
        id: "sentry",
        description: "Errors and performance",
        category: "Development",
        connection: CatalogConnection::Http {
            url: "https://mcp.sentry.dev/mcp",
        },
        documentation: "https://factory.mintlify.app/cli/configuration/mcp",
    },
    McpCatalogEntry {
        id: "notion",
        description: "Pages and workspace documents",
        category: "Work",
        connection: CatalogConnection::Http {
            url: "https://mcp.notion.com/mcp",
        },
        documentation: "https://factory.mintlify.app/cli/configuration/mcp",
    },
    McpCatalogEntry {
        id: "figma",
        description: "Design files and components",
        category: "Design",
        connection: CatalogConnection::Http {
            url: "https://mcp.figma.com/mcp",
        },
        documentation: "https://factory.mintlify.app/cli/configuration/mcp",
    },
    McpCatalogEntry {
        id: "stripe",
        description: "Payments and subscriptions",
        category: "Business",
        connection: CatalogConnection::Http {
            url: "https://mcp.stripe.com",
        },
        documentation: "https://factory.mintlify.app/cli/configuration/mcp",
    },
    McpCatalogEntry {
        id: "vercel",
        description: "Projects and deployments",
        category: "Cloud",
        connection: CatalogConnection::Http {
            url: "https://mcp.vercel.com/",
        },
        documentation: "https://factory.mintlify.app/cli/configuration/mcp",
    },
    McpCatalogEntry {
        id: "hugging-face",
        description: "Models and datasets",
        category: "Development",
        connection: CatalogConnection::Http {
            url: "https://huggingface.co/mcp",
        },
        documentation: "https://factory.mintlify.app/cli/configuration/mcp",
    },
    McpCatalogEntry {
        id: "socket",
        description: "Dependency security",
        category: "Development",
        connection: CatalogConnection::Http {
            url: "https://mcp.socket.dev/",
        },
        documentation: "https://factory.mintlify.app/cli/configuration/mcp",
    },
    McpCatalogEntry {
        id: "intercom",
        description: "Customer conversations",
        category: "Business",
        connection: CatalogConnection::Http {
            url: "https://mcp.intercom.com/mcp",
        },
        documentation: "https://factory.mintlify.app/cli/configuration/mcp",
    },
    McpCatalogEntry {
        id: "monday",
        description: "Boards and tasks",
        category: "Work",
        connection: CatalogConnection::Http {
            url: "https://mcp.monday.com/mcp",
        },
        documentation: "https://factory.mintlify.app/cli/configuration/mcp",
    },
    McpCatalogEntry {
        id: "paypal",
        description: "Commerce and payments",
        category: "Business",
        connection: CatalogConnection::Http {
            url: "https://mcp.paypal.com/mcp",
        },
        documentation: "https://factory.mintlify.app/cli/configuration/mcp",
    },
    McpCatalogEntry {
        id: "canva",
        description: "Designs and presentations",
        category: "Design",
        connection: CatalogConnection::Http {
            url: "https://mcp.canva.com/mcp",
        },
        documentation: "https://factory.mintlify.app/cli/configuration/mcp",
    },
    McpCatalogEntry {
        id: "netlify",
        description: "Sites and deployments",
        category: "Cloud",
        connection: CatalogConnection::Http {
            url: "https://netlify-mcp.netlify.app/mcp",
        },
        documentation: "https://factory.mintlify.app/cli/configuration/mcp",
    },
    McpCatalogEntry {
        id: "stytch",
        description: "Authentication configuration",
        category: "Development",
        connection: CatalogConnection::Http {
            url: "https://mcp.stytch.dev/mcp",
        },
        documentation: "https://factory.mintlify.app/cli/configuration/mcp",
    },
    McpCatalogEntry {
        id: "supabase",
        description: "Database projects and development",
        category: "Data",
        connection: CatalogConnection::Http {
            url: "https://mcp.supabase.com/mcp",
        },
        documentation: "https://supabase.com/docs/guides/ai-tools/mcp",
    },
    McpCatalogEntry {
        id: "neon",
        description: "Postgres projects and branches",
        category: "Data",
        connection: CatalogConnection::Http {
            url: "https://mcp.neon.tech/mcp",
        },
        documentation: "https://neon.com/docs/ai/neon-mcp-server",
    },
    McpCatalogEntry {
        id: "github",
        description: "Repositories and pull requests",
        category: "Development",
        connection: CatalogConnection::Http {
            url: "https://api.githubcopilot.com/mcp/",
        },
        documentation: "https://docs.github.com/en/copilot/how-tos/provide-context/use-mcp-in-your-ide/set-up-the-github-mcp-server",
    },
    McpCatalogEntry {
        id: "tavily",
        description: "Web search and extraction",
        category: "Research",
        connection: CatalogConnection::Http {
            url: "https://mcp.tavily.com/mcp",
        },
        documentation: "https://docs.tavily.com/documentation/mcp",
    },
    McpCatalogEntry {
        id: "atlassian",
        description: "Jira and Confluence",
        category: "Work",
        connection: CatalogConnection::Http {
            url: "https://mcp.atlassian.com/v1/mcp",
        },
        documentation: "https://developer.atlassian.com/cloud/rovo-mcp/changelog/",
    },
    McpCatalogEntry {
        id: "microsoft-learn",
        description: "Microsoft documentation",
        category: "Research",
        connection: CatalogConnection::Http {
            url: "https://learn.microsoft.com/api/mcp",
        },
        documentation: "https://learn.microsoft.com/en-us/training/support/mcp-developer-reference",
    },
    McpCatalogEntry {
        id: "cloudflare-docs",
        description: "Documentation",
        category: "Cloud",
        connection: CatalogConnection::Http {
            url: "https://docs.mcp.cloudflare.com/mcp",
        },
        documentation: "https://github.com/cloudflare/mcp-server-cloudflare",
    },
    McpCatalogEntry {
        id: "cloudflare-bindings",
        description: "Workers storage and compute",
        category: "Cloud",
        connection: CatalogConnection::Http {
            url: "https://bindings.mcp.cloudflare.com/mcp",
        },
        documentation: "https://github.com/cloudflare/mcp-server-cloudflare",
    },
    McpCatalogEntry {
        id: "cloudflare-builds",
        description: "Workers builds",
        category: "Cloud",
        connection: CatalogConnection::Http {
            url: "https://builds.mcp.cloudflare.com/mcp",
        },
        documentation: "https://github.com/cloudflare/mcp-server-cloudflare",
    },
    McpCatalogEntry {
        id: "cloudflare-observability",
        description: "Application logs and analytics",
        category: "Cloud",
        connection: CatalogConnection::Http {
            url: "https://observability.mcp.cloudflare.com/mcp",
        },
        documentation: "https://github.com/cloudflare/mcp-server-cloudflare",
    },
    McpCatalogEntry {
        id: "cloudflare-containers",
        description: "Development containers",
        category: "Cloud",
        connection: CatalogConnection::Http {
            url: "https://containers.mcp.cloudflare.com/mcp",
        },
        documentation: "https://github.com/cloudflare/mcp-server-cloudflare",
    },
    McpCatalogEntry {
        id: "cloudflare-browser",
        description: "Web pages and screenshots",
        category: "Cloud",
        connection: CatalogConnection::Http {
            url: "https://browser.mcp.cloudflare.com/mcp",
        },
        documentation: "https://github.com/cloudflare/mcp-server-cloudflare",
    },
    McpCatalogEntry {
        id: "cloudflare-logs",
        description: "Logpush job health",
        category: "Cloud",
        connection: CatalogConnection::Http {
            url: "https://logs.mcp.cloudflare.com/mcp",
        },
        documentation: "https://github.com/cloudflare/mcp-server-cloudflare",
    },
    McpCatalogEntry {
        id: "cloudflare-ai-gateway",
        description: "AI Gateway logs",
        category: "Cloud",
        connection: CatalogConnection::Http {
            url: "https://ai-gateway.mcp.cloudflare.com/mcp",
        },
        documentation: "https://github.com/cloudflare/mcp-server-cloudflare",
    },
    McpCatalogEntry {
        id: "cloudflare-autorag",
        description: "AutoRAG search",
        category: "Cloud",
        connection: CatalogConnection::Http {
            url: "https://autorag.mcp.cloudflare.com/mcp",
        },
        documentation: "https://github.com/cloudflare/mcp-server-cloudflare",
    },
    McpCatalogEntry {
        id: "cloudflare-auditlogs",
        description: "Account audit logs",
        category: "Cloud",
        connection: CatalogConnection::Http {
            url: "https://auditlogs.mcp.cloudflare.com/mcp",
        },
        documentation: "https://github.com/cloudflare/mcp-server-cloudflare",
    },
    McpCatalogEntry {
        id: "cloudflare-dns-analytics",
        description: "DNS analytics",
        category: "Cloud",
        connection: CatalogConnection::Http {
            url: "https://dns-analytics.mcp.cloudflare.com/mcp",
        },
        documentation: "https://github.com/cloudflare/mcp-server-cloudflare",
    },
    McpCatalogEntry {
        id: "cloudflare-dex",
        description: "Application experience monitoring",
        category: "Cloud",
        connection: CatalogConnection::Http {
            url: "https://dex.mcp.cloudflare.com/mcp",
        },
        documentation: "https://github.com/cloudflare/mcp-server-cloudflare",
    },
    McpCatalogEntry {
        id: "cloudflare-casb",
        description: "SaaS security configuration",
        category: "Cloud",
        connection: CatalogConnection::Http {
            url: "https://casb.mcp.cloudflare.com/mcp",
        },
        documentation: "https://github.com/cloudflare/mcp-server-cloudflare",
    },
    McpCatalogEntry {
        id: "cloudflare-radar",
        description: "Internet traffic insights",
        category: "Cloud",
        connection: CatalogConnection::Http {
            url: "https://radar.mcp.cloudflare.com/mcp",
        },
        documentation: "https://github.com/cloudflare/mcp-server-cloudflare",
    },
    McpCatalogEntry {
        id: "cloudflare-blog",
        description: "Cloudflare blog posts",
        category: "Cloud",
        connection: CatalogConnection::Http {
            url: "https://blog.mcp.cloudflare.com/mcp",
        },
        documentation: "https://github.com/cloudflare/mcp-server-cloudflare",
    },
    McpCatalogEntry {
        id: "cloudflare",
        description: "Cloudflare API",
        category: "Cloud",
        connection: CatalogConnection::Http {
            url: "https://mcp.cloudflare.com/mcp",
        },
        documentation: "https://github.com/cloudflare/mcp",
    },
];
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_has_unique_ids_and_safe_destinations() {
        let mut ids = std::collections::HashSet::new();
        assert!(catalog_entries().len() >= 40);
        for entry in catalog_entries() {
            assert!(ids.insert(entry.id));
            assert!(entry.documentation.starts_with("https://"));
            let value = entry.configuration();
            assert!(value.get("headers").is_none());
            assert!(value.get("env").is_none());
            if let CatalogConnection::Http { url } = entry.connection {
                let url = reqwest::Url::parse(url).unwrap();
                assert_eq!(url.scheme(), "https");
                assert!(url.username().is_empty());
                assert!(url.password().is_none());
                assert_eq!(value["transport"], "http");
            } else {
                assert_eq!(value["transport"], "stdio");
            }
        }
    }
    #[test]
    fn search_matches_all_words_across_name_category_and_description() {
        let linear = catalog_entries().iter().find(|e| e.id == "linear").unwrap();
        assert!(linear.matches("LINEAR issues"));
        assert!(!linear.matches("linear database"));
    }
}

#[cfg(test)]
mod localized_search_tests {
    use super::*;
    use crate::localization::Locale;
    #[test]
    fn translated_catalog_search_does_not_change_connection_configuration() {
        for entry in catalog_entries() {
            let config = entry.configuration();
            for locale in Locale::ALL {
                assert!(
                    entry.matches_in(locale, &locale.translate(entry.description).to_uppercase())
                );
                assert_eq!(entry.configuration(), config);
            }
        }
    }
}
