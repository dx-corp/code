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
interspersed_globals = true
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
short_bundles = true
option_terminator = true
prefixes = [[]]
flags = ["-n", "--line-number", "--hidden", "--no-ignore", "--files", "-l", "--files-with-matches", "-c", "--count", "-i", "--ignore-case", "-F", "--fixed-strings", "-w", "--word-regexp", "-v", "--invert-match", "-U", "--multiline", "--no-heading", "-H", "--with-filename", "--json", "--stats", "-P", "--pcre2", "-a", "--text"]
value_flags = ["-g", "--glob", "--iglob", "-t", "--type", "-T", "--type-not", "-e", "--regexp", "-f", "--file", "-m", "--max-count", "-C", "--context", "-B", "--before-context", "-A", "--after-context", "--max-depth", "--color", "--sort", "--sortr"]
max_positionals = 128

[[reads]]
programs = ["fd", "fdfind"]
short_bundles = true
option_terminator = true
prefixes = [[]]
flags = ["-H", "-I", "-a", "-p", "-F", "-i", "-s", "-L", "-0", "-g", "--glob", "--hidden", "--no-ignore", "--absolute-path", "--full-path", "--fixed-strings", "--ignore-case", "--case-sensitive", "--follow", "--print0"]
value_flags = ["-t", "--type", "-e", "--extension", "-E", "--exclude", "-d", "--max-depth", "--base-directory", "--color"]
max_positionals = 128

[[reads]]
programs = ["jq"]
short_bundles = true
option_terminator = true
prefixes = [[]]
flags = ["-r", "--raw-output", "-c", "--compact-output", "-s", "--slurp", "-e", "--exit-status", "-n", "--null-input", "-S", "--sort-keys", "-M", "--monochrome-output", "--tab"]
value_flags = ["--indent", "-f", "--from-file"]
pair_value_flags = ["--arg", "--argjson", "--rawfile", "--slurpfile"]
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

[[commands]]
names = ["git"]
reason = "Git arguments may write files or repository state, access the network, or execute configured helpers"
probes = [["--version"]]
global_flags = ["--no-pager", "--no-lazy-fetch"]
network_guard_prefix = ["--no-lazy-fetch"]
global_value_flags = ["-C"]
global_literal_options = [["-c", "core.fsmonitor=false"]]
network = true
local_reads = true

[[commands]]
names = ["cat", "head", "tail", "grep", "ag", "find", "locate", "ls", "pwd", "tree", "exa", "eza", "echo", "printf", "wc", "sort", "uniq", "diff", "cut", "tr", "file", "stat", "du", "df", "which", "whereis", "type", "date", "cal", "whoami", "id", "groups", "hostname", "uname", "env", "printenv", "test", "[", "true", "false"]
reason = "Inspection arguments may write output, change system state, or invoke helpers; review this form"

[[commands]]
names = ["awk", "gawk", "mawk", "nawk", "sed", "gsed", "yq"]
reason = "Text processor may execute embedded programs, load extensions, or write files; review the program and options"

[[commands]]
names = ["less", "more", "bat", "man", "info", "help", "fzf"]
reason = "Interactive viewer may launch a pager, execute commands, or load configured helpers"

[[commands]]
names = ["command", "builtin", "eval", "exec", "source", ".", "nice", "nohup", "timeout", "time", "watch", "xargs", "parallel"]
reason = "Shell dispatcher or wrapper may execute another program, change its environment, or write output"

[[commands]]
names = ["sqlite3", "psql", "mysql", "mariadb", "mongosh", "redis-cli", "duckdb", "clickhouse", "influx"]
reason = "Database client may execute queries or startup scripts, change data, or expose credentials"
network = true

[[commands]]
names = ["dig", "host", "nslookup", "ping", "ping6", "traceroute", "tracepath", "mtr", "telnet", "ftp", "rsync", "scp", "curl", "wget"]
reason = "Network operation may contact remote systems, transfer data, or write files"
network = true

[[commands]]
names = ["tar", "zip", "unzip", "gzip", "gunzip", "bzip2", "bunzip2", "xz", "unxz", "zstd", "unzstd", "7z", "7za", "cpio"]
reason = "Archive operation may create, extract, overwrite, or delete files"

[[commands]]
names = ["sbt", "mix", "elixir", "erl", "clojure", "lein", "dune", "ocaml", "ghc", "cabal", "stack", "zig", "meson"]
reason = "Language or build tool may execute project code and write artifacts"

[[commands]]
names = ["molecule", "hatch", "coverage", "pylint", "pyright", "biome", "tap", "mocha", "ava", "cucumber"]
reason = "Development tool may execute project code or plugins, or rewrite files"

[[reads]]
programs = ["cat"]
prefixes = [[]]
flags = ["-A", "-b", "-e", "-E", "-n", "-s", "-t", "-T", "-u", "-v", "--show-all", "--number", "--squeeze-blank"]
value_flags = []
max_positionals = 128
short_bundles = true
option_terminator = true

[[reads]]
programs = ["head", "tail"]
numeric_short_flag = true
prefixes = [[]]
flags = ["-q", "-v", "--quiet", "--verbose", "-f", "--follow"]
value_flags = ["-n", "--lines", "-c", "--bytes"]
max_positionals = 128
short_bundles = true
option_terminator = true

[[reads]]
programs = ["grep"]
prefixes = [[]]
flags = ["-E", "-F", "-G", "-P", "-i", "-v", "-w", "-x", "-c", "-l", "-L", "-n", "-h", "-H", "-o", "-q", "-s", "-r", "-R", "-a", "-I", "--line-number", "--ignore-case", "--fixed-strings", "--recursive"]
value_flags = ["-e", "--regexp", "-f", "--file", "-m", "--max-count", "-A", "--after-context", "-B", "--before-context", "-C", "--context", "--include", "--exclude"]
max_positionals = 128
short_bundles = true
option_terminator = true

[[reads]]
programs = ["ag"]
prefixes = [[]]
flags = ["-i", "-s", "-w", "-v", "-l", "-L", "-c", "-n", "-H", "-h", "-a", "--hidden", "--nocolor", "--nogroup"]
value_flags = ["-G", "--file-search-regex", "-g", "--filename-pattern", "--ignore"]
max_positionals = 128
short_bundles = true
option_terminator = true

[[reads]]
programs = ["find"]
prefixes = [[]]
flags = ["-H", "-L", "-P", "-print", "-print0", "-ls", "-empty", "-readable", "-writable", "-executable", "-prune", "-quit", "-true", "-false", "-a", "-and", "-o", "-or", "-not", "!", "(", ")"]
value_flags = ["-name", "-iname", "-path", "-ipath", "-regex", "-iregex", "-type", "-maxdepth", "-mindepth", "-mtime", "-mmin", "-size", "-newer", "-printf", "-files0-from"]
max_positionals = 128

[[reads]]
programs = ["ls"]
prefixes = [[]]
flags = ["-a", "-A", "-l", "-h", "-R", "-t", "-r", "-S", "-d", "-F", "-i", "-n", "-1", "--all", "--almost-all", "--human-readable", "--recursive"]
value_flags = ["--color", "--sort", "--time-style"]
max_positionals = 128
short_bundles = true
option_terminator = true

[[reads]]
programs = ["tree"]
prefixes = [[]]
flags = ["-a", "-d", "-f", "-l", "-i", "-n", "-p", "-s", "-h", "-D", "-F", "--dirsfirst"]
value_flags = ["-L", "-P", "-I"]
max_positionals = 128
short_bundles = true
option_terminator = true

[[reads]]
programs = ["exa", "eza"]
prefixes = [[]]
flags = ["-a", "-A", "-l", "-h", "-R", "-T", "-d", "-F", "-i", "-1", "--all", "--long", "--tree", "--git", "--icons"]
value_flags = ["--level", "--sort", "--color"]
max_positionals = 128
short_bundles = true
option_terminator = true

[[reads]]
programs = ["pwd"]
prefixes = [[]]
flags = ["-L", "-P"]
value_flags = []
max_positionals = 0

[[reads]]
programs = ["echo"]
prefixes = [[]]
flags = ["-n", "-e", "-E"]
value_flags = []
max_positionals = 128
short_bundles = true
option_terminator = false

[[reads]]
programs = ["printf"]
prefixes = [[]]
flags = []
value_flags = []
max_positionals = 128

[[reads]]
programs = ["wc"]
prefixes = [[]]
flags = ["-c", "-m", "-l", "-w", "-L", "--bytes", "--chars", "--lines", "--words", "--max-line-length"]
value_flags = []
max_positionals = 128
short_bundles = true
option_terminator = true

[[reads]]
programs = ["sort"]
prefixes = [[]]
flags = ["-n", "-r", "-f", "-b", "-d", "-g", "-h", "-i", "-M", "-s", "-u", "-V", "-c", "-C", "-z", "--numeric-sort", "--reverse", "--unique", "--check"]
value_flags = ["-k", "--key", "-t", "--field-separator"]
max_positionals = 128
short_bundles = true
option_terminator = true

[[reads]]
programs = ["uniq"]
prefixes = [[]]
flags = ["-c", "-d", "-u", "-i", "-z", "--count", "--repeated", "--unique"]
value_flags = ["-f", "--skip-fields", "-s", "--skip-chars", "-w", "--check-chars"]
max_positionals = 1
short_bundles = true
option_terminator = true

[[reads]]
programs = ["diff"]
prefixes = [[]]
flags = ["-u", "-r", "-q", "-s", "-w", "-b", "-B", "-i", "-N", "--unified", "--recursive", "--brief", "--ignore-all-space"]
value_flags = ["-U", "--exclude"]
max_positionals = 2
short_bundles = true
option_terminator = true

[[reads]]
programs = ["cut"]
prefixes = [[]]
flags = ["-s", "-z", "--only-delimited", "--complement"]
value_flags = ["-b", "--bytes", "-c", "--characters", "-d", "--delimiter", "-f", "--fields", "--output-delimiter"]
max_positionals = 128
short_bundles = true
option_terminator = true

[[reads]]
programs = ["tr"]
prefixes = [[]]
flags = ["-c", "-C", "-d", "-s", "--complement", "--delete", "--squeeze-repeats"]
value_flags = []
max_positionals = 2
short_bundles = true
option_terminator = true

[[reads]]
programs = ["file"]
prefixes = [[]]
flags = ["-b", "-i", "-L", "-h", "--brief", "--mime", "--mime-type", "--mime-encoding"]
value_flags = ["-F", "--separator"]
max_positionals = 128
short_bundles = true
option_terminator = true

[[reads]]
programs = ["stat"]
prefixes = [[]]
flags = ["-L", "-f", "--dereference"]
value_flags = ["-c", "--format", "--printf"]
max_positionals = 128
option_terminator = true

[[reads]]
programs = ["du"]
prefixes = [[]]
flags = ["-a", "-s", "-h", "-k", "-m", "-c", "-L", "-P", "-x", "--all", "--summarize", "--human-readable"]
value_flags = ["-d", "--max-depth", "--block-size", "--exclude"]
max_positionals = 128
short_bundles = true
option_terminator = true

[[reads]]
programs = ["df"]
prefixes = [[]]
flags = ["-a", "-h", "-H", "-i", "-k", "-m", "-P", "-T", "--all", "--human-readable"]
value_flags = ["-t", "--type", "-x", "--exclude-type"]
max_positionals = 128
short_bundles = true
option_terminator = true

[[reads]]
programs = ["which", "whereis", "type"]
prefixes = [[]]
flags = ["-a", "-p", "-P", "-t", "-b", "-m", "-s"]
value_flags = []
max_positionals = 128
short_bundles = true

[[reads]]
programs = ["date"]
prefixes = [[]]
flags = ["-u", "--utc", "--universal", "-R", "--rfc-email"]
value_flags = []
max_positionals = 1
positional_prefix = "+"

[[reads]]
programs = ["cal"]
prefixes = [[]]
flags = ["-1", "-3", "-j", "-y"]
value_flags = []
max_positionals = 2

[[reads]]
programs = ["whoami", "env"]
prefixes = [[]]
flags = []
value_flags = []
max_positionals = 0

[[reads]]
programs = ["id"]
prefixes = [[]]
flags = ["-u", "-g", "-G", "-n", "-r"]
value_flags = []
max_positionals = 1
short_bundles = true

[[reads]]
programs = ["groups"]
prefixes = [[]]
flags = []
value_flags = []
max_positionals = 32

[[reads]]
programs = ["hostname"]
prefixes = [[]]
flags = ["-s", "-f", "-d", "-i", "-I"]
value_flags = []
max_positionals = 0

[[reads]]
programs = ["uname"]
prefixes = [[]]
flags = ["-a", "-s", "-n", "-r", "-v", "-m", "-p", "-i", "-o"]
value_flags = []
max_positionals = 0
short_bundles = true

[[reads]]
programs = ["printenv"]
prefixes = [[]]
flags = ["-0"]
value_flags = []
max_positionals = 32

[[reads]]
programs = ["true", "false"]
prefixes = [[]]
flags = []
value_flags = []
max_positionals = 0

[[reads]]
programs = ["git"]
prefixes = [["status"]]
required_global_options = [["-c", "core.fsmonitor=false"]]
flags = ["-s", "--short", "-b", "--branch", "--porcelain", "--porcelain=1", "--porcelain=2", "--porcelain=v1", "--porcelain=v2", "--ignored", "--show-stash", "-z", "--no-renames"]
value_flags = ["--untracked-files", "--ignore-submodules"]
max_positionals = 128
short_bundles = true
option_terminator = true

[[reads]]
programs = ["git"]
prefixes = [["log"], ["reflog", "show"], ["stash", "list"]]
numeric_short_flag = true
flags = ["--oneline", "--format=oneline", "--format=short", "--format=medium", "--format=full", "--format=fuller", "--format=reference", "--format=raw", "--format=%H", "--format=%h", "--format=%s", "--no-show-signature", "--all", "--branches", "--tags", "--remotes", "--decorate", "--no-decorate", "--graph", "--reverse", "--first-parent", "--no-merges", "--merges", "--date-order", "--topo-order", "--no-walk", "--no-ext-diff", "--no-textconv"]
value_flags = ["-n", "--max-count", "--skip", "--since", "--until", "--author", "--committer", "--grep", "--date"]
max_positionals = 128
option_terminator = true

required_flags = ["--no-show-signature"]
required_any_flags = ["--oneline", "--format=oneline", "--format=short", "--format=medium", "--format=full", "--format=fuller", "--format=reference", "--format=raw", "--format=%H", "--format=%h", "--format=%s"]
[[reads]]
programs = ["git"]
prefixes = [["diff"]]
required_global_options = [["-c", "core.fsmonitor=false"]]
required_flags = ["--no-ext-diff", "--no-textconv"]
flags = ["--stat", "--numstat", "--shortstat", "--name-only", "--name-status", "--raw", "--summary", "--check", "--cached", "--staged", "--quiet", "--exit-code", "--no-ext-diff", "--no-textconv", "--no-renames", "--no-color", "-w", "--ignore-all-space"]
value_flags = ["--diff-filter", "-U", "--unified"]
max_positionals = 128
option_terminator = true

[[reads]]
programs = ["git"]
prefixes = [["show"]]
required_flags = ["--no-ext-diff", "--no-textconv", "--no-show-signature"]
flags = ["--oneline", "--format=oneline", "--format=short", "--format=medium", "--format=full", "--format=fuller", "--format=reference", "--format=raw", "--format=%H", "--format=%h", "--format=%s", "--no-show-signature", "--stat", "--numstat", "--shortstat", "--name-only", "--name-status", "--raw", "--summary", "--no-patch", "--no-ext-diff", "--no-textconv", "--no-color"]
value_flags = []
max_positionals = 128
option_terminator = true

required_any_flags = ["--oneline", "--format=oneline", "--format=short", "--format=medium", "--format=full", "--format=fuller", "--format=reference", "--format=raw", "--format=%H", "--format=%h", "--format=%s"]
[[reads]]
programs = ["git"]
prefixes = [["branch"]]
flags = ["--no-sort", "-a", "-r", "-v", "--all", "--remotes", "--verbose", "--no-color"]
value_flags = []
max_positionals = 0
short_bundles = true

required_flags = ["--no-sort"]
[[reads]]
programs = ["git"]
prefixes = [["branch", "--list"], ["branch", "-l"]]
flags = ["--no-sort", "-a", "-r", "-v", "--all", "--remotes", "--verbose", "--no-color"]
value_flags = []
max_positionals = 128
short_bundles = true

required_flags = ["--no-sort"]
[[reads]]
programs = ["git"]
prefixes = [["tag"]]
flags = ["--no-sort"]
value_flags = []
max_positionals = 0

required_flags = ["--no-sort"]
[[reads]]
programs = ["git"]
prefixes = [["tag", "--list"], ["tag", "-l"]]
flags = ["--no-sort", "-n", "--ignore-case"]
value_flags = []
max_positionals = 128

required_flags = ["--no-sort"]
[[reads]]
programs = ["git"]
prefixes = [["remote"]]
flags = ["-v", "--verbose"]
value_flags = []
max_positionals = 0

[[reads]]
programs = ["git"]
prefixes = [["remote", "get-url"]]
flags = ["--all", "--push"]
value_flags = []
max_positionals = 1
min_positionals = 1

[[reads]]
programs = ["git"]
prefixes = [["config"]]
flags = ["--get", "--get-all", "--show-origin", "--show-scope", "--local", "--global", "--system"]
value_flags = []
max_positionals = 1
min_positionals = 1

[[reads]]
programs = ["git"]
prefixes = [["config", "--list"], ["config", "-l"]]
flags = ["--list", "-l", "--show-origin", "--show-scope", "--local", "--global", "--system"]
value_flags = []
max_positionals = 0

[[reads]]
programs = ["git"]
prefixes = [["describe"]]
flags = ["--tags", "--all", "--always", "--long", "--exact-match"]
value_flags = ["--abbrev", "--match", "--exclude"]
max_positionals = 1

[[reads]]
programs = ["git"]
prefixes = [["ls-files"]]
required_global_options = [["-c", "core.fsmonitor=false"]]
flags = ["-c", "-d", "-m", "-o", "-i", "-s", "-u", "-t", "-v", "-z", "--cached", "--deleted", "--modified", "--others", "--ignored", "--stage", "--unmerged", "--exclude-standard"]
value_flags = ["--exclude"]
max_positionals = 128
short_bundles = true
option_terminator = true

[[reads]]
programs = ["git"]
prefixes = [["ls-tree"]]
flags = ["-r", "-t", "-d", "-l", "-z", "--name-only", "--name-status", "--full-name", "--full-tree"]
value_flags = []
max_positionals = 128
short_bundles = true
option_terminator = true
min_positionals = 1

[[reads]]
programs = ["git"]
prefixes = [["rev-parse"]]
flags = ["--verify", "--quiet", "--short", "--show-toplevel", "--show-prefix", "--show-cdup", "--git-dir", "--absolute-git-dir", "--is-inside-work-tree", "--is-bare-repository", "--is-shallow-repository", "--show-object-format", "--symbolic", "--symbolic-full-name", "--abbrev-ref"]
value_flags = []
max_positionals = 128

[[reads]]
programs = ["git"]
prefixes = [["show-ref"]]
flags = ["--heads", "--tags", "--verify", "--quiet", "--hash", "--dereference"]
value_flags = []
max_positionals = 128

[[reads]]
programs = ["git"]
prefixes = [["merge-base"]]
flags = ["-a", "--all", "--is-ancestor", "--octopus", "--independent"]
value_flags = []
max_positionals = 32
min_positionals = 2

[[reads]]
programs = ["git"]
prefixes = [["branch", "--show-current"]]
max_positionals = 0
"#;
