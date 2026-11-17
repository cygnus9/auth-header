# auth-header Implementation Plan

## 1. Goal

Create a small cross-platform Rust CLI that resolves a credential from a provider or command and prints a formatted HTTP header value.

Primary use case:

```text
auth-header github
```

which runs `gh auth token` and prints:

```text
Bearer <token>
```

This makes omp configuration portable across Windows, Linux, and macOS:

```json
"Authorization": "!auth-header github"
```

## 2. Scope

### MVP includes

- Rust command-line application named `auth-header`
- Built-in GitHub provider using the installed `gh` executable
- GitHub Enterprise hostname selection
- Generic executable-plus-arguments provider
- Optional named profiles in the tool's own TOML configuration
- Configurable authentication scheme, defaulting to `Bearer`
- Strict stdout/stderr/exit-status contract
- Failure-only, secret-free diagnostic records
- Listing and inspection of recent failures
- Cross-platform executable lookup and process spawning without a shell
- Portable `${NAME}` environment expansion in command arguments for `exec` and profiles

### Explicitly out of scope for MVP

- Direct GitHub OAuth or GitHub API authentication
- Token caching
- Token storage or secret management
- Arbitrary shell command strings
- Automatic detection of an already-prefixed token
- Multi-header output
- Provider implementations beyond GitHub and generic commands

## 3. CLI contract

### Built-in GitHub provider

```text
auth-header github
auth-header github --hostname github.example.com
```

The provider invokes `gh auth token`, adding `--hostname` when requested.

### Generic command provider

```text
auth-header exec --scheme Bearer -- gh auth token
auth-header exec --scheme Basic -- company-cli auth print-token
auth-header exec --no-scheme -- secret-cli read service-key
```

The scheme options must appear before the `--` command separator. `--scheme VALUE` prefixes the normalized credential with `VALUE` and a space. `--no-scheme` emits the normalized credential unchanged. If neither option is provided, the default is `Bearer`. `--scheme` and `--no-scheme` are mutually exclusive.

The command after `--` is executed directly as an executable and argument list. No shell is involved by default.
Command arguments may contain `${NAME}` placeholders. `auth-header` expands them from the current process environment after argument parsing. Expansion is explicit and portable; it does not invoke a shell, split values on whitespace, support defaults, perform command substitution, or recurse. A missing variable is an error. Commands that already read environment variables themselves continue to receive the inherited environment unchanged.

### Named profile

```text
auth-header profile github
auth-header profile internal
```

### Diagnostics

```text
auth-header log
auth-header log --count 20
auth-header log --all
auth-header log --id <id>
auth-header log --path
```

`auth-header log` lists the ten most recent retained failure records by default. `--count N` changes the number, `--all` lists every retained record, and `--id` prints the complete record for one failure. The command must not invoke the configured provider.

### General behavior

- stdout contains only the final header value on success
- stderr contains human-readable diagnostics on failure
- zero exit status means success
- nonzero exit status means failure
- errors are never written to stdout
- `--version` is supported

## 4. Resolution pipeline

Implement the data flow as:

```text
provider/profile
    -> executable + argument list
    -> expand `${NAME}` in arguments
    -> child-process stdout
    -> normalize and validate raw credential
    -> apply authentication scheme
    -> write final header value to stdout
```

Normalization and formatting rules:

- accept one trailing newline from the child process
- trim the trailing newline without broadly trimming credential content
- reject empty output
- reject carriage returns and embedded newlines
- reject failed child-process exit status
- never auto-detect or remove an existing `Bearer`/`Token` prefix
- validate a scheme as one nonempty token without whitespace or control characters
- emit `<scheme> <credential>` when a scheme is configured
- emit only the credential when `--no-scheme` or `scheme = "none"` is selected

Environment expansion applies to command arguments, not the executable name or authentication scheme. Expanded values remain a single argument even when they contain whitespace.

The generic command provider should use Rust process APIs directly. It must not invoke `sh`, `bash`, PowerShell, or `cmd.exe` unless a future explicitly opt-in shell mode is designed.

## 5. Providers and profiles

### Built-in GitHub provider

Keep GitHub behavior as a convenience provider backed by the generic command execution path. Do not duplicate process-execution logic.

The direct GitHub API is intentionally not part of this project. `gh` already owns GitHub login, host selection, and credential storage.

### Profile configuration

Profiles belong to `auth-header`, not to omp. Do not parse or modify omp configuration.

Proposed TOML shape:

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

`scheme` is either a scheme token such as `"Bearer"` or `"Basic"`, or `"none"` to emit the raw credential. Profiles may use `${NAME}` in command arguments for environment-dependent values, for example `${SERVICE_ENDPOINT}`. Profiles must not store secret values as part of the MVP.

Environment variables are already inherited by child processes. Expansion is intended for values that must be passed as command arguments, such as a variable endpoint. Passing a secret through an argument can expose it in process listings; commands should read secrets directly from their environment whenever possible.

Configuration precedence:

1. Explicit CLI options
2. Named profile values
3. Built-in provider defaults

Use platform-standard per-user configuration directories, with XDG behavior on Linux and the conventional application-data directory on Windows/macOS.

## 6. Failure logging

### Output contract

Successful provider execution must not emit diagnostics on stdout. Immediate errors go to stderr, for example:

```text
auth-header: executable `gh` was not found on PATH
hint: run `auth-header log` for details
```

On any failed credential resolution, write one diagnostic record for that failed invocation. Do not write a record for successful resolution. Child-process stderr must not be copied blindly because arbitrary commands can accidentally emit credentials.

The failure record is for failures in `auth-header` or its credential source invocation. If a header is successfully produced but the downstream HTTP consumer later rejects it, `auth-header` cannot observe or diagnose that failure.

### Log command

`auth-header log` lists recent failures in descending timestamp order. Each list entry contains:

- short opaque identifier
- timestamp
- provider or profile
- failure category

`auth-header log --id <id>` prints the complete safe record for one entry. The identifier is the supported lookup mechanism; users do not need to find or provide a filesystem path.

Use these top-level categories:

- `invocation`: the configured credential source could not be started, timed out, or returned a nonzero status
- `tool`: `auth-header` rejected configuration or source output, or encountered an internal tool error

### Log locations and retention

Use a per-user state directory:

- Linux: `$XDG_STATE_HOME/auth-header/`, falling back to `~/.local/state/auth-header/`
- macOS: `~/Library/Logs/auth-header/`
- Windows: `%LOCALAPPDATA%\\auth-header\\`

Store one file per failure under a private entries directory. Use the opaque record ID as the lookup key, not the displayed filesystem path. Retain a bounded number of recent entries so repeated failures cannot grow storage without limit; `--all` means all currently retained entries.

Write records atomically where practical. If logging fails, it must not replace or hide the original provider error and must not change the provider result.

### Log contents

Permitted fields:

- short record ID
- timestamp
- auth-header version
- provider/profile name
- executable basename
- failure category
- specific error code
- child exit code, when available
- duration

Never log:

- raw credentials
- generated header values
- child stdout
- child stderr
- environment variables
- unredacted argument lists

Create log files with restrictive user-only permissions where supported.

## 7. Error categories

Define stable internal categories for useful diagnostics:

- configuration-invalid
- profile-not-found
- executable-not-found
- process-spawn-failed
- command-failed
- invalid-credential-output
- log-read-failed
- log-write-failed

Error text should state the failed operation and an actionable next step without exposing secrets.

## 8. Implementation phases

### Phase 1: Rust project bootstrap

- Initialize the Cargo package in this directory.
- Establish the binary target and basic module layout.
- Add command-line parsing and version output.
- Add formatting, linting, and test commands used by the project.

### Phase 2: Core command execution

- Implement direct executable-plus-arguments spawning.
- Implement timeout and exit-status handling.
- Implement credential output normalization and validation.
- Implement `${NAME}` expansion in command arguments with missing-variable and no-shell semantics.
- Implement scheme selection (`--scheme`, `--no-scheme`) and strict stdout behavior.
- Add tests with a controlled fake executable.

### Phase 3: GitHub provider

- Implement `auth-header github`.
- Support optional `--hostname`.
- Reuse the generic command execution path.
- Add missing-`gh`, failed-`gh`, and successful-output coverage.

### Phase 4: Profiles and configuration

- Implement platform-specific configuration path resolution.
- Parse the TOML profile schema.
- Implement profile lookup and precedence rules.
- Ensure profiles cannot inject shell syntax because commands remain argument arrays.

### Phase 5: Failure logging

- Implement platform-specific per-user state paths.
- Implement failure-record creation only after an unsuccessful resolution.
- Implement bounded retention and opaque record IDs.
- Implement `log`, `--count`, `--all`, `--id`, and `--path`.
- Verify that logging failures do not alter provider results.

### Phase 6: Verification and documentation

- Add unit tests for normalization, scheme selection, scheme formatting, environment expansion, missing variables, error categories, and config precedence.
- Add integration tests for process failures, missing executables, failure listing, record inspection, retention, and log-write failure behavior.
- Run a real CLI smoke test for success and failure paths.
- Document installation, omp configuration, GitHub setup, profiles, failure logging, and diagnostic inspection.

## 9. Acceptance criteria

The implementation is complete when:

1. `auth-header github` prints a valid `Bearer <token>` value when `gh auth token` succeeds.
2. The same omp configuration works without PowerShell-specific syntax on Windows, Linux, and macOS.
3. A missing `gh` executable produces a nonzero exit status and a useful stderr message.
4. Successful stdout contains no diagnostics or extra formatting.
5. Generic executable-plus-arguments sources work without a shell, support Bearer, custom schemes, no-scheme output, and `${NAME}` argument expansion.
6. Named profiles can reproduce the generic-command behavior without storing credentials.
7. `auth-header log` lists recent failures even when the provider executable is unavailable.
8. Each failure entry has an opaque identifier, timestamp, provider/profile, and failure category; `auth-header log --id <id>` retrieves its complete safe record.
9. No token, generated header, child stdout, child stderr, or unredacted arguments are persisted in logs.
10. Environment expansion never splits values into multiple arguments and never persists expanded values in diagnostic records.
11. Logging failures do not cause an otherwise successful credential lookup to fail.

## 10. Design risks to revisit later

- Whether the failure-record retention limit should be configurable
- Whether some providers need explicit expiry metadata and caching
- Whether multi-header output is needed by real consumers
- Whether a structured diagnostic format should supplement the human-readable log
