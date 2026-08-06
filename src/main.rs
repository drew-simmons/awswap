use inquire::{Select, error::InquireError};
use owo_colors::OwoColorize;
use std::collections::BTreeSet;
use std::env;
use std::fmt;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HELP: &str = r#"awswap — quickly switch AWS profiles

Usage:
  awswap [options]                  Interactively choose and activate a profile
  awswap [options] <profile>        Activate a profile
  awswap [options] -                Switch to the previous profile
  awswap current                    Print the active profile
  awswap list                       List configured profiles
  awswap status [profile]           Show identity and integration status
  awswap doctor [profile]           Diagnose configuration and credentials
  awswap login [options] [profile]  Refresh AWS and ECR authentication
  awswap init <shell>               Print a shell hook (bash, zsh, or fish)
  awswap completions <shell>        Print shell completions
  awswap help

Options:
      --no-ecr              Skip automatic Docker/Helm ECR login
  -r, --registry <value>    ECR registry hostname or account ID (repeatable)
  -q, --quiet               Suppress progress and success output
      --json                Emit machine-readable JSON where supported
  -v, --verbose             Show commands and detailed AWS failures
  -h, --help                Show help
  -V, --version             Show version

Environment:
  AWSWAP_HOME               State directory (default: $XDG_STATE_HOME/awswap)
  AWSWAP_NO_ECR             Skip automatic Docker/Helm ECR login
  AWSWAP_ECR_REGISTRIES     Comma-separated registry hosts or account IDs
  NO_COLOR                  Disable colored output

Install the hook once so `awswap` updates the current shell:
  eval "$(awswap init zsh)" # use bash or fish as appropriate
"#;

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Shell {
    Bash,
    Zsh,
    Fish,
}

impl Shell {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "bash" => Ok(Self::Bash),
            "zsh" => Ok(Self::Zsh),
            "fish" => Ok(Self::Fish),
            _ => Err(format!(
                "unsupported shell '{value}'; expected bash, zsh, or fish"
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegistryLoginMethod {
    Password,
    EcrCredentialHelper,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegistryClient {
    Docker,
    Helm,
}

impl RegistryClient {
    fn command(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Helm => "helm",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Docker => "Docker",
            Self::Helm => "Helm",
        }
    }

    fn login_args(self, registry: &str) -> Vec<&str> {
        match self {
            Self::Docker => vec!["login", registry, "--username", "AWS", "--password-stdin"],
            Self::Helm => vec![
                "registry",
                "login",
                registry,
                "--username",
                "AWS",
                "--password-stdin",
            ],
        }
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
struct State {
    current: Option<String>,
    previous: Option<String>,
    recent: Vec<String>,
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
struct ProfileConfig {
    region: Option<String>,
    account_id: Option<String>,
    role_name: Option<String>,
    source_profile: Option<String>,
    is_sso: bool,
    is_login: bool,
}

impl ProfileConfig {
    fn auth_label(&self) -> &'static str {
        if self.is_sso {
            "SSO"
        } else if self.is_login {
            "login"
        } else if self.source_profile.is_some() || self.role_name.is_some() {
            "role"
        } else {
            "credentials"
        }
    }
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
struct Identity {
    account: String,
    arn: String,
    user_id: String,
}

impl Identity {
    fn display_name(&self) -> &str {
        self.arn
            .split_once(":assumed-role/")
            .map(|(_, value)| value)
            .or_else(|| self.arn.rsplit_once('/').map(|(_, value)| value))
            .unwrap_or(&self.arn)
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
struct Options {
    no_ecr: bool,
    registries: Vec<String>,
    quiet: bool,
    json: bool,
    verbose: bool,
}

impl Options {
    fn progress(&self, message: impl fmt::Display) {
        if !self.quiet && !self.json && (io::stderr().is_terminal() || self.verbose) {
            eprintln!("{} {message}", "…".cyan().bold());
        }
    }

    fn trace(&self, command: &str, args: &[&str]) {
        if self.verbose {
            eprintln!("{} {command} {}", "+".dimmed(), args.join(" ").dimmed());
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct ProfileChoice {
    name: String,
    marker: &'static str,
    region: String,
    auth: &'static str,
    account: String,
}

impl fmt::Display for ProfileChoice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}{:<24} {:<15} {:<11} {}",
            self.marker, self.name, self.region, self.auth, self.account
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CheckLevel {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Eq, PartialEq)]
struct DoctorCheck {
    level: CheckLevel,
    name: String,
    detail: String,
}

fn main() -> ExitCode {
    if env::var_os("NO_COLOR").is_some() {
        owo_colors::set_override(false);
    }
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.is_empty() => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{} {error}", "error:".red().bold());
            ExitCode::FAILURE
        }
    }
}

fn parse_cli(args: Vec<String>) -> Result<(Vec<String>, Options)> {
    let mut positional = Vec::new();
    let mut options = Options::default();
    let mut arguments = args.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--no-ecr" => options.no_ecr = true,
            "-q" | "--quiet" => options.quiet = true,
            "--json" => options.json = true,
            "-v" | "--verbose" => options.verbose = true,
            "-r" | "--registry" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| format!("{argument} requires a value"))?;
                add_registries(&mut options.registries, &value);
            }
            "--" => {
                positional.extend(arguments);
                break;
            }
            "-" | "-h" | "--help" | "-V" | "--version" => positional.push(argument),
            _ if argument.starts_with("--registry=") => {
                add_registries(
                    &mut options.registries,
                    argument.trim_start_matches("--registry="),
                );
            }
            _ if argument.starts_with('-') => return Err(format!("unknown option '{argument}'")),
            _ => positional.push(argument),
        }
    }
    if options.quiet && options.verbose {
        return Err("--quiet and --verbose cannot be used together".into());
    }
    Ok((positional, options))
}

fn add_registries(registries: &mut Vec<String>, value: &str) {
    registries.extend(
        value
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_string),
    );
}

fn run(args: Vec<String>) -> Result<()> {
    let (args, options) = parse_cli(args)?;
    match args.as_slice() {
        [] => switch(None, &options),
        [arg] if arg == "-h" || arg == "--help" || arg == "help" => {
            print!("{HELP}");
            Ok(())
        }
        [arg] if arg == "-V" || arg == "--version" || arg == "version" => {
            println!("awswap {VERSION}");
            Ok(())
        }
        [arg] if arg == "list" || arg == "ls" => list_profiles(&options),
        [arg] if arg == "current" => current_profile(&options),
        [arg] if arg == "status" => status(None, &options),
        [command, profile] if command == "status" => status(Some(profile), &options),
        [arg] if arg == "doctor" => doctor(None, &options),
        [command, profile] if command == "doctor" => doctor(Some(profile), &options),
        [arg] if arg == "-" => switch_previous(&options),
        [arg] if arg == "login" => login(None, &options),
        [command, profile] if command == "login" => login(Some(profile), &options),
        [command, shell] if command == "init" => {
            print_shell_hook(Shell::parse(shell)?);
            Ok(())
        }
        [command, shell] if command == "completions" => {
            print_completions(Shell::parse(shell)?);
            Ok(())
        }
        [profile] => switch(Some(profile), &options),
        [command, ..] => Err(format!(
            "unknown command or invalid arguments: {command}\n\n{HELP}"
        )),
    }
}

fn switch(requested: Option<&String>, options: &Options) -> Result<()> {
    require_command("aws")?;
    let profiles = discover_profiles()?;
    if profiles.is_empty() {
        return Err("no AWS profiles found; run `aws configure sso` first".into());
    }

    let mut state = load_state()?;
    let profile = match requested {
        Some(profile) => {
            ensure_profile_exists(profile, &profiles)?;
            profile.clone()
        }
        None if io::stdin().is_terminal() && io::stderr().is_terminal() => {
            select_profile(&profiles, &state)?
        }
        None => {
            return Err("interactive selection requires a terminal; pass a profile name".into());
        }
    };

    activate(&profile, &mut state, true, options)
}

fn switch_previous(options: &Options) -> Result<()> {
    require_command("aws")?;
    let profiles = discover_profiles()?;
    let mut state = load_state()?;
    let previous = state
        .previous
        .clone()
        .ok_or_else(|| "no previous AWS profile".to_string())?;
    ensure_profile_exists(&previous, &profiles)?;
    activate(&previous, &mut state, true, options)
}

fn activate(
    profile: &str,
    state: &mut State,
    authenticate_ecr: bool,
    options: &Options,
) -> Result<()> {
    let identity = ensure_credentials(profile, options)?;

    let old_current = state.current.clone();
    if old_current.as_deref() != Some(profile) {
        state.previous = old_current;
        state.current = Some(profile.to_string());
    }
    state.recent.retain(|recent| recent != profile);
    state.recent.insert(0, profile.to_string());
    state.recent.truncate(8);
    save_state(state)?;

    let profile_config = read_profile_config(profile)?;
    let hook_active = shell_hook_active();
    if options.json {
        println!(
            "{}",
            serde_json::json!({
                "profile": profile,
                "region": profile_config.region,
                "account": identity.account,
                "identity": identity.arn,
                "shell_active": hook_active,
            })
        );
    } else if !options.quiet {
        let verb = if hook_active { "Active" } else { "Selected" };
        let region = profile_config.region.as_deref().unwrap_or("no-region");
        println!(
            "{} {} {}  {}  {}  {}",
            "✓".green().bold(),
            verb.dimmed(),
            profile.green().bold(),
            identity.account.dimmed(),
            region.dimmed(),
            identity.display_name().dimmed(),
        );
    }

    if authenticate_ecr
        && !options.no_ecr
        && !env_flag("AWSWAP_NO_ECR")
        && let Err(error) = login_ecr(profile, &profile_config, options)
    {
        eprintln!("{} {error}", "warning:".yellow().bold());
        let state = if hook_active { "active" } else { "selected" };
        eprintln!(
            "{}",
            format!("AWS profile is {state}; retry with `awswap login`.").dimmed()
        );
    }

    if !hook_active && !options.quiet && !options.json && io::stdout().is_terminal() {
        eprintln!(
            "{} shell unchanged; run {} once to activate future selections",
            "tip:".cyan().bold(),
            shell_setup_hint().cyan()
        );
    }
    Ok(())
}

fn login(requested: Option<&String>, options: &Options) -> Result<()> {
    require_command("aws")?;
    let profiles = discover_profiles()?;
    let state = load_state()?;
    let profile = resolve_profile(requested, &profiles, &state)?;

    refresh_credentials(&profile, options)?;
    let identity = validate_credentials(&profile, options)?;
    let config = read_profile_config(&profile)?;
    if !options.no_ecr && !env_flag("AWSWAP_NO_ECR") {
        login_ecr(&profile, &config, options)?;
    }
    if options.json {
        println!(
            "{}",
            serde_json::json!({"profile": profile, "account": identity.account, "authenticated": true})
        );
    } else if !options.quiet {
        println!("{} {}", "✓ Authenticated".green().bold(), profile.bold());
    }
    Ok(())
}

fn resolve_profile(
    requested: Option<&String>,
    profiles: &[String],
    state: &State,
) -> Result<String> {
    match requested {
        Some(profile) => {
            ensure_profile_exists(profile, profiles)?;
            Ok(profile.clone())
        }
        None => effective_profile(state)
            .filter(|profile| profiles.contains(profile))
            .ok_or_else(|| "no active profile; run `awswap <profile>` first".to_string()),
    }
}

fn list_profiles(options: &Options) -> Result<()> {
    let profiles = discover_profiles()?;
    if profiles.is_empty() {
        return Err("no AWS profiles found".into());
    }
    let state = load_state()?;
    let current = effective_profile(&state);
    let ordered = ordered_profiles(&profiles, &state, current.as_deref());
    if options.json {
        let values: Vec<_> = ordered
            .iter()
            .map(|profile| {
                let config = read_profile_config(profile).unwrap_or_default();
                serde_json::json!({
                    "name": profile,
                    "current": current.as_deref() == Some(profile.as_str()),
                    "previous": state.previous.as_deref() == Some(profile.as_str()),
                    "region": config.region,
                    "auth": config.auth_label(),
                })
            })
            .collect();
        println!("{}", serde_json::Value::Array(values));
    } else if io::stdout().is_terminal() && !options.quiet {
        let config_contents = read_aws_config_contents()?;
        for choice in profile_choices(&ordered, &state, current.as_deref(), &config_contents) {
            let marker = match choice.marker {
                "● " => "●".green().to_string(),
                "↩ " => "↩".cyan().to_string(),
                _ => " ".into(),
            };
            println!(
                "{} {:<24} {:<15} {:<11} {}",
                marker, choice.name, choice.region, choice.auth, choice.account
            );
        }
    } else {
        for profile in ordered {
            println!("{profile}");
        }
    }
    Ok(())
}

fn current_profile(options: &Options) -> Result<()> {
    let state = load_state()?;
    if let Some(profile) = effective_profile(&state) {
        if options.json {
            println!("{}", serde_json::json!({"profile": profile}));
        } else {
            println!("{profile}");
        }
        Ok(())
    } else {
        Err("no active AWS profile".into())
    }
}

fn ordered_profiles(profiles: &[String], state: &State, current: Option<&str>) -> Vec<String> {
    let mut ordered = Vec::new();
    let mut add = |candidate: Option<&str>| {
        if let Some(candidate) = candidate
            && profiles.iter().any(|profile| profile == candidate)
            && !ordered.iter().any(|profile| profile == candidate)
        {
            ordered.push(candidate.to_string());
        }
    };
    add(current);
    add(state.previous.as_deref());
    for recent in &state.recent {
        add(Some(recent));
    }
    for profile in profiles {
        add(Some(profile));
    }
    ordered
}

fn profile_choices(
    profiles: &[String],
    state: &State,
    current: Option<&str>,
    config_contents: &str,
) -> Vec<ProfileChoice> {
    profiles
        .iter()
        .map(|profile| {
            let config = parse_profile_config(config_contents, profile);
            let marker = if current == Some(profile.as_str()) {
                "● "
            } else if state.previous.as_deref() == Some(profile.as_str()) {
                "↩ "
            } else {
                "  "
            };
            ProfileChoice {
                name: profile.clone(),
                marker,
                region: config.region.clone().unwrap_or_else(|| "—".into()),
                auth: config.auth_label(),
                account: config.account_id.unwrap_or_default(),
            }
        })
        .collect()
}

fn select_profile(profiles: &[String], state: &State) -> Result<String> {
    let current = effective_profile(state);
    let ordered = ordered_profiles(profiles, state, current.as_deref());
    let config_contents = read_aws_config_contents()?;
    let choices = profile_choices(&ordered, state, current.as_deref(), &config_contents);
    Select::new("AWS profile", choices)
        .with_starting_cursor(0)
        .with_page_size(ordered.len().min(12))
        .with_help_message("↑↓ move • type filter • enter select • esc cancel")
        .prompt()
        .map(|choice| choice.name)
        .map_err(|error| match error {
            InquireError::OperationCanceled | InquireError::OperationInterrupted => String::new(),
            other => format!("could not read selection: {other}"),
        })
}

fn ensure_credentials(profile: &str, options: &Options) -> Result<Identity> {
    match validate_credentials(profile, options) {
        Ok(identity) => Ok(identity),
        Err(first_error) if !first_error.contains("`awswap login") => Err(first_error),
        Err(first_error) => {
            if !options.quiet && !options.json {
                eprintln!(
                    "{} credentials for {} need refreshing",
                    "auth:".yellow().bold(),
                    profile.bold()
                );
            }
            refresh_credentials(profile, options).map_err(|login_error| {
                format!("credentials are unavailable ({first_error}); {login_error}")
            })?;
            validate_credentials(profile, options)
        }
    }
}

fn validate_credentials(profile: &str, options: &Options) -> Result<Identity> {
    options.progress(format!("Validating credentials for {profile}…"));
    let output = aws_output(
        profile,
        &["sts", "get-caller-identity", "--output", "json"],
        options,
    )?;
    if output.status.success() {
        parse_identity(&output.stdout)
    } else {
        Err(aws_command_error(
            "AWS credential check failed",
            profile,
            &output,
            options.verbose,
        ))
    }
}

fn parse_identity(bytes: &[u8]) -> Result<Identity> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("AWS returned invalid identity JSON: {error}"))?;
    Ok(Identity {
        account: value["Account"].as_str().unwrap_or_default().to_string(),
        arn: value["Arn"].as_str().unwrap_or_default().to_string(),
        user_id: value["UserId"].as_str().unwrap_or_default().to_string(),
    })
}

fn refresh_credentials(profile: &str, options: &Options) -> Result<()> {
    let config = read_profile_config(profile)?;
    let login_args: &[&str] = if config.is_sso {
        &["sso", "login"]
    } else if config.is_login {
        &["login"]
    } else {
        return Err(format!(
            "profile '{profile}' is not configured for SSO or `aws login`; refresh its credentials manually"
        ));
    };

    if !options.quiet && !options.json {
        eprintln!(
            "{} opening AWS sign-in for {}…",
            "auth:".cyan().bold(),
            profile.bold()
        );
    }
    options.trace("aws", login_args);
    let status = Command::new("aws")
        .args(login_args)
        .args(["--profile", profile])
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_SESSION_TOKEN")
        .status()
        .map_err(|error| format!("could not start AWS login: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("AWS login failed with {status}"))
    }
}

fn login_ecr(profile: &str, config: &ProfileConfig, options: &Options) -> Result<()> {
    let registries = ecr_registries(profile, config, options)?;
    if registries.is_empty() {
        return Ok(());
    }

    let has_docker = command_exists("docker");
    let has_helm = command_exists("helm");
    if !has_docker && !has_helm {
        return Err("Docker and Helm are not installed; skipped ECR login".into());
    }

    let region = config
        .region
        .as_deref()
        .ok_or_else(|| format!("profile '{profile}' has no region; skipped ECR login"))?;
    options.progress(format!("Requesting ECR credentials in {region}…"));
    let password_output = aws_output(
        profile,
        &["ecr", "get-login-password", "--region", region],
        options,
    )?;
    if !password_output.status.success() {
        return Err(aws_command_error(
            "could not get an ECR login token",
            profile,
            &password_output,
            options.verbose,
        ));
    }
    let password = password_output.stdout;

    let mut failures = Vec::new();
    for registry in registries {
        for client in [RegistryClient::Docker, RegistryClient::Helm] {
            let installed = match client {
                RegistryClient::Docker => has_docker,
                RegistryClient::Helm => has_helm,
            };
            if !installed {
                continue;
            }

            match registry_login(client, profile, &registry, &password) {
                Ok(method) => {
                    if !options.quiet && !options.json {
                        let detail = match method {
                            RegistryLoginMethod::Password => String::new(),
                            RegistryLoginMethod::EcrCredentialHelper => {
                                format!("  {}", "(ecr-login)".dimmed())
                            }
                        };
                        eprintln!("{} {:<7} {registry}{detail}", "✓".green(), client.label());
                    }
                }
                Err(error) => failures.push(error),
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn ecr_dns_suffix(region: &str) -> &'static str {
    if region.starts_with("cn-") {
        "amazonaws.com.cn"
    } else if region.starts_with("us-gov-") {
        "amazonaws.com"
    } else if region.starts_with("us-iso-") {
        "c2s.ic.gov"
    } else if region.starts_with("us-isob-") {
        "sc2s.sgov.gov"
    } else if region.starts_with("eu-isoe-") {
        "cloud.adc-e.uk"
    } else if region.starts_with("us-isof-") {
        "csp.hci.ic.gov"
    } else if region.starts_with("eusc-") {
        "amazonaws.eu"
    } else {
        "amazonaws.com"
    }
}

fn ecr_registry(account: &str, region: &str) -> String {
    format!("{account}.dkr.ecr.{region}.{}", ecr_dns_suffix(region))
}

fn ecr_registries(profile: &str, config: &ProfileConfig, options: &Options) -> Result<Vec<String>> {
    let region = match config.region.as_deref() {
        Some(region) => region,
        None => return Ok(Vec::new()),
    };

    let configured = if options.registries.is_empty() {
        env::var("AWSWAP_ECR_REGISTRIES")
            .ok()
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    } else {
        options.registries.clone()
    };
    if !configured.is_empty() {
        let mut registries = BTreeSet::new();
        for item in configured {
            let host = if item.chars().all(|character| character.is_ascii_digit()) {
                ecr_registry(&item, region)
            } else {
                normalize_registry(&item)
            };
            registries.insert(host);
        }
        return Ok(registries.into_iter().collect());
    }

    let output = aws_output(
        profile,
        &[
            "sts",
            "get-caller-identity",
            "--query",
            "Account",
            "--output",
            "text",
        ],
        options,
    )?;
    if !output.status.success() {
        return Err(aws_command_error(
            "could not determine the AWS account",
            profile,
            &output,
            options.verbose,
        ));
    }
    let account = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if account.is_empty() || account == "None" {
        return Err("AWS returned no account ID; skipped ECR login".into());
    }
    Ok(vec![ecr_registry(&account, region)])
}

fn registry_login(
    client: RegistryClient,
    profile: &str,
    registry: &str,
    password: &[u8],
) -> Result<RegistryLoginMethod> {
    if client == RegistryClient::Docker
        && docker_credential_helper(registry)?.as_deref() == Some("ecr-login")
    {
        validate_ecr_credential_helper(profile, registry)?;
        return Ok(RegistryLoginMethod::EcrCredentialHelper);
    }

    let command = client.command();
    let mut child = Command::new(command)
        .args(client.login_args(registry))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start {command}: {error}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| format!("could not open {command} input"))?
        .write_all(password)
        .map_err(|error| format!("could not send credentials to {command}: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("could not wait for {command}: {error}"))?;
    if output.status.success() {
        Ok(RegistryLoginMethod::Password)
    } else {
        Err(command_error(
            &format!("{command} login to {registry} failed"),
            &output,
        ))
    }
}

fn docker_credential_helper(registry: &str) -> Result<Option<String>> {
    let Some(path) = docker_config_path() else {
        return Ok(None);
    };
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    parse_docker_credential_helper(&contents, registry)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))
}

fn parse_docker_credential_helper(
    contents: &str,
    registry: &str,
) -> std::result::Result<Option<String>, serde_json::Error> {
    let config: serde_json::Value = serde_json::from_str(contents)?;
    Ok(config
        .get("credHelpers")
        .and_then(serde_json::Value::as_object)
        .and_then(|helpers| helpers.get(registry))
        .and_then(serde_json::Value::as_str)
        .or_else(|| config.get("credsStore").and_then(serde_json::Value::as_str))
        .filter(|helper| !helper.is_empty())
        .map(str::to_string))
}

fn validate_ecr_credential_helper(profile: &str, registry: &str) -> Result<()> {
    let command = "docker-credential-ecr-login";
    require_command(command)?;
    let mut child = Command::new(command)
        .arg("get")
        .env("AWS_PROFILE", profile)
        .env("AWS_DEFAULT_PROFILE", profile)
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_SESSION_TOKEN")
        .env_remove("AWS_SECURITY_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start {command}: {error}"))?;
    writeln!(
        child
            .stdin
            .take()
            .ok_or_else(|| format!("could not open {command} input"))?,
        "{registry}"
    )
    .map_err(|error| format!("could not send registry to {command}: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("could not wait for {command}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error(
            &format!("{command} could not authenticate to {registry}"),
            &output,
        ))
    }
}

fn discover_profiles() -> Result<Vec<String>> {
    let mut profiles = BTreeSet::new();
    if let Ok(output) = Command::new("aws")
        .args(["configure", "list-profiles"])
        .output()
        && output.status.success()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let profile = line.trim();
            if !profile.is_empty() {
                profiles.insert(profile.to_string());
            }
        }
    }

    for path in [aws_config_path(), aws_credentials_path()] {
        if let Ok(contents) = fs::read_to_string(path) {
            profiles.extend(parse_profiles(&contents));
        }
    }
    Ok(profiles.into_iter().collect())
}

fn parse_profiles(contents: &str) -> BTreeSet<String> {
    contents
        .lines()
        .filter_map(parse_section)
        .filter_map(|section| {
            if section == "default" {
                Some(section.to_string())
            } else if let Some(profile) = section.strip_prefix("profile ") {
                Some(profile.trim().to_string())
            } else if !section.starts_with("sso-session ")
                && !section.starts_with("services ")
                && !section.contains(' ')
            {
                Some(section.to_string())
            } else {
                None
            }
        })
        .collect()
}

fn read_aws_config_contents() -> Result<String> {
    match fs::read_to_string(aws_config_path()) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(format!("could not read AWS config: {error}")),
    }
}

fn read_profile_config(profile: &str) -> Result<ProfileConfig> {
    Ok(parse_profile_config(&read_aws_config_contents()?, profile))
}

fn parse_profile_config(contents: &str, profile: &str) -> ProfileConfig {
    let target = if profile == "default" {
        "default".to_string()
    } else {
        format!("profile {profile}")
    };
    let mut current_section = String::new();
    let mut config = ProfileConfig::default();

    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if let Some(section) = parse_section(line) {
            current_section = section.to_string();
            continue;
        }
        if current_section != target || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((raw_key, raw_value)) = line.split_once('=') else {
            continue;
        };
        let key = raw_key.trim();
        let value = raw_value.trim();
        match key {
            "region" if !value.is_empty() => config.region = Some(value.to_string()),
            "sso_account_id" if !value.is_empty() => config.account_id = Some(value.to_string()),
            "role_name" if !value.is_empty() => config.role_name = Some(value.to_string()),
            "role_arn" if !value.is_empty() => {
                config.account_id = value
                    .split(':')
                    .nth(4)
                    .filter(|part| !part.is_empty())
                    .map(str::to_string);
                config.role_name = value
                    .rsplit('/')
                    .next()
                    .filter(|part| !part.is_empty())
                    .map(str::to_string);
            }
            "source_profile" if !value.is_empty() => {
                config.source_profile = Some(value.to_string())
            }
            "sso_session" | "sso_start_url" if !value.is_empty() => config.is_sso = true,
            "login_session" if !value.is_empty() => config.is_login = true,
            _ => {}
        }
    }
    config
}

fn parse_section(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    trimmed
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .map(str::trim)
}

fn ensure_profile_exists(profile: &str, profiles: &[String]) -> Result<()> {
    if profiles.iter().any(|candidate| candidate == profile) {
        Ok(())
    } else {
        let hint = closest_profile(profile, profiles)
            .map(|candidate| format!("; did you mean '{candidate}'?"))
            .unwrap_or_default();
        Err(format!("AWS profile '{profile}' was not found{hint}"))
    }
}

fn closest_profile<'a>(requested: &str, profiles: &'a [String]) -> Option<&'a str> {
    profiles
        .iter()
        .map(|profile| (edit_distance(requested, profile), profile.as_str()))
        .filter(|(distance, _)| *distance <= 3)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, profile)| profile)
}

fn edit_distance(left: &str, right: &str) -> usize {
    let right_chars: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right_chars.len()).collect();
    for (left_index, left_char) in left.chars().enumerate() {
        let mut current = vec![left_index + 1];
        for (right_index, right_char) in right_chars.iter().enumerate() {
            let substitution = previous[right_index] + usize::from(left_char != *right_char);
            current.push(
                (current[right_index] + 1)
                    .min(previous[right_index + 1] + 1)
                    .min(substitution),
            );
        }
        previous = current;
    }
    previous[right_chars.len()]
}

fn aws_output(profile: &str, args: &[&str], options: &Options) -> Result<Output> {
    options.trace("aws", args);
    let mut command = Command::new("aws");
    command
        .args(args)
        .args(["--profile", profile])
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_SESSION_TOKEN")
        .env("AWS_PAGER", "");
    command
        .output()
        .map_err(|error| format!("could not run AWS CLI: {error}"))
}

fn output_detail(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    detail.replace('\n', " ")
}

fn command_error(context: &str, output: &Output) -> String {
    let detail = output_detail(output);
    if detail.is_empty() {
        format!("{context} ({})", output.status)
    } else {
        format!("{context}: {detail}")
    }
}

fn classify_aws_error(context: &str, profile: &str, detail: &str) -> Option<String> {
    let normalized = detail.to_ascii_lowercase();
    if normalized.contains("expiredtoken")
        || normalized.contains("token has expired")
        || normalized.contains("sso session") && normalized.contains("expired")
        || normalized.contains("unauthorizedssotoken")
    {
        Some(format!(
            "{context}: credentials for '{profile}' expired; run `awswap login {profile}`"
        ))
    } else if normalized.contains("unable to locate credentials")
        || normalized.contains("invalidclienttokenid")
        || normalized.contains("unrecognizedclientexception")
        || normalized.contains("credentials could not be loaded")
    {
        Some(format!(
            "{context}: credentials for '{profile}' are unavailable; run `awswap login {profile}`"
        ))
    } else if normalized.contains("accessdenied")
        || normalized.contains("access denied")
        || normalized.contains("not authorized")
    {
        Some(format!(
            "{context}: access denied for '{profile}'; verify its IAM permissions"
        ))
    } else if normalized.contains("could not connect")
        || normalized.contains("endpoint url")
        || normalized.contains("timed out")
        || normalized.contains("name or service not known")
        || normalized.contains("temporary failure in name resolution")
    {
        Some(format!(
            "{context}: could not reach AWS; check the network, proxy, and configured region"
        ))
    } else if normalized.contains("you must specify a region")
        || normalized.contains("invalid region")
    {
        Some(format!(
            "{context}: profile '{profile}' has no valid region; configure one with `aws configure set region <region> --profile {profile}`"
        ))
    } else if normalized.contains("config profile") && normalized.contains("could not be found") {
        Some(format!(
            "{context}: profile '{profile}' is missing from the AWS configuration"
        ))
    } else {
        None
    }
}

fn aws_command_error(context: &str, profile: &str, output: &Output, verbose: bool) -> String {
    let detail = output_detail(output);
    if let Some(classified) = classify_aws_error(context, profile, &detail) {
        if verbose && !detail.is_empty() {
            format!("{classified}\nAWS CLI: {detail}")
        } else {
            classified
        }
    } else if detail.is_empty() {
        format!("{context} ({})", output.status)
    } else {
        format!("{context}: {detail}")
    }
}

fn normalize_registry(value: &str) -> String {
    value
        .trim()
        .trim_end_matches('/')
        .strip_prefix("https://")
        .or_else(|| value.trim().trim_end_matches('/').strip_prefix("http://"))
        .unwrap_or(value.trim().trim_end_matches('/'))
        .split('/')
        .next()
        .unwrap_or_default()
        .to_string()
}

fn effective_profile(state: &State) -> Option<String> {
    env::var("AWS_PROFILE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| state.current.clone())
}

fn state_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("AWSWAP_HOME") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(path).join("awswap"));
    }
    home_dir()
        .map(|home| home.join(".local/state/awswap"))
        .ok_or_else(|| "could not determine the state directory; set AWSWAP_HOME".into())
}

fn state_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("state"))
}

fn load_state() -> Result<State> {
    let path = state_path()?;
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(State::default()),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    Ok(parse_state(&contents))
}

fn parse_state(contents: &str) -> State {
    let mut state = State::default();
    for line in contents.lines() {
        if let Some(value) = line
            .strip_prefix("current=")
            .filter(|value| !value.is_empty())
        {
            state.current = Some(value.to_string());
        } else if let Some(value) = line
            .strip_prefix("previous=")
            .filter(|value| !value.is_empty())
        {
            state.previous = Some(value.to_string());
        } else if let Some(value) = line
            .strip_prefix("recent=")
            .filter(|value| !value.is_empty())
            && !state.recent.iter().any(|recent| recent == value)
        {
            state.recent.push(value.to_string());
        }
    }
    state.recent.truncate(8);
    state
}

fn save_state(state: &State) -> Result<()> {
    let directory = state_dir()?;
    fs::create_dir_all(&directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let path = directory.join("state");
    let temporary = directory.join(format!(".state.{}.tmp", std::process::id()));
    let mut contents = format!(
        "current={}\nprevious={}\n",
        state.current.as_deref().unwrap_or_default(),
        state.previous.as_deref().unwrap_or_default()
    );
    for recent in &state.recent {
        contents.push_str(&format!("recent={recent}\n"));
    }
    fs::write(&temporary, contents)
        .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .map_err(|error| format!("could not update {}: {error}", path.display()))
}

fn docker_config_path() -> Option<PathBuf> {
    env::var_os("DOCKER_CONFIG")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".docker")))
        .map(|directory| directory.join("config.json"))
}

fn aws_config_path() -> PathBuf {
    env::var_os("AWS_CONFIG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().unwrap_or_default().join(".aws/config"))
}

fn aws_credentials_path() -> PathBuf {
    env::var_os("AWS_SHARED_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().unwrap_or_default().join(".aws/credentials"))
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn require_command(command: &str) -> Result<()> {
    if command_exists(command) {
        Ok(())
    } else {
        Err(format!("'{command}' was not found in PATH"))
    }
}

fn command_exists(command: &str) -> bool {
    env::var_os("PATH").is_some_and(|paths| {
        env::split_paths(&paths).any(|directory| {
            let candidate = directory.join(command);
            candidate.is_file() && is_executable(&candidate)
        })
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn env_flag(name: &str) -> bool {
    env::var(name).is_ok_and(|value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no"
        )
    })
}

fn status(requested: Option<&String>, options: &Options) -> Result<()> {
    require_command("aws")?;
    let profiles = discover_profiles()?;
    let state = load_state()?;
    let profile = resolve_profile(requested, &profiles, &state)?;
    let config = read_profile_config(&profile)?;
    let identity = validate_credentials(&profile, options)?;
    let clients: Vec<&str> = [
        ("Docker", command_exists("docker")),
        ("Helm", command_exists("helm")),
    ]
    .into_iter()
    .filter_map(|(name, installed)| installed.then_some(name))
    .collect();
    if options.json {
        println!(
            "{}",
            serde_json::json!({
                "profile": profile,
                "region": config.region,
                "account": identity.account,
                "arn": identity.arn,
                "identity": identity.display_name(),
                "credentials": "valid",
                "shell_hook": shell_hook_active(),
                "ecr_clients": clients,
            })
        );
    } else if !options.quiet {
        println!("{:<14} {}", "Profile", profile.green().bold());
        println!(
            "{:<14} {}",
            "Region",
            config.region.as_deref().unwrap_or("not configured")
        );
        println!("{:<14} {}", "Account", identity.account);
        println!("{:<14} {}", "Identity", identity.display_name());
        println!("{:<14} {}", "Credentials", "valid".green());
        println!(
            "{:<14} {}",
            "Shell hook",
            if shell_hook_active() {
                "active"
            } else {
                "not installed"
            }
        );
        println!(
            "{:<14} {}",
            "ECR clients",
            if clients.is_empty() {
                "none".into()
            } else {
                clients.join(", ")
            }
        );
    }
    Ok(())
}

fn doctor(requested: Option<&String>, options: &Options) -> Result<()> {
    let mut checks = Vec::new();
    let aws_installed = command_exists("aws");
    if aws_installed {
        let version = Command::new("aws")
            .arg("--version")
            .output()
            .ok()
            .map(|output| {
                let detail = output_detail(&output);
                if detail.is_empty() {
                    "installed".into()
                } else {
                    detail
                }
            })
            .unwrap_or_else(|| "installed".into());
        checks.push(DoctorCheck {
            level: CheckLevel::Pass,
            name: "AWS CLI".into(),
            detail: version,
        });
    } else {
        checks.push(DoctorCheck {
            level: CheckLevel::Fail,
            name: "AWS CLI".into(),
            detail: "not found in PATH".into(),
        });
    }

    let config_path = aws_config_path();
    checks.push(DoctorCheck {
        level: if config_path.is_file() {
            CheckLevel::Pass
        } else {
            CheckLevel::Warn
        },
        name: "AWS config".into(),
        detail: config_path.display().to_string(),
    });

    let profiles = discover_profiles()?;
    checks.push(DoctorCheck {
        level: if profiles.is_empty() {
            CheckLevel::Fail
        } else {
            CheckLevel::Pass
        },
        name: "Profiles".into(),
        detail: format!("{} discovered", profiles.len()),
    });
    let state = load_state()?;
    let profile = resolve_profile(requested, &profiles, &state).ok();
    checks.push(DoctorCheck {
        level: if profile.is_some() {
            CheckLevel::Pass
        } else {
            CheckLevel::Fail
        },
        name: "Active profile".into(),
        detail: profile.clone().unwrap_or_else(|| "none selected".into()),
    });

    let mut diagnosed_identity = None;
    let mut diagnosed_config = None;
    if let Some(profile) = profile.as_deref() {
        let config = read_profile_config(profile)?;
        checks.push(DoctorCheck {
            level: if config.region.is_some() {
                CheckLevel::Pass
            } else {
                CheckLevel::Warn
            },
            name: "Region".into(),
            detail: config
                .clone()
                .region
                .unwrap_or_else(|| "not configured; ECR login will be skipped".into()),
        });
        match validate_credentials(profile, options) {
            Ok(identity) => {
                checks.push(DoctorCheck {
                    level: CheckLevel::Pass,
                    name: "Credentials".into(),
                    detail: format!("{} · {}", identity.account, identity.display_name()),
                });
                diagnosed_identity = Some(identity);
            }
            Err(error) => checks.push(DoctorCheck {
                level: CheckLevel::Fail,
                name: "Credentials".into(),
                detail: error,
            }),
        }
        diagnosed_config = Some(config);
    }

    checks.push(DoctorCheck {
        level: if shell_hook_active() {
            CheckLevel::Pass
        } else {
            CheckLevel::Warn
        },
        name: "Shell hook".into(),
        detail: if shell_hook_active() {
            "active".into()
        } else {
            format!("not active; run {}", shell_setup_hint())
        },
    });
    checks.push(DoctorCheck {
        level: CheckLevel::Pass,
        name: "State".into(),
        detail: state_path()?.display().to_string(),
    });

    let docker = command_exists("docker");
    let helm = command_exists("helm");
    checks.push(DoctorCheck {
        level: if docker || helm {
            CheckLevel::Pass
        } else {
            CheckLevel::Warn
        },
        name: "ECR clients".into(),
        detail: match (docker, helm) {
            (true, true) => "Docker, Helm".into(),
            (true, false) => "Docker".into(),
            (false, true) => "Helm".into(),
            (false, false) => "Docker and Helm not found; ECR login is optional".into(),
        },
    });
    if docker
        && let (Some(identity), Some(config)) = (&diagnosed_identity, &diagnosed_config)
        && let Some(region) = config.region.as_deref()
    {
        let registry = ecr_registry(&identity.account, region);
        match docker_credential_helper(&registry) {
            Ok(Some(helper)) => checks.push(DoctorCheck {
                level: CheckLevel::Pass,
                name: "Docker helper".into(),
                detail: format!("{helper} for {registry}"),
            }),
            Ok(None) => checks.push(DoctorCheck {
                level: CheckLevel::Pass,
                name: "Docker helper".into(),
                detail: "not configured; awswap will use docker login".into(),
            }),
            Err(error) => checks.push(DoctorCheck {
                level: CheckLevel::Warn,
                name: "Docker helper".into(),
                detail: error,
            }),
        }
    }

    let static_overrides: Vec<&str> = [
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_SECURITY_TOKEN",
    ]
    .into_iter()
    .filter(|name| env::var_os(name).is_some())
    .collect();
    checks.push(DoctorCheck {
        level: if static_overrides.is_empty() {
            CheckLevel::Pass
        } else {
            CheckLevel::Warn
        },
        name: "Environment".into(),
        detail: if static_overrides.is_empty() {
            "no static credential overrides".into()
        } else {
            format!(
                "{} override profiles; the shell hook clears them",
                static_overrides.join(", ")
            )
        },
    });

    if options.json {
        let values: Vec<_> = checks.iter().map(|check| serde_json::json!({
            "status": match check.level { CheckLevel::Pass => "pass", CheckLevel::Warn => "warn", CheckLevel::Fail => "fail" },
            "name": check.name,
            "detail": check.detail,
        })).collect();
        println!("{}", serde_json::Value::Array(values));
    } else if !options.quiet {
        for check in &checks {
            let symbol = match check.level {
                CheckLevel::Pass => "✓".green().to_string(),
                CheckLevel::Warn => "!".yellow().to_string(),
                CheckLevel::Fail => "✗".red().to_string(),
            };
            println!("{symbol} {:<16} {}", check.name, check.detail);
        }
    }
    let failures = checks
        .iter()
        .filter(|check| check.level == CheckLevel::Fail)
        .count();
    if failures == 0 {
        Ok(())
    } else {
        Err(format!("doctor found {failures} blocking problem(s)"))
    }
}

fn print_completions(shell: Shell) {
    let script = match shell {
        Shell::Bash => {
            r#"_awswap() {
  local current command profiles
  current="${COMP_WORDS[COMP_CWORD]}"
  command="${COMP_WORDS[1]-}"
  profiles="$(command awswap list --quiet 2>/dev/null)"
  if [ "$COMP_CWORD" -eq 1 ]; then
    COMPREPLY=( $(compgen -W "current list status doctor login init completions help - --no-ecr --registry --quiet --json --verbose $profiles" -- "$current") )
  elif [[ "$command" == "init" || "$command" == "completions" ]]; then
    COMPREPLY=( $(compgen -W "bash zsh fish" -- "$current") )
  elif [[ "$command" == "login" || "$command" == "status" || "$command" == "doctor" ]]; then
    COMPREPLY=( $(compgen -W "$profiles" -- "$current") )
  fi
}
complete -F _awswap awswap
"#
        }
        Shell::Zsh => {
            r#"#compdef awswap
_awswap() {
  local -a commands profiles
  commands=(current list status doctor login init completions help - --no-ecr --registry --quiet --json --verbose)
  profiles=("${(@f)$(command awswap list --quiet 2>/dev/null)}")
  if (( CURRENT == 2 )); then
    _describe 'command or profile' commands
    _describe 'AWS profile' profiles
  elif [[ "$words[2]" == init || "$words[2]" == completions ]]; then
    _values 'shell' bash zsh fish
  elif [[ "$words[2]" == login || "$words[2]" == status || "$words[2]" == doctor ]]; then
    _describe 'AWS profile' profiles
  fi
}
compdef _awswap awswap
"#
        }
        Shell::Fish => {
            r#"complete -c awswap -f
complete -c awswap -n '__fish_use_subcommand' -a 'current list status doctor login init completions help -'
complete -c awswap -n '__fish_use_subcommand' -a '(command awswap list --quiet 2>/dev/null)'
complete -c awswap -n '__fish_seen_subcommand_from login status doctor' -a '(command awswap list --quiet 2>/dev/null)'
complete -c awswap -n '__fish_seen_subcommand_from init completions' -a 'bash zsh fish'
complete -c awswap -l no-ecr -d 'Skip ECR authentication'
complete -c awswap -s r -l registry -r -d 'ECR registry or account ID'
complete -c awswap -s q -l quiet -d 'Suppress progress output'
complete -c awswap -l json -d 'Emit JSON'
complete -c awswap -s v -l verbose -d 'Show detailed diagnostics'
"#
        }
    };
    print!("{script}");
}

fn shell_hook_active() -> bool {
    env::var_os("AWSWAP_SHELL_HOOK").is_some()
}

fn shell_setup_hint() -> String {
    let shell = env::var("SHELL")
        .ok()
        .and_then(|path| Path::new(&path).file_name()?.to_str().map(str::to_string))
        .filter(|shell| matches!(shell.as_str(), "bash" | "zsh" | "fish"))
        .unwrap_or_else(|| "zsh".to_string());
    if shell == "fish" {
        "awswap init fish | source".into()
    } else {
        format!("eval \"$(awswap init {shell})\"")
    }
}

fn print_shell_hook(shell: Shell) {
    match shell {
        Shell::Bash | Shell::Zsh => print!(
            r#"awswap() {{
  command awswap "$@"
  local awswap_status=$?
  if [ "$awswap_status" -eq 0 ]; then
    case "${{1-}}" in
      init|list|ls|current|status|doctor|completions|help|version|-h|--help|-V|--version) ;;
      *)
        local awswap_profile
        awswap_profile="$(env AWS_PROFILE= AWS_DEFAULT_PROFILE= awswap current 2>/dev/null)"
        if [ -n "$awswap_profile" ]; then
          export AWS_PROFILE="$awswap_profile"
          export AWS_DEFAULT_PROFILE="$awswap_profile"
          unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN AWS_SECURITY_TOKEN
        fi
        ;;
    esac
  fi
  return "$awswap_status"
}}
export AWSWAP_SHELL_HOOK=1
if awswap_profile="$(env AWS_PROFILE= AWS_DEFAULT_PROFILE= awswap current 2>/dev/null)" && [ -n "$awswap_profile" ]; then
  export AWS_PROFILE="$awswap_profile"
  export AWS_DEFAULT_PROFILE="$awswap_profile"
  unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN AWS_SECURITY_TOKEN
fi
"#
        ),
        Shell::Fish => print!(
            r#"function awswap
    command awswap $argv
    set -l awswap_status $status
    if test $awswap_status -eq 0
        switch (string join ' ' $argv)
            case init list ls current status doctor completions help version '-h' '--help' '-V' '--version'
            case '*'
                set -l awswap_profile (env AWS_PROFILE= AWS_DEFAULT_PROFILE= awswap current 2>/dev/null)
                if test -n "$awswap_profile"
                    set -gx AWS_PROFILE "$awswap_profile"
                    set -gx AWS_DEFAULT_PROFILE "$awswap_profile"
                    set -e AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN AWS_SECURITY_TOKEN
                end
        end
    end
    return $awswap_status
end
set -gx AWSWAP_SHELL_HOOK 1
set -l awswap_profile (env AWS_PROFILE= AWS_DEFAULT_PROFILE= awswap current 2>/dev/null)
if test -n "$awswap_profile"
    set -gx AWS_PROFILE "$awswap_profile"
    set -gx AWS_DEFAULT_PROFILE "$awswap_profile"
    set -e AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN AWS_SECURITY_TOKEN
end
"#
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_profiles_from_config_and_credentials() {
        let contents = r#"
            [default]
            region = us-east-1
            [profile dev]
            sso_session = company
            [sso-session company]
            sso_start_url = https://example.awsapps.com/start
            [services local]
            endpoint_url = http://localhost:4566
            [legacy]
            aws_access_key_id = example
        "#;
        let profiles = parse_profiles(contents);
        assert_eq!(
            profiles,
            ["default", "dev", "legacy"]
                .into_iter()
                .map(str::to_string)
                .collect()
        );
    }

    #[test]
    fn parses_sso_profile_configuration() {
        let contents = r#"
            [default]
            login_session = browser
            region = us-west-2
            [profile dev]
            region = eu-west-1
            sso_session = company
            [sso-session company]
            sso_region = us-east-1
        "#;
        assert_eq!(
            parse_profile_config(contents, "dev"),
            ProfileConfig {
                region: Some("eu-west-1".into()),
                is_sso: true,
                ..ProfileConfig::default()
            }
        );
        assert_eq!(
            parse_profile_config(contents, "default"),
            ProfileConfig {
                region: Some("us-west-2".into()),
                is_login: true,
                ..ProfileConfig::default()
            }
        );
    }

    #[test]
    fn state_round_trips() {
        let state =
            parse_state("current=prod\nprevious=dev\nrecent=prod\nrecent=qa\nunknown=ignored\n");
        assert_eq!(
            state,
            State {
                current: Some("prod".into()),
                previous: Some("dev".into()),
                recent: vec!["prod".into(), "qa".into()],
            }
        );
    }

    #[test]
    fn builds_ecr_registries_for_all_aws_partitions() {
        let account = "123456789012";
        let cases = [
            ("us-east-1", "amazonaws.com"),
            ("cn-north-1", "amazonaws.com.cn"),
            ("us-gov-west-1", "amazonaws.com"),
            ("us-iso-east-1", "c2s.ic.gov"),
            ("us-isob-east-1", "sc2s.sgov.gov"),
            ("eu-isoe-west-1", "cloud.adc-e.uk"),
            ("us-isof-south-1", "csp.hci.ic.gov"),
            ("eusc-de-east-1", "amazonaws.eu"),
        ];

        for (region, suffix) in cases {
            assert_eq!(
                ecr_registry(account, region),
                format!("{account}.dkr.ecr.{region}.{suffix}")
            );
        }
    }

    #[test]
    fn normalizes_registry_urls() {
        assert_eq!(
            normalize_registry("https://123.dkr.ecr.us-east-1.amazonaws.com/repo/"),
            "123.dkr.ecr.us-east-1.amazonaws.com"
        );
        assert_eq!(
            normalize_registry("registry.example.com"),
            "registry.example.com"
        );
    }

    #[test]
    fn computes_edit_distance() {
        assert_eq!(edit_distance("prod", "prod"), 0);
        assert_eq!(edit_distance("prd", "prod"), 1);
        assert_eq!(edit_distance("dev", "production"), 9);
    }

    #[test]
    fn selects_host_specific_or_global_docker_helper() {
        let config = r#"{
            "credsStore": "osxkeychain",
            "credHelpers": {
                "123.dkr.ecr.us-east-1.amazonaws.com": "ecr-login"
            }
        }"#;
        assert_eq!(
            parse_docker_credential_helper(config, "123.dkr.ecr.us-east-1.amazonaws.com").unwrap(),
            Some("ecr-login".into())
        );
        assert_eq!(
            parse_docker_credential_helper(config, "registry.example.com").unwrap(),
            Some("osxkeychain".into())
        );
    }

    #[test]
    fn rejects_invalid_docker_config() {
        assert!(parse_docker_credential_helper("not json", "registry.example.com").is_err());
    }

    #[test]
    fn uses_client_specific_registry_login_commands() {
        let registry = "123.dkr.ecr.us-east-1.amazonaws.com";
        assert_eq!(
            RegistryClient::Docker.login_args(registry),
            ["login", registry, "--username", "AWS", "--password-stdin"]
        );
        assert_eq!(
            RegistryClient::Helm.login_args(registry),
            [
                "registry",
                "login",
                registry,
                "--username",
                "AWS",
                "--password-stdin"
            ]
        );
    }

    #[test]
    fn parses_global_flags_in_any_position() {
        let (args, options) = parse_cli(vec![
            "login".into(),
            "--no-ecr".into(),
            "dev".into(),
            "--registry=123456789012,registry.example.com".into(),
            "--json".into(),
        ])
        .unwrap();
        assert_eq!(args, ["login", "dev"]);
        assert!(options.no_ecr);
        assert!(options.json);
        assert_eq!(options.registries, ["123456789012", "registry.example.com"]);
    }

    #[test]
    fn rejects_conflicting_output_flags() {
        assert!(parse_cli(vec!["--quiet".into(), "--verbose".into()]).is_err());
        assert!(parse_cli(vec!["--unknown".into()]).is_err());
    }

    #[test]
    fn orders_current_previous_and_recent_profiles_first() {
        let profiles = vec!["alpha".into(), "dev".into(), "prod".into(), "qa".into()];
        let state = State {
            current: Some("dev".into()),
            previous: Some("prod".into()),
            recent: vec!["qa".into(), "dev".into()],
        };
        assert_eq!(
            ordered_profiles(&profiles, &state, Some("dev")),
            ["dev", "prod", "qa", "alpha"]
        );
    }

    #[test]
    fn parses_identity_receipt() {
        let identity = parse_identity(
            br#"{"UserId":"ARO:test","Account":"123456789012","Arn":"arn:aws:sts::123456789012:assumed-role/Developer/drew"}"#,
        )
        .unwrap();
        assert_eq!(identity.account, "123456789012");
        assert_eq!(identity.display_name(), "Developer/drew");
    }

    #[test]
    fn classifies_common_aws_failures() {
        assert!(
            classify_aws_error("check failed", "dev", "ExpiredToken: token has expired")
                .unwrap()
                .contains("awswap login dev")
        );
        assert!(
            classify_aws_error("check failed", "prod", "AccessDenied: not authorized")
                .unwrap()
                .contains("IAM permissions")
        );
        assert!(
            classify_aws_error("check failed", "dev", "Could not connect to endpoint URL")
                .unwrap()
                .contains("check the network")
        );
    }
}
