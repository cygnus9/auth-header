mod platform;

use std::{
    collections::BTreeMap,
    env,
    fmt::Write as _,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use chrono::{Local, TimeZone, Utc};
use clap::{Args, Parser, Subcommand};
use serde::Deserialize;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_LOG_COUNT: usize = 10;
const MAX_RETAINED_FAILURES: usize = 100;
static RECORD_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Parser, Debug)]
#[command(
    name = "auth-header",
    version,
    about = "Resolve credentials into HTTP header values"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the GitHub CLI credential provider.
    Github {
        #[arg(long)]
        hostname: Option<String>,
        #[command(flatten)]
        scheme: SchemeOptions,
    },
    /// Run an executable and use its stdout as the credential.
    Exec {
        #[command(flatten)]
        scheme: SchemeOptions,
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Resolve a named profile from the auth-header TOML configuration.
    Profile {
        name: String,
        #[command(flatten)]
        scheme: SchemeOptions,
    },
    /// Inspect retained failure records without invoking a provider.
    Log {
        #[arg(long, conflicts_with_all = ["all", "id", "path"])]
        count: Option<usize>,
        #[arg(long, conflicts_with_all = ["count", "id", "path"])]
        all: bool,
        #[arg(long, conflicts_with_all = ["count", "all", "path"])]
        id: Option<String>,
        #[arg(long, conflicts_with_all = ["count", "all", "id"])]
        path: bool,
        #[arg(long)]
        utc: bool,
    },
}

#[derive(Args, Debug, Clone, Default)]
struct SchemeOptions {
    #[arg(long, conflicts_with = "no_scheme", value_name = "VALUE")]
    scheme: Option<String>,
    #[arg(long)]
    no_scheme: bool,
}

#[derive(Debug, Clone, Copy)]
enum Category {
    Invocation,
    Tool,
}

impl Category {
    fn as_str(self) -> &'static str {
        match self {
            Self::Invocation => "invocation",
            Self::Tool => "tool",
        }
    }
}

#[derive(Debug)]
struct AppError {
    category: Category,
    code: String,
    message: String,
    provider: String,
    executable: Option<String>,
    exit_code: Option<i32>,
    duration_ms: Option<u128>,
}

type AppResult<T> = Result<T, Box<AppError>>;

impl AppError {
    fn tool(
        provider: impl Into<String>,
        executable: Option<&str>,
        code: &str,
        message: impl Into<String>,
    ) -> Box<Self> {
        Box::new(Self {
            category: Category::Tool,
            code: code.to_owned(),
            message: message.into(),
            provider: provider.into(),
            executable: executable.map(str::to_owned),
            exit_code: None,
            duration_ms: None,
        })
    }

    fn invocation(
        provider: impl Into<String>,
        executable: Option<&str>,
        code: &str,
        message: impl Into<String>,
        duration_ms: u128,
        exit_code: Option<i32>,
    ) -> Box<Self> {
        Box::new(Self {
            category: Category::Invocation,
            code: code.to_owned(),
            message: message.into(),
            provider: provider.into(),
            executable: executable.map(str::to_owned),
            exit_code,
            duration_ms: Some(duration_ms),
        })
    }
}

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default)]
    profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Deserialize)]
struct Profile {
    #[serde(rename = "type")]
    kind: String,
    program: String,
    #[serde(default)]
    args: Vec<String>,
    scheme: Option<String>,
}

#[derive(Debug, Clone)]
struct FailureRecord {
    id: String,
    timestamp: u128,
    version: String,
    provider: String,
    executable: String,
    category: String,
    error_code: String,
    exit_code: Option<i32>,
    duration_ms: Option<u128>,
}

fn main() {
    let cli = Cli::parse();
    let is_log = matches!(cli.command, Commands::Log { .. });
    let result = execute(cli.command);

    match result {
        Ok(()) => {}
        Err(error) if is_log => {
            eprintln!("auth-header: {}", error.message);
            std::process::exit(1);
        }
        Err(error) => {
            let log_warning = write_failure_record(&error).err();
            eprintln!("auth-header: {}", error.message);
            if let Some(logging_error) = log_warning {
                eprintln!("auth-header: warning: could not save failure record: {logging_error}");
            } else {
                eprintln!("hint: run `auth-header log` for details");
            }
            std::process::exit(1);
        }
    }
}

fn execute(command: Commands) -> AppResult<()> {
    match command {
        Commands::Github { hostname, scheme } => {
            let mut args = vec!["auth".to_owned(), "token".to_owned()];
            if let Some(hostname) = hostname {
                if hostname.is_empty() || hostname.chars().any(char::is_whitespace) {
                    return Err(AppError::tool(
                        "github",
                        Some("gh"),
                        "configuration-invalid",
                        "GitHub hostname must be nonempty and contain no whitespace",
                    ));
                }
                args.push("--hostname".to_owned());
                args.push(hostname);
            }
            let selected_scheme = select_scheme(&scheme, None, "github", Some("gh"))?;
            resolve_command("github", "gh", &args, selected_scheme.as_deref())
        }
        Commands::Exec { scheme, command } => {
            let Some((program, args)) = command.split_first() else {
                return Err(AppError::tool(
                    "exec",
                    None,
                    "configuration-invalid",
                    "a command is required after `--`",
                ));
            };
            let selected_scheme = select_scheme(&scheme, None, "exec", Some(program))?;
            resolve_command("exec", program, args, selected_scheme.as_deref())
        }
        Commands::Profile { name, scheme } => execute_profile(&name, &scheme),
        Commands::Log {
            count,
            all,
            id,
            path,
            utc,
        } => execute_log(count, all, id, path, utc),
    }
}

fn execute_profile(name: &str, options: &SchemeOptions) -> AppResult<()> {
    let config_path = config_path();
    let config_text = match fs::read_to_string(&config_path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(AppError::tool(
                format!("profile:{name}"),
                None,
                "profile-not-found",
                format!(
                    "profile `{name}` was not found; create {}",
                    config_path.display()
                ),
            ));
        }
        Err(error) => {
            return Err(AppError::tool(
                format!("profile:{name}"),
                None,
                "configuration-invalid",
                format!("could not read {}: {error}", config_path.display()),
            ));
        }
    };
    let config: Config = toml::from_str(&config_text).map_err(|error| {
        AppError::tool(
            format!("profile:{name}"),
            None,
            "configuration-invalid",
            format!(
                "invalid configuration in {}: {error}",
                config_path.display()
            ),
        )
    })?;
    let Some(profile) = config.profiles.get(name) else {
        return Err(AppError::tool(
            format!("profile:{name}"),
            None,
            "profile-not-found",
            format!(
                "profile `{name}` is not defined in {}",
                config_path.display()
            ),
        ));
    };
    if profile.kind != "command" || profile.program.is_empty() {
        return Err(AppError::tool(
            format!("profile:{name}"),
            (!profile.program.is_empty()).then_some(profile.program.as_str()),
            "configuration-invalid",
            format!("profile `{name}` must have type = \"command\" and a nonempty program"),
        ));
    }
    let selected_scheme = select_scheme(
        options,
        profile.scheme.as_deref(),
        &format!("profile:{name}"),
        Some(&profile.program),
    )?;
    resolve_command(
        format!("profile:{name}"),
        &profile.program,
        &profile.args,
        selected_scheme.as_deref(),
    )
}

fn select_scheme(
    options: &SchemeOptions,
    configured: Option<&str>,
    provider: &str,
    executable: Option<&str>,
) -> AppResult<Option<String>> {
    if options.no_scheme {
        return Ok(None);
    }
    if let Some(scheme) = options.scheme.as_deref() {
        return validate_scheme(scheme, provider, executable);
    }
    match configured {
        Some("none") => Ok(None),
        Some(scheme) => validate_scheme(scheme, provider, executable),
        None => Ok(Some("Bearer".to_owned())),
    }
}

fn validate_scheme(
    scheme: &str,
    provider: &str,
    executable: Option<&str>,
) -> AppResult<Option<String>> {
    if scheme.is_empty()
        || scheme
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(AppError::tool(
            provider,
            executable,
            "configuration-invalid",
            "authentication scheme must be one nonempty token without whitespace or control characters",
        ));
    }
    if scheme == "none" {
        Ok(None)
    } else {
        Ok(Some(scheme.to_owned()))
    }
}

fn resolve_command(
    provider: impl Into<String>,
    program: &str,
    args: &[String],
    scheme: Option<&str>,
) -> AppResult<()> {
    let provider = provider.into();
    let expanded_args = args
        .iter()
        .map(|argument| {
            expand_argument(argument).map_err(|message| {
                AppError::tool(&provider, Some(program), "configuration-invalid", message)
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let credential = run_process(&provider, program, &expanded_args)?;
    let header = format_header(&credential, scheme).map_err(|message| {
        AppError::tool(
            &provider,
            Some(program),
            "invalid-credential-output",
            message,
        )
    })?;
    println!("{header}");
    Ok(())
}

fn run_process(provider: &str, program: &str, args: &[String]) -> AppResult<String> {
    let started = Instant::now();
    let mut child = Command::new(program);
    child
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = match child.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(AppError::invocation(
                provider,
                Some(program),
                "executable-not-found",
                format!("executable `{program}` was not found on PATH"),
                started.elapsed().as_millis(),
                None,
            ));
        }
        Err(error) => {
            return Err(AppError::invocation(
                provider,
                Some(program),
                "process-spawn-failed",
                format!("could not start `{program}`: {error}"),
                started.elapsed().as_millis(),
                None,
            ));
        }
    };

    let mut stdout = child.stdout.take().expect("piped stdout must be available");
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.read_to_end(&mut bytes);
        (result, bytes)
    });

    let status = match wait_for_process(provider, program, started, &mut child) {
        Ok(status) => status,
        Err(error) => {
            let _ = reader.join();
            return Err(error);
        }
    };
    let (read_result, output) = reader.join().map_err(|_| {
        AppError::invocation(
            provider,
            Some(program),
            "process-output-read-failed",
            format!("could not read output from `{program}`"),
            started.elapsed().as_millis(),
            status.code(),
        )
    })?;
    if let Err(error) = read_result {
        return Err(AppError::invocation(
            provider,
            Some(program),
            "process-output-read-failed",
            format!("could not read output from `{program}`: {error}"),
            started.elapsed().as_millis(),
            status.code(),
        ));
    }
    let duration = started.elapsed().as_millis();
    if !status.success() {
        return Err(AppError::invocation(
            provider,
            Some(program),
            "command-failed",
            format!(
                "credential command `{program}` exited with status {}",
                status_text(status.code())
            ),
            duration,
            status.code(),
        ));
    }
    String::from_utf8(output).map_err(|_| {
        AppError::tool(
            provider,
            Some(program),
            "invalid-credential-output",
            "credential command output was not valid UTF-8",
        )
    })
}

fn wait_for_process(
    provider: &str,
    program: &str,
    started: Instant,
    child: &mut Child,
) -> AppResult<ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if started.elapsed() >= COMMAND_TIMEOUT => {
                terminate_process(child);
                return Err(AppError::invocation(
                    provider,
                    Some(program),
                    "process-timeout",
                    format!(
                        "credential command `{program}` timed out after {} seconds",
                        COMMAND_TIMEOUT.as_secs()
                    ),
                    started.elapsed().as_millis(),
                    None,
                ));
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                terminate_process(child);
                return Err(AppError::invocation(
                    provider,
                    Some(program),
                    "process-wait-failed",
                    format!("could not wait for `{program}`: {error}"),
                    started.elapsed().as_millis(),
                    None,
                ));
            }
        }
    }
}

fn terminate_process(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn status_text(code: Option<i32>) -> String {
    code.map_or_else(|| "signal".to_owned(), |code| code.to_string())
}

fn expand_argument(argument: &str) -> Result<String, String> {
    let mut result = String::with_capacity(argument.len());
    let mut cursor = 0;
    while let Some(relative) = argument[cursor..].find("${") {
        let start = cursor + relative;
        result.push_str(&argument[cursor..start]);
        let name_start = start + 2;
        let Some(relative_end) = argument[name_start..].find('}') else {
            return Err(format!(
                "invalid environment placeholder in argument `{argument}`"
            ));
        };
        let end = name_start + relative_end;
        let name = &argument[name_start..end];
        if name.is_empty()
            || !name.bytes().enumerate().all(|(index, byte)| {
                (index == 0 && (byte == b'_' || byte.is_ascii_alphabetic()))
                    || (index > 0 && (byte == b'_' || byte.is_ascii_alphanumeric()))
            })
        {
            return Err(format!(
                "invalid environment variable name `{name}` in command argument"
            ));
        }
        let value = env::var(name).map_err(|error| match error {
            env::VarError::NotPresent => format!("environment variable `{name}` is not set"),
            env::VarError::NotUnicode(_) => {
                format!("environment variable `{name}` is not valid UTF-8")
            }
        })?;
        result.push_str(&value);
        cursor = end + 1;
    }
    result.push_str(&argument[cursor..]);
    Ok(result)
}

fn format_header(credential: &str, scheme: Option<&str>) -> Result<String, String> {
    let normalized = normalize_credential(credential)?;
    Ok(match scheme {
        Some(scheme) => format!("{scheme} {normalized}"),
        None => normalized,
    })
}

fn normalize_credential(raw: &str) -> Result<String, String> {
    let normalized = raw.strip_suffix('\n').unwrap_or(raw);
    if normalized.is_empty() {
        return Err("credential command returned empty output".to_owned());
    }
    if normalized.contains('\n') || normalized.contains('\r') {
        return Err("credential output contains an embedded newline or carriage return".to_owned());
    }
    Ok(normalized.to_owned())
}

fn execute_log(
    count: Option<usize>,
    all: bool,
    id: Option<String>,
    path: bool,
    utc: bool,
) -> AppResult<()> {
    let entries_dir = state_entries_dir();
    if path {
        println!("{}", entries_dir.display());
        return Ok(());
    }
    if let Some(id) = id {
        if !valid_record_id(&id) {
            return Err(AppError::tool(
                "log",
                None,
                "log-read-failed",
                "record ID contains invalid characters",
            ));
        }
        let record_path = entries_dir.join(format!("{id}.log"));
        let text = fs::read_to_string(&record_path).map_err(|error| {
            AppError::tool(
                "log",
                None,
                "log-read-failed",
                format!("could not read record `{id}`: {error}"),
            )
        })?;
        let record = parse_record(&text)
            .map_err(|error| AppError::tool("log", None, "log-read-failed", error))?;
        print_record(&record, utc)?;
        return Ok(());
    }

    let mut records = read_records(&entries_dir)?;
    records.sort_by(|left, right| {
        right
            .timestamp
            .cmp(&left.timestamp)
            .then_with(|| right.id.cmp(&left.id))
    });
    let limit = if all {
        records.len()
    } else {
        count.unwrap_or(DEFAULT_LOG_COUNT)
    };
    for record in records.into_iter().take(limit) {
        let timestamp = format_timestamp(record.timestamp, utc)
            .map_err(|error| AppError::tool("log", None, "log-read-failed", error))?;
        println!(
            "{}\t{}\t{}\t{}",
            record.id, timestamp, record.provider, record.category
        );
    }
    Ok(())
}

fn read_records(entries_dir: &Path) -> AppResult<Vec<FailureRecord>> {
    let directory = match fs::read_dir(entries_dir) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(AppError::tool(
                "log",
                None,
                "log-read-failed",
                format!("could not read {}: {error}", entries_dir.display()),
            ));
        }
    };
    let mut records = Vec::new();
    for entry in directory {
        let entry = entry.map_err(|error| {
            AppError::tool(
                "log",
                None,
                "log-read-failed",
                format!("could not inspect failure records: {error}"),
            )
        })?;
        if entry
            .path()
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("log")
        {
            continue;
        }
        let text = fs::read_to_string(entry.path()).map_err(|error| {
            AppError::tool(
                "log",
                None,
                "log-read-failed",
                format!("could not read failure record: {error}"),
            )
        })?;
        if let Ok(record) = parse_record(&text) {
            records.push(record);
        }
    }
    Ok(records)
}

fn print_record(record: &FailureRecord, utc: bool) -> AppResult<()> {
    let timestamp = format_timestamp(record.timestamp, utc)
        .map_err(|error| AppError::tool("log", None, "log-read-failed", error))?;
    println!("id: {}", record.id);
    println!("timestamp: {timestamp}");
    println!("version: {}", record.version);
    println!("provider: {}", record.provider);
    println!("executable: {}", record.executable);
    println!("category: {}", record.category);
    println!("error_code: {}", record.error_code);
    println!(
        "exit_code: {}",
        record
            .exit_code
            .map_or_else(|| "-".to_owned(), |code| code.to_string())
    );
    println!(
        "duration_ms: {}",
        record
            .duration_ms
            .map_or_else(|| "-".to_owned(), |duration| duration.to_string())
    );
    Ok(())
}

fn write_failure_record(error: &AppError) -> Result<(), String> {
    let entries_dir = state_entries_dir();
    fs::create_dir_all(&entries_dir)
        .map_err(|error| format!("could not create {}: {error}", entries_dir.display()))?;
    platform::set_private_directory(&entries_dir)?;
    let timestamp = now_millis();
    let counter = RECORD_COUNTER.fetch_add(1, Ordering::Relaxed);
    let id = format!("{timestamp:x}-{:x}-{counter:x}", std::process::id());
    let record = FailureRecord {
        id: id.clone(),
        timestamp,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        provider: error.provider.clone(),
        executable: error
            .executable
            .as_deref()
            .map_or_else(|| "-".to_owned(), executable_basename),
        category: error.category.as_str().to_owned(),
        error_code: error.code.clone(),
        exit_code: error.exit_code,
        duration_ms: error.duration_ms,
    };
    let text = serialize_record(&record);
    let temporary = entries_dir.join(format!(".{id}.tmp"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("could not create temporary record: {error}"))?;
    platform::set_private_file(&file)?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("could not write failure record: {error}"))?;
    drop(file);
    fs::rename(&temporary, entries_dir.join(format!("{id}.log")))
        .map_err(|error| format!("could not commit failure record: {error}"))?;
    prune_records(&entries_dir)
}

fn executable_basename(program: &str) -> String {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(program)
        .to_owned()
}

fn prune_records(entries_dir: &Path) -> Result<(), String> {
    let mut paths = fs::read_dir(entries_dir)
        .map_err(|error| format!("could not inspect failure records: {error}"))?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("log")
        })
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect::<Vec<_>>();
    if paths.len() <= MAX_RETAINED_FAILURES {
        return Ok(());
    }
    paths.sort_by_key(|(modified, _)| *modified);
    let remove_count = paths.len() - MAX_RETAINED_FAILURES;
    for (_, path) in paths.into_iter().take(remove_count) {
        fs::remove_file(path)
            .map_err(|error| format!("could not prune failure record: {error}"))?;
    }
    Ok(())
}

fn serialize_record(record: &FailureRecord) -> String {
    let mut text = String::new();
    for (key, value) in [
        ("id", record.id.clone()),
        ("timestamp", record.timestamp.to_string()),
        ("version", record.version.clone()),
        ("provider", record.provider.clone()),
        ("executable", record.executable.clone()),
        ("category", record.category.clone()),
        ("error_code", record.error_code.clone()),
        (
            "exit_code",
            record
                .exit_code
                .map_or_else(String::new, |value| value.to_string()),
        ),
        (
            "duration_ms",
            record
                .duration_ms
                .map_or_else(String::new, |value| value.to_string()),
        ),
    ] {
        text.push_str(key);
        text.push('=');
        text.push_str(&encode_field(&value));
        text.push('\n');
    }
    text
}

fn parse_record(text: &str) -> Result<FailureRecord, String> {
    let mut fields = BTreeMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            return Err("failure record is malformed".to_owned());
        };
        fields.insert(key.to_owned(), decode_field(value)?);
    }
    let get = |key: &str| {
        fields
            .get(key)
            .cloned()
            .ok_or_else(|| format!("failure record lacks {key}"))
    };
    let parse_optional = |key: &str| -> Result<Option<i32>, String> {
        let value = get(key)?;
        if value.is_empty() {
            Ok(None)
        } else {
            value
                .parse()
                .map(Some)
                .map_err(|_| format!("failure record has invalid {key}"))
        }
    };
    Ok(FailureRecord {
        id: get("id")?,
        timestamp: get("timestamp")?
            .parse()
            .map_err(|_| "failure record has invalid timestamp".to_owned())?,
        version: get("version")?,
        provider: get("provider")?,
        executable: get("executable")?,
        category: get("category")?,
        error_code: get("error_code")?,
        exit_code: parse_optional("exit_code")?,
        duration_ms: {
            let value = get("duration_ms")?;
            if value.is_empty() {
                None
            } else {
                Some(
                    value
                        .parse()
                        .map_err(|_| "failure record has invalid duration_ms".to_owned())?,
                )
            }
        },
    })
}

fn encode_field(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            let _ = write!(encoded, "{byte:02X}");
        }
    }
    encoded
}

fn decode_field(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len() {
            return Err("failure record contains invalid escaping".to_owned());
        }
        let high = hex_value(bytes[index + 1])
            .ok_or_else(|| "failure record contains invalid escaping".to_owned())?;
        let low = hex_value(bytes[index + 2])
            .ok_or_else(|| "failure record contains invalid escaping".to_owned())?;
        decoded.push(high * 16 + low);
        index += 3;
    }
    String::from_utf8(decoded).map_err(|_| "failure record contains invalid UTF-8".to_owned())
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn valid_record_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn config_path() -> PathBuf {
    if let Some(path) = env::var_os("AUTH_HEADER_CONFIG_DIR") {
        return PathBuf::from(path).join("config.toml");
    }
    platform_config_dir().join("config.toml")
}

fn state_entries_dir() -> PathBuf {
    if let Some(path) = env::var_os("AUTH_HEADER_STATE_DIR") {
        return PathBuf::from(path).join("entries");
    }
    platform_state_dir().join("entries")
}

fn platform_config_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        return env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join("AppData/Roaming"))
            .join("auth-header");
    }
    #[cfg(target_os = "macos")]
    {
        return home_dir().join("Library/Application Support/auth-header");
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        env::var_os("XDG_CONFIG_HOME")
            .map_or_else(|| home_dir().join(".config"), PathBuf::from)
            .join("auth-header")
    }
}

fn platform_state_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        return env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join("AppData/Local"))
            .join("auth-header");
    }
    #[cfg(target_os = "macos")]
    {
        return home_dir().join("Library/Logs/auth-header");
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        env::var_os("XDG_STATE_HOME")
            .map_or_else(|| home_dir().join(".local/state"), PathBuf::from)
            .join("auth-header")
    }
}

fn home_dir() -> PathBuf {
    env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn format_timestamp(timestamp: u128, utc: bool) -> Result<String, String> {
    let millis = i64::try_from(timestamp)
        .map_err(|_| format!("invalid timestamp {timestamp}: outside supported range"))?;
    let instant = Utc
        .timestamp_millis_opt(millis)
        .single()
        .ok_or_else(|| format!("invalid timestamp {timestamp}: outside supported range"))?;
    if utc {
        Ok(instant.format("%Y-%m-%d %H:%M:%S%.3f %:z").to_string())
    } else {
        Ok(instant
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S%.3f %:z")
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expansion_preserves_whitespace_as_one_value() {
        let key = "AUTH_HEADER_TEST_EXPANSION";
        unsafe {
            env::set_var(key, "one two");
        }
        assert_eq!(
            expand_argument("--value=${AUTH_HEADER_TEST_EXPANSION}"),
            Ok("--value=one two".to_owned())
        );
        unsafe {
            env::remove_var(key);
        }
    }

    #[test]
    fn expansion_rejects_missing_variables() {
        unsafe {
            env::remove_var("AUTH_HEADER_TEST_MISSING");
        }
        assert!(
            expand_argument("${AUTH_HEADER_TEST_MISSING}")
                .unwrap_err()
                .contains("not set")
        );
    }

    #[test]
    fn normalization_removes_only_one_final_newline() {
        assert_eq!(normalize_credential(" token \n"), Ok(" token ".to_owned()));
        assert!(normalize_credential("token\n\n").is_err());
        assert!(normalize_credential("token\r\n").is_err());
    }

    #[test]
    fn formatting_applies_or_omits_scheme() {
        assert_eq!(
            format_header("abc\n", Some("Bearer")),
            Ok("Bearer abc".to_owned())
        );
        assert_eq!(format_header("abc\n", None), Ok("abc".to_owned()));
    }

    #[test]
    fn exec_parser_accepts_no_scheme_before_separator() {
        let cli = Cli::try_parse_from([
            "auth-header",
            "exec",
            "--no-scheme",
            "--",
            "printf",
            "%s",
            "value",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Commands::Exec {
                scheme: SchemeOptions {
                    no_scheme: true,
                    scheme: None
                },
                ..
            }
        ));
    }

    #[test]
    fn log_parser_accepts_utc_without_changing_defaults() {
        let cli = Cli::try_parse_from(["auth-header", "log", "--utc"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Log {
                count: None,
                all: false,
                id: None,
                path: false,
                utc: true,
            }
        ));
    }

    #[test]
    fn timestamp_formatting_uses_utc_or_local_time() {
        assert_eq!(
            format_timestamp(0, true),
            Ok("1970-01-01 00:00:00.000 +00:00".to_owned())
        );
        let expected_local = Utc
            .timestamp_millis_opt(0)
            .single()
            .unwrap()
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S%.3f %:z")
            .to_string();
        assert_eq!(format_timestamp(0, false), Ok(expected_local));
    }

    #[test]
    fn timestamp_formatting_rejects_unrepresentable_values() {
        assert!(format_timestamp(u128::MAX, true).is_err());
    }

    #[test]
    fn records_store_only_executable_basename() {
        assert_eq!(executable_basename("/opt/tools/company-cli"), "company-cli");
    }

    #[test]
    fn record_fields_round_trip_without_plaintext_delimiters() {
        let record = FailureRecord {
            id: "abc-1".to_owned(),
            timestamp: 123,
            version: "0.1.0".to_owned(),
            provider: "profile:internal".to_owned(),
            executable: "company cli".to_owned(),
            category: "invocation".to_owned(),
            error_code: "command-failed".to_owned(),
            exit_code: Some(2),
            duration_ms: Some(4),
        };
        let serialized = serialize_record(&record);
        assert!(serialized.contains("timestamp=123\n"));
        let parsed = parse_record(&serialized).unwrap();
        assert_eq!(parsed.timestamp, 123);
        assert_eq!(parsed.provider, record.provider);
        assert_eq!(parsed.executable, record.executable);
        assert_eq!(parsed.exit_code, Some(2));
    }
}
