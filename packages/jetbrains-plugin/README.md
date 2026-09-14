# Deixic Code JetBrains Plugin

JetBrains IDE integration for [Deixic Code](https://github.com/evalops/maestro), the AI coding assistant. The existing repository and package URLs remain compatibility coordinates.

## Features

- **AI-Powered Chat** - Streaming responses from Claude, GPT-4, Gemini, and more
- **IDE Integration** - Native access to diagnostics, definitions, and references via PSI
- **Context Management** - Pin files and inject active editor context
- **Session Management** - Switch between and resume past conversations
- **Tool Execution** - Full transparency into tool calls and results

## Supported IDEs

This plugin supports all JetBrains IDEs based on IntelliJ Platform 2024.3+:

- IntelliJ IDEA (Community & Ultimate)
- PyCharm
- WebStorm
- GoLand
- Rider
- CLion
- RubyMine
- PhpStorm
- DataGrip
- Android Studio

## Requirements

1. **Deixic Code Server** - The plugin connects to a running Deixic Code server
2. **JDK 17+** - Required for building the plugin

## Getting Started

### 1. Start the Deixic Code Server

```bash
# Install Deixic Code from the retained compatibility package
npm install -g @evalops/maestro

# Start the web server with the canonical command
deixic-code web
```

The server will start on `http://localhost:8080` by default.

### 2. Install the Plugin

#### From Marketplace (Recommended)
1. Open **Settings** → **Plugins** → **Marketplace**
2. Search for "Deixic Code"
3. Click **Install**

#### From Source
```bash
# Build the plugin
./gradlew buildPlugin

# The plugin ZIP will be in build/distributions/
```

Then install via **Settings** → **Plugins** → ⚙️ → **Install Plugin from Disk**

### 3. Configure the Plugin

1. Open **Settings** → **Tools** → **Deixic Code**
2. Set the **API Endpoint** (default: `http://localhost:8080`)
3. Choose your **Default Model** (e.g., `claude-sonnet-4-5`)

### 4. Use Deixic Code

- Open the **Deixic Code** tool window (right sidebar; its stable internal ID remains `Maestro`)
- Type your message and press **Ctrl+Enter** to send
- Use **Ctrl+Alt+C** to add files to context
- Use **Ctrl+Shift+C** to focus the Deixic Code window

## Keyboard Shortcuts

| Action | Shortcut |
|--------|----------|
| Send message | Ctrl+Enter |
| Add file to context | Ctrl+Alt+C |
| Focus Deixic Code | Ctrl+Shift+C |

## IDE-Specific Features

When connected to the Deixic Code server, the plugin provides IDE-aware tools:

| Tool | Description |
|------|-------------|
| `jetbrains_get_diagnostics` | Get errors/warnings from code analysis |
| `jetbrains_get_definition` | Navigate to symbol definitions |
| `jetbrains_find_references` | Find all usages of a symbol |
| `jetbrains_read_file_range` | Read specific line ranges |

## Development

### Status

The JetBrains plugin is kept as an active package. Pull requests that touch
`packages/jetbrains-plugin/`, the shared headless proto, or the JetBrains
GitHub Actions `jetbrains-plugin` job, which executes
`./gradlew check buildPlugin --no-daemon`.

### Build

```bash
# Build the plugin
./gradlew buildPlugin

# Run tests
./gradlew test

# Run IDE with plugin installed (for testing)
./gradlew runIde
```

### Project Structure

```
src/main/kotlin/com/evalops/maestro/
├── api/                    # HTTP/SSE client and data models
├── services/               # Application and project services
├── settings/               # Plugin settings
├── toolwindow/             # UI components
├── tools/                  # PSI-based tool executors
└── actions/                # IDE actions
```

### Dependencies

- **OkHttp** - HTTP client with SSE support
- **Gson** - JSON serialization
- **Kotlin Coroutines** - Async operations

## Troubleshooting

### "Disconnected" Status

1. Ensure the Deixic Code server is running: `deixic-code web`
2. Check the API endpoint in settings matches the server URL
3. Look for errors in **Help** → **Diagnostic Tools** → **Debug Log**

### IDE-Specific Tools Not Working

The PSI-based tools require:
1. The file must be within the project
2. Language support must be available (e.g., Java plugin for Java files)
3. Indexing must be complete

### Plugin Build Errors

Ensure you have:
- JDK 17 or later
- Gradle 8.11+
- Internet access for dependency downloads

## License

MIT License - see [LICENSE](../../LICENSE) for details.
