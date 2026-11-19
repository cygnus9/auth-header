# auth-header

`auth-header` resolves a credential from an installed executable and prints an HTTP header value. It never invokes a shell.

## Usage

```text
auth-header github
auth-header github --hostname github.example.com
auth-header exec --scheme Basic -- company-cli auth print-token
auth-header exec --no-scheme -- secret-cli read service-key
auth-header profile internal
```

The command after `exec --` is an executable followed by its argument list. Arguments support `${NAME}` expansion from the current environment. Expansion is one argument, does not split whitespace, and fails when the variable is missing. The executable name and scheme are not expanded.

Provider stdout is normalized by removing one trailing newline. Empty output, embedded newlines, carriage returns, invalid UTF-8, nonzero exits, failed starts, and timeouts are errors. A successful invocation writes only the formatted header to stdout. Provider stderr is not copied to avoid leaking arbitrary diagnostics.

The default scheme is `Bearer`. Use `--scheme NAME` for another scheme or `--no-scheme` to print the credential unchanged. Schemes must be a single nonempty token. Existing prefixes in credentials are not detected or removed.

## GitHub

The built-in provider runs `gh auth token` directly:

```text
auth-header github
auth-header github --hostname github.example.com
```

Authenticate and manage hosts with the GitHub CLI (`gh`). `auth-header` does not implement GitHub OAuth or store tokens.

## Profiles

Profiles are stored in the platform user configuration directory:

- Linux: `${XDG_CONFIG_HOME:-~/.config}/auth-header/config.toml`
- macOS: `~/Library/Application Support/auth-header/config.toml`
- Windows: `%APPDATA%\\auth-header\\config.toml`

Example:

```toml
[profiles.internal]
type = "command"
program = "company-cli"
args = ["auth", "print-token", "--endpoint", "${SERVICE_ENDPOINT}"]
scheme = "Bearer"

[profiles.api-key]
type = "command"
program = "secret-cli"
args = ["read", "service-key"]
scheme = "none"
```

CLI scheme options override a profile scheme. A profile cannot contain a credential value; it only describes the executable and arguments.

## Failure records

Failed credential resolution writes a secret-free record under the platform user state directory:

- Linux: `${XDG_STATE_HOME:-~/.local/state}/auth-header/entries/`
- macOS: `~/Library/Logs/auth-header/entries/`
- Windows: `%LOCALAPPDATA%\\auth-header\\entries\\`

Inspect records without invoking a provider:

```text
auth-header log
auth-header log --count 20
auth-header log --all
auth-header log --id <id>
auth-header log --utc
auth-header log --path
```

List and detail timestamps default to the process environment's local timezone. Use `--utc` to select UTC. Displayed timestamps use `YYYY-MM-DD HH:MM:SS.sss ±HH:MM`; persisted records retain numeric Unix epoch-millisecond timestamps.

Records contain only an opaque ID, timestamp, version, provider, executable basename, category, error code, exit code, and duration. Credentials, generated headers, child output, environment values, and argument lists are never persisted. Up to 100 recent records are retained. If record writing fails, the original provider error and exit status are preserved.

For isolated testing, `AUTH_HEADER_CONFIG_DIR` and `AUTH_HEADER_STATE_DIR` override the configuration and state directories.

## Build

```text
cargo build --release
cargo test
```

The resulting binary is `target/release/auth-header`.
