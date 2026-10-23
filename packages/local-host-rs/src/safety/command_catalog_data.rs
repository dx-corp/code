//! Declarative command knowledge, compiled with its safety-policy owner.

pub(super) const DEFINITION: &str = r#"# Embedded at build time; workspace files cannot extend approval authority.
# Names recognize tools. Only exact probes and the read grammars below grant
# read-only status. All other arguments retain approval with an operation reason.
# gh semantics: https://cli.github.com/manual/ ; Python: https://docs.python.org/3/using/cmdline.html

[[commands]]
names = ["gh"]
reason = "GitHub operation may change remote state, access credentials, or execute extensions; review its arguments"
probes = [["--version"], ["--help"]]
global_value_flags = ["-R", "--repo"]
network = true

[[commands]]
names = ["python", "python2", "python2.7", "python3", "python3.8", "python3.9", "python3.10", "python3.11", "python3.12", "python3.13", "python3.14", "python3.15", "pypy", "pypy3"]
reason = "Python executes code from a script, module, inline program, or stdin; review that code's effects"
probes = [["--version"], ["-V"], ["-VV"], ["--help"], ["-h"], ["--help-all"], ["--help-env"], ["--help-xoptions"]]

[[commands]]
names = ["node", "nodejs", "bun", "deno", "ruby", "perl", "php", "lua", "luajit", "R", "Rscript", "julia", "sh", "bash", "zsh", "fish", "pwsh"]
reason = "Interpreter or shell may execute code, access files, or start other processes"

[[commands]]
names = ["cargo", "rustc", "rustup", "go", "gcc", "g++", "clang", "clang++", "cc", "c++", "javac", "java", "swift", "dotnet", "make", "cmake", "ninja", "bazel", "gradle", "mvn", "ant", "just", "task"]
reason = "Build tool may execute project code, hooks, or plugins and write build artifacts"

[[commands]]
names = ["npm", "npx", "pnpm", "pnpx", "yarn", "corepack", "pip", "pip3", "pipx", "uv", "uvx", "poetry", "pdm", "conda", "mamba", "brew", "apt", "apt-get", "dpkg", "dnf", "yum", "pacman", "apk", "gem", "bundle", "composer"]
reason = "Package tool may install or remove software, run hooks, or execute downloaded code"

[[commands]]
names = ["pytest", "ruff", "mypy", "black", "isort", "eslint", "prettier", "tsc", "jest", "vitest", "playwright", "tox", "nox"]
reason = "Development tool may execute project code or plugins, or rewrite files"

[[commands]]
names = ["docker", "podman", "nerdctl", "kubectl", "helm", "kustomize", "terraform", "tofu", "pulumi", "ansible", "ansible-playbook", "aws", "gcloud", "az", "doctl", "flyctl", "vercel", "wrangler"]
reason = "Infrastructure tool may change remote resources, expose credentials, or execute workloads"
network = true

[[commands]]
names = ["ssh", "sftp", "nc", "netcat", "socat", "openssl", "http", "https", "aria2c"]
reason = "Network tool may transfer data, access credentials, write files, or execute remote commands"
network = true

[[commands]]
names = ["touch", "mkdir", "install", "ln", "truncate", "patch", "apply_patch", "split", "csplit"]
reason = "Filesystem operation may create, overwrite, or alter files"

[[commands]]
names = ["rg", "fd", "fdfind", "jq", "ps", "uptime", "free", "lscpu", "lsmem", "nproc", "getconf", "basename", "dirname", "realpath", "readlink", "sha256sum", "sha512sum", "md5sum", "b2sum", "strings"]
reason = "Inspection command uses arguments outside its reviewed read-only forms"
probes = [["--version"], ["--help"]]

# Build/package/interpreter tools remain recognized above, but their version
# dispatch can load repository hooks or select/download another toolchain.
# Do not assume '--version' is universally a side-effect-free probe.

# GitHub built-in reads. API calls, aliases, extensions, credentials, browser
# launches, downloads, workflow execution, and mutations remain reviewable.
[[reads]]
programs = ["gh"]
prefixes = [["pr", "view"], ["issue", "view"]]
flags = ["--comments", "-c", "--help"]
value_flags = ["--json", "--jq", "-q", "--template", "-t"]
max_positionals = 1

[[reads]]
programs = ["gh"]
prefixes = [["pr", "list"], ["issue", "list"]]
flags = ["--draft", "-d", "--help"]
value_flags = ["--json", "--jq", "-q", "--template", "-t", "--limit", "-L", "--state", "-s", "--search", "-S", "--label", "-l", "--assignee", "-a", "--author", "-A", "--base", "-B", "--head", "-H", "--milestone", "-m", "--app"]

[[reads]]
programs = ["gh"]
prefixes = [["pr", "checks"]]
flags = ["--required", "--help"]
value_flags = ["--json", "--jq", "-q", "--template", "-t"]
max_positionals = 1

[[reads]]
programs = ["gh"]
prefixes = [["pr", "diff"]]
flags = ["--patch", "--name-only", "--help"]
value_flags = ["--color"]
max_positionals = 1

[[reads]]
programs = ["gh"]
prefixes = [["pr", "status"], ["issue", "status"], ["status"]]
flags = ["--help"]
value_flags = ["--json", "--jq", "-q", "--template", "-t"]

[[reads]]
programs = ["gh"]
prefixes = [["repo", "view"], ["repo", "list"], ["release", "view"], ["release", "list"]]
flags = ["--help", "--archived", "--fork", "--source", "--exclude-drafts", "--exclude-pre-releases"]
value_flags = ["--json", "--jq", "-q", "--template", "-t", "--limit", "-L", "--language", "--visibility"]
max_positionals = 1

[[reads]]
programs = ["gh"]
prefixes = [["run", "list"]]
flags = ["--all", "-a", "--help"]
value_flags = ["--json", "--jq", "-q", "--template", "-t", "--limit", "-L", "--branch", "-b", "--commit", "-c", "--created", "--event", "-e", "--status", "-s", "--user", "-u", "--workflow", "-w"]

[[reads]]
programs = ["gh"]
prefixes = [["run", "view"]]
flags = ["--log", "--log-failed", "--exit-status", "--verbose", "-v", "--help"]
value_flags = ["--json", "--jq", "-q", "--template", "-t", "--job", "-j", "--attempt", "-a"]
max_positionals = 1

[[reads]]
programs = ["gh"]
prefixes = [["workflow", "list"]]
flags = ["--all", "-a", "--help"]
value_flags = ["--json", "--jq", "-q", "--template", "-t", "--limit", "-L"]

[[reads]]
programs = ["gh"]
prefixes = [["workflow", "view"]]
flags = ["--yaml", "-y", "--help"]
value_flags = ["--ref", "-r"]
max_positionals = 1

[[reads]]
programs = ["gh"]
prefixes = [["search", "code"], ["search", "commits"], ["search", "issues"], ["search", "prs"], ["search", "repos"]]
flags = ["--help"]
value_flags = ["--json", "--jq", "-q", "--template", "-t", "--limit", "-L", "--repo", "--owner", "--language", "--filename", "--extension", "--state", "--sort", "--order", "--match"]
max_positionals = 32

[[reads]]
programs = ["rg"]
prefixes = [[]]
flags = ["-n", "--line-number", "--hidden", "--no-ignore", "--files", "-l", "--files-with-matches", "-c", "--count", "-i", "--ignore-case", "-F", "--fixed-strings", "-w", "--word-regexp", "-v", "--invert-match", "-U", "--multiline", "--no-heading", "-H", "--with-filename", "--json", "--stats", "-P", "--pcre2", "-a", "--text"]
value_flags = ["-g", "--glob", "--iglob", "-t", "--type", "-T", "--type-not", "-e", "--regexp", "-f", "--file", "-m", "--max-count", "-C", "--context", "-B", "--before-context", "-A", "--after-context", "--max-depth", "--color", "--sort", "--sortr"]
max_positionals = 128

[[reads]]
programs = ["fd", "fdfind"]
prefixes = [[]]
flags = ["--hidden", "--no-ignore", "--absolute-path", "--full-path", "--fixed-strings", "--ignore-case", "--case-sensitive", "--follow", "--print0"]
value_flags = ["-t", "--type", "-e", "--extension", "-E", "--exclude", "-d", "--max-depth", "--base-directory", "--glob", "--color"]
max_positionals = 128

[[reads]]
programs = ["jq"]
prefixes = [[]]
flags = ["-r", "--raw-output", "-c", "--compact-output", "-s", "--slurp", "-e", "--exit-status", "-n", "--null-input", "-S", "--sort-keys", "-M", "--monochrome-output", "--tab"]
value_flags = ["--indent", "-f", "--from-file"]
max_positionals = 128

[[reads]]
programs = ["uptime", "free", "lscpu", "lsmem", "nproc", "getconf", "basename", "dirname", "realpath", "readlink", "sha256sum", "sha512sum", "md5sum", "b2sum", "strings"]
prefixes = [[]]
flags = ["-a", "-e", "-f", "-l", "-h", "-m", "-g", "--all", "--human", "--zero", "--canonicalize", "--canonicalize-existing", "--canonicalize-missing"]
max_positionals = 128

# Environment-printing BSD forms such as auxe/e are intentionally excluded.
[[reads]]
programs = ["ps"]
prefixes = [[], ["aux"], ["ax"], ["-ef"]]
flags = ["-a", "-e", "-f", "-l"]
value_flags = ["-p", "--pid", "-C", "--sort"]
"#;
