# GitHub Copilot provider

`claude-code-proxy` can use a GitHub Copilot subscription as a provider while exposing the Anthropic Messages interface expected by Claude Code.

## Authentication

Use GitHub's device flow:

```bash
claude-code-proxy github-copilot auth login
claude-code-proxy github-copilot auth status
```

Or copy an existing reusable credential:

```bash
claude-code-proxy github-copilot copy vscode
claude-code-proxy github-copilot copy opencode
```

`import` remains an alias for `copy`.

### VS Code / GitHub Copilot sources

The provider checks reusable credentials in this order:

1. `COPILOT_GITHUB_TOKEN`
2. `GH_TOKEN`
3. `GITHUB_TOKEN`
4. `$XDG_CONFIG_HOME/github-copilot/hosts.json`
5. `$XDG_CONFIG_HOME/github-copilot/apps.json`
6. `~/.config/github-copilot/hosts.json`
7. `~/.config/github-copilot/apps.json`

OAuth/user tokens (`gho_`, `ghu_`) and fine-grained GitHub tokens (`github_pat_`) are accepted as copy candidates. Classic `ghp_` PATs are deliberately not reused.

### OpenCode source

The provider reads OpenCode's `auth.json` and looks for `github-copilot` or `github-copilot-enterprise` entries. Unix defaults to `~/.local/share/opencode/auth.json` (or `$XDG_DATA_HOME/opencode/auth.json`); Windows uses `%LOCALAPPDATA%/opencode/auth.json`.

## Models

Static model names are namespaced so they do not collide with the existing Codex provider:

```text
github-copilot:gpt-5.6-sol
github-copilot:gpt-5.6-luna
github-copilot:claude-sonnet-4.6
```

Query the live Copilot catalog after authentication:

```bash
claude-code-proxy github-copilot models
```

### `-fast` compatibility

GPT `-fast` names are no longer advertised as separate models. Legacy input such as:

```text
gpt-5.6-sol-fast
github-copilot:gpt-5.6-sol-fast
```

is normalized to the corresponding base GPT model. Non-GPT names such as Cursor or Grok models containing `-fast` are not changed.

## Transport routing

The provider chooses the Copilot upstream protocol by model family:

- GPT-5 / GPT-6 / Codex families -> `/v1/responses`
- Claude / Gemini and other chat-compatible families -> `/chat/completions`

Both transports are translated back to Anthropic Messages responses and Anthropic SSE events for Claude Code. Existing response translators in this repository are reused for tool calls, reasoning output, stop reasons, and streaming.

## Claude Code

Start the proxy:

```bash
claude-code-proxy serve
```

Then configure Claude Code, for example:

```bash
export ANTHROPIC_BASE_URL="http://127.0.0.1:18765"
export ANTHROPIC_AUTH_TOKEN="unused"
export ANTHROPIC_MODEL="github-copilot:gpt-5.6-sol"
export ANTHROPIC_SMALL_FAST_MODEL="github-copilot:gpt-5.6-luna"
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
claude
```

PowerShell:

```powershell
$env:ANTHROPIC_BASE_URL="http://127.0.0.1:18765"
$env:ANTHROPIC_AUTH_TOKEN="unused"
$env:ANTHROPIC_MODEL="github-copilot:gpt-5.6-sol"
$env:ANTHROPIC_SMALL_FAST_MODEL="github-copilot:gpt-5.6-luna"
$env:CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1"
claude
```

## Copilot client identity overrides

Defaults track the currently referenced VS Code Copilot Chat behavior. They can be overridden without rebuilding:

```text
CCP_COPILOT_CLIENT_ID
CCP_COPILOT_VSCODE_VERSION
CCP_COPILOT_PLUGIN_VERSION
CCP_COPILOT_API_VERSION
```

## Implementation references

The implementation was designed against active Copilot proxy/auth projects rather than copied verbatim. Primary references include:

- `MartinForReal/ghc-proxy` — Rust Copilot proxy with OpenAI/Anthropic/Claude Code compatibility, Responses support, token refresh, and VS Code identity headers.
- `anomalyco/opencode-copilot-auth` / OpenCode GitHub Copilot auth implementations — GitHub device auth, client identity, token exchange, and credential storage behavior.
- `cavanaug/opencode-copilot-vscode` — model-family routing and Responses API behavior for modern GPT models.

GitHub Copilot upstream endpoints are not a stable public API contract. The provider therefore keeps editor/plugin/API identity versions configurable and has live model discovery so upstream changes can be adapted without changing the Claude Code-facing API.
