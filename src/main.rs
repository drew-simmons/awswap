use clap::{Args, Parser, Subcommand, ValueEnum};
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
const AFTER_HELP: &str = r#"Environment:
  AWSWAP_HOME            State directory (default: $XDG_STATE_HOME/awswap)
  AWSWAP_NO_ECR          Skip automatic Docker/Helm ECR login
  AWSWAP_ECR_REGISTRIES  Comma-separated registry hosts or account IDs
  NO_COLOR               Disable colored output

Install the hook once so `awswap` updates the current shell:
  eval "$(awswap init zsh)" # use bash or fish as appropriate"#;

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Shell {
    Bash,
    Zsh,
    Fish,
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

    fn config_path(self) -> Option<PathBuf> {
        match self {
            Self::Docker => docker_config_path(),
            Self::Helm => helm_registry_config_path(),
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

#[derive(Args, Debug, Default, Eq, PartialEq)]
struct Options {
    /// Skip automatic Docker/Helm ECR login
    #[arg(long, global = true)]
    no_ecr: bool,

    /// ECR registry hostname or account ID; repeatable
    #[arg(
        short = 'r',
        long = "registry",
        global = true,
        value_name = "VALUE",
        value_delimiter = ',',
        value_parser = parse_registry
    )]
    registries: Vec<String>,

    /// Suppress progress and success output
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    quiet: bool,

    /// Emit machine-readable JSON where supported
    #[arg(long, global = true)]
    json: bool,

    /// Show commands and detailed AWS failures
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Debug, Parser)]
#[command(name = "awswap", version, about = "Quickly switch AWS profiles")]
#[command(
    after_help = AFTER_HELP,
    override_usage = "awswap [OPTIONS] [PROFILE]\n       awswap [OPTIONS] <COMMAND>"
)]
struct Cli {
    #[command(flatten)]
    options: Options,

    /// AWS profile to activate, or `-` to switch to the previous profile
    #[arg(value_name = "PROFILE")]
    profile: Option<String>,

    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Print the active profile
    Current,

    /// List configured profiles
    #[command(visible_alias = "ls")]
    List,

    /// Show identity and integration status
    Status {
        /// AWS profile to inspect
        profile: Option<String>,
    },

    /// Diagnose configuration and credentials
    Doctor {
        /// AWS profile to inspect
        profile: Option<String>,
    },

    /// Refresh AWS and ECR authentication
    Login {
        /// AWS profile to authenticate
        profile: Option<String>,
    },

    /// Print a shell hook
    Init {
        /// Shell to target
        #[arg(value_enum)]
        shell: Shell,
    },

    /// Print shell completions
    Completions {
        /// Shell to target
        #[arg(value_enum)]
        shell: Shell,
    },

    /// Print the version
    #[command(hide = true)]
    Version,
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

#[derive(Debug, Eq, PartialEq)]
struct StatusReport {
    profile: String,
    config: ProfileConfig,
    identity: Identity,
    clients: Vec<&'static str>,
    shell_hook: bool,
}

fn main() -> ExitCode {
    configure_color();
    exit_code(run(Cli::parse()))
}

fn configure_color() {
    if env::var_os("NO_COLOR").is_some() {
        owo_colors::set_override(false);
    }
}

fn exit_code(result: Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.is_empty() => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{} {error}", "error:".red().bold());
            ExitCode::FAILURE
        }
    }
}

fn parse_registry(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        Err("registry cannot be empty".into())
    } else {
        Ok(value.to_string())
    }
}

fn run(cli: Cli) -> Result<()> {
    let Cli {
        options,
        profile,
        command,
    } = cli;
    reject_mixed_action(&profile, &command)?;
    match command {
        Some(command) => run_command(command, &options),
        None => run_switch(profile.as_ref(), &options),
    }
}

fn reject_mixed_action(profile: &Option<String>, command: &Option<CliCommand>) -> Result<()> {
    if profile.is_some() && command.is_some() {
        Err("a profile cannot be used with a command".into())
    } else {
        Ok(())
    }
}

fn run_switch(profile: Option<&String>, options: &Options) -> Result<()> {
    if profile.is_some_and(|profile| profile == "-") {
        switch_previous(options)
    } else {
        switch(profile, options)
    }
}

fn run_command(command: CliCommand, options: &Options) -> Result<()> {
    match command {
        CliCommand::Current => current_profile(options),
        CliCommand::List => list_profiles(options),
        CliCommand::Status { profile } => status(profile.as_ref(), options),
        other => run_command_secondary(other, options),
    }
}

fn run_command_secondary(command: CliCommand, options: &Options) -> Result<()> {
    match command {
        CliCommand::Doctor { profile } => doctor(profile.as_ref(), options),
        CliCommand::Login { profile } => login(profile.as_ref(), options),
        CliCommand::Init { shell } => {
            print_shell_hook(shell);
            Ok(())
        }
        other => run_command_output(other),
    }
}

fn run_command_output(command: CliCommand) -> Result<()> {
    match command {
        CliCommand::Completions { shell } => {
            print_completions(shell);
            Ok(())
        }
        CliCommand::Version => {
            println!("awswap {VERSION}");
            Ok(())
        }
        _ => unreachable!("command was handled by an earlier dispatch stage"),
    }
}

fn switch(requested: Option<&String>, options: &Options) -> Result<()> {
    let (profiles, mut state) = switch_context()?;
    let profile = profile_for_switch(requested, &profiles, &state)?;
    activate(&profile, &mut state, true, options)
}

fn switch_context() -> Result<(Vec<String>, State)> {
    require_command("aws")?;
    let profiles = discover_profiles()?;
    require_profiles(&profiles)?;
    Ok((profiles, load_state()?))
}

fn require_profiles(profiles: &[String]) -> Result<()> {
    if profiles.is_empty() {
        Err("no AWS profiles found; run `aws configure sso` first".into())
    } else {
        Ok(())
    }
}

fn profile_for_switch(
    requested: Option<&String>,
    profiles: &[String],
    state: &State,
) -> Result<String> {
    match requested {
        Some(profile) => named_profile_for_switch(profile, profiles),
        None => unnamed_profile_for_switch(profiles, state),
    }
}

fn unnamed_profile_for_switch(profiles: &[String], state: &State) -> Result<String> {
    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        select_profile(profiles, state)
    } else {
        Err("interactive selection requires a terminal; pass a profile name".into())
    }
}

fn named_profile_for_switch(profile: &str, profiles: &[String]) -> Result<String> {
    ensure_profile_exists(profile, profiles)?;
    Ok(profile.to_string())
}

fn switch_previous(options: &Options) -> Result<()> {
    let (profiles, mut state) = switch_context()?;
    let previous = previous_profile(&state, &profiles)?;
    activate(&previous, &mut state, true, options)
}

fn previous_profile(state: &State, profiles: &[String]) -> Result<String> {
    let previous = state
        .previous
        .clone()
        .ok_or_else(|| "no previous AWS profile".to_string())?;
    ensure_profile_exists(&previous, profiles)?;
    Ok(previous)
}

fn activate(
    profile: &str,
    state: &mut State,
    authenticate_ecr: bool,
    options: &Options,
) -> Result<()> {
    let identity = ensure_credentials(profile, options)?;
    update_state(profile, state)?;
    let profile_config = read_profile_config(profile)?;
    finish_activation(
        profile,
        &identity,
        &profile_config,
        authenticate_ecr,
        options,
    );
    Ok(())
}

fn update_state(profile: &str, state: &mut State) -> Result<()> {
    let old_current = state.current.clone();
    if old_current.as_deref() != Some(profile) {
        state.previous = old_current;
        state.current = Some(profile.to_string());
    }
    state.recent.retain(|recent| recent != profile);
    state.recent.insert(0, profile.to_string());
    state.recent.truncate(8);
    save_state(state)
}

fn finish_activation(
    profile: &str,
    identity: &Identity,
    profile_config: &ProfileConfig,
    authenticate_ecr: bool,
    options: &Options,
) {
    let hook_active = shell_hook_active();
    print_activation(profile, identity, profile_config, hook_active, options);
    maybe_authenticate_ecr(
        profile,
        profile_config,
        authenticate_ecr,
        hook_active,
        options,
    );
    print_shell_tip(hook_active, options);
}

fn print_activation(
    profile: &str,
    identity: &Identity,
    profile_config: &ProfileConfig,
    hook_active: bool,
    options: &Options,
) {
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
}

fn maybe_authenticate_ecr(
    profile: &str,
    profile_config: &ProfileConfig,
    authenticate_ecr: bool,
    hook_active: bool,
    options: &Options,
) {
    if should_authenticate_ecr(authenticate_ecr, options)
        && let Err(error) = login_ecr(profile, profile_config, options)
    {
        eprintln!("{} {error}", "warning:".yellow().bold());
        let state = if hook_active { "active" } else { "selected" };
        eprintln!(
            "{}",
            format!("AWS profile is {state}; retry with `awswap login`.").dimmed()
        );
    }
}

fn should_authenticate_ecr(authenticate_ecr: bool, options: &Options) -> bool {
    authenticate_ecr && !options.no_ecr && !env_flag("AWSWAP_NO_ECR")
}

fn print_shell_tip(hook_active: bool, options: &Options) {
    if should_print_shell_tip(hook_active, options) {
        eprintln!(
            "{} shell unchanged; run {} once to activate future selections",
            "tip:".cyan().bold(),
            shell_setup_hint().cyan()
        );
    }
}

fn should_print_shell_tip(hook_active: bool, options: &Options) -> bool {
    !hook_active && !options.quiet && !options.json && io::stdout().is_terminal()
}

fn login(requested: Option<&String>, options: &Options) -> Result<()> {
    require_command("aws")?;
    let profile = login_profile(requested)?;
    let identity = authenticate_profile(&profile, options)?;
    print_login(&profile, &identity, options);
    Ok(())
}

fn login_profile(requested: Option<&String>) -> Result<String> {
    let profiles = discover_profiles()?;
    let state = load_state()?;
    resolve_profile(requested, &profiles, &state)
}

fn authenticate_profile(profile: &str, options: &Options) -> Result<Identity> {
    refresh_credentials(profile, options)?;
    let identity = validate_credentials(profile, options)?;
    let config = read_profile_config(profile)?;
    login_ecr_if_enabled(profile, &config, options)?;
    Ok(identity)
}

fn login_ecr_if_enabled(profile: &str, config: &ProfileConfig, options: &Options) -> Result<()> {
    if should_authenticate_ecr(true, options) {
        login_ecr(profile, config, options)
    } else {
        Ok(())
    }
}

fn print_login(profile: &str, identity: &Identity, options: &Options) {
    if options.json {
        println!(
            "{}",
            serde_json::json!({"profile": profile, "account": identity.account, "authenticated": true})
        );
    } else if !options.quiet {
        println!("{} {}", "✓ Authenticated".green().bold(), profile.bold());
    }
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
    require_list_profiles(&profiles)?;
    let state = load_state()?;
    let current = effective_profile(&state);
    let ordered = ordered_profiles(&profiles, &state, current.as_deref());
    print_profiles(&ordered, &state, current.as_deref(), options)
}

fn require_list_profiles(profiles: &[String]) -> Result<()> {
    if profiles.is_empty() {
        Err("no AWS profiles found".into())
    } else {
        Ok(())
    }
}

fn print_profiles(
    ordered: &[String],
    state: &State,
    current: Option<&str>,
    options: &Options,
) -> Result<()> {
    if options.json {
        print_profiles_json(ordered, state, current);
        return Ok(());
    }
    print_profiles_text(ordered, state, current, options)
}

fn print_profiles_text(
    ordered: &[String],
    state: &State,
    current: Option<&str>,
    options: &Options,
) -> Result<()> {
    if io::stdout().is_terminal() && !options.quiet {
        return print_profiles_table(ordered, state, current);
    }
    for profile in ordered {
        println!("{profile}");
    }
    Ok(())
}

fn print_profiles_json(ordered: &[String], state: &State, current: Option<&str>) {
    let values: Vec<_> = ordered
        .iter()
        .map(|profile| {
            let config = read_profile_config(profile).unwrap_or_default();
            serde_json::json!({
                "name": profile,
                "current": current == Some(profile.as_str()),
                "previous": state.previous.as_deref() == Some(profile.as_str()),
                "region": config.region,
                "auth": config.auth_label(),
            })
        })
        .collect();
    println!("{}", serde_json::Value::Array(values));
}

fn print_profiles_table(ordered: &[String], state: &State, current: Option<&str>) -> Result<()> {
    let config_contents = read_aws_config_contents()?;
    for choice in profile_choices(ordered, state, current, &config_contents) {
        print_profile_choice(&choice);
    }
    Ok(())
}

fn print_profile_choice(choice: &ProfileChoice) {
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
    read_aws_config_contents().and_then(|config_contents| {
        prompt_for_profile(&ordered, state, current.as_deref(), &config_contents)
    })
}

fn prompt_for_profile(
    ordered: &[String],
    state: &State,
    current: Option<&str>,
    config_contents: &str,
) -> Result<String> {
    let choices = profile_choices(ordered, state, current, config_contents);
    Select::new("AWS profile", choices)
        .with_starting_cursor(0)
        .with_page_size(ordered.len().min(12))
        .with_help_message("↑↓ move • type filter • enter select • esc cancel")
        .prompt()
        .map(|choice| choice.name)
        .map_err(selection_error)
}

fn selection_error(error: InquireError) -> String {
    match error {
        InquireError::OperationCanceled | InquireError::OperationInterrupted => String::new(),
        other => format!("could not read selection: {other}"),
    }
}

fn ensure_credentials(profile: &str, options: &Options) -> Result<Identity> {
    match validate_credentials(profile, options) {
        Ok(identity) => Ok(identity),
        Err(first_error) => refresh_expired_credentials(profile, options, first_error),
    }
}

fn refresh_expired_credentials(
    profile: &str,
    options: &Options,
    first_error: String,
) -> Result<Identity> {
    if !first_error.contains("`awswap login") {
        return Err(first_error);
    }
    print_refresh_notice(profile, options);
    refresh_credentials(profile, options).map_err(|login_error| {
        format!("credentials are unavailable ({first_error}); {login_error}")
    })?;
    validate_credentials(profile, options)
}

fn print_refresh_notice(profile: &str, options: &Options) {
    if !options.quiet && !options.json {
        eprintln!(
            "{} credentials for {} need refreshing",
            "auth:".yellow().bold(),
            profile.bold()
        );
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
    let login_args = credential_login_args(profile, &config)?;
    print_login_notice(profile, options);
    run_aws_login(profile, login_args, options)
}

fn credential_login_args<'a>(profile: &str, config: &ProfileConfig) -> Result<&'a [&'a str]> {
    if config.is_sso {
        Ok(&["sso", "login"])
    } else if config.is_login {
        Ok(&["login"])
    } else {
        Err(format!(
            "profile '{profile}' is not configured for SSO or `aws login`; refresh its credentials manually"
        ))
    }
}

fn print_login_notice(profile: &str, options: &Options) {
    if !options.quiet && !options.json {
        eprintln!(
            "{} opening AWS sign-in for {}…",
            "auth:".cyan().bold(),
            profile.bold()
        );
    }
}

fn run_aws_login(profile: &str, login_args: &[&str], options: &Options) -> Result<()> {
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
    login_ecr_registries(profile, config, options, &registries)
}

fn login_ecr_registries(
    profile: &str,
    config: &ProfileConfig,
    options: &Options,
    registries: &[String],
) -> Result<()> {
    let clients = installed_registry_clients()?;
    let region = config
        .region
        .as_deref()
        .ok_or_else(|| format!("profile '{profile}' has no region; skipped ECR login"))?;
    let password = ecr_password(profile, region, options)?;
    let failures = registry_login_failures(profile, registries, &clients, &password, options);
    finish_registry_logins(failures)
}

fn installed_registry_clients() -> Result<Vec<RegistryClient>> {
    let clients = [RegistryClient::Docker, RegistryClient::Helm]
        .into_iter()
        .filter(|client| command_exists(client.command()))
        .collect::<Vec<_>>();
    if clients.is_empty() {
        Err("Docker and Helm are not installed; skipped ECR login".into())
    } else {
        Ok(clients)
    }
}

fn ecr_password(profile: &str, region: &str, options: &Options) -> Result<Vec<u8>> {
    options.progress(format!("Requesting ECR credentials in {region}…"));
    let output = aws_output(
        profile,
        &["ecr", "get-login-password", "--region", region],
        options,
    )?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(aws_command_error(
            "could not get an ECR login token",
            profile,
            &output,
            options.verbose,
        ))
    }
}

fn registry_login_failures(
    profile: &str,
    registries: &[String],
    clients: &[RegistryClient],
    password: &[u8],
    options: &Options,
) -> Vec<String> {
    let mut failures = Vec::new();
    for registry in registries {
        failures.extend(registry_failures(
            profile, registry, clients, password, options,
        ));
    }
    failures
}

fn registry_failures(
    profile: &str,
    registry: &str,
    clients: &[RegistryClient],
    password: &[u8],
    options: &Options,
) -> Vec<String> {
    let mut failures = Vec::new();
    for &client in clients {
        match registry_login(client, profile, registry, password) {
            Ok(method) => print_registry_login(client, registry, method, options),
            Err(error) => failures.push(error),
        }
    }
    failures
}

fn print_registry_login(
    client: RegistryClient,
    registry: &str,
    method: RegistryLoginMethod,
    options: &Options,
) {
    if !options.quiet && !options.json {
        let detail = registry_login_detail(method);
        eprintln!("{} {:<7} {registry}{detail}", "✓".green(), client.label());
    }
}

fn registry_login_detail(method: RegistryLoginMethod) -> String {
    match method {
        RegistryLoginMethod::Password => String::new(),
        RegistryLoginMethod::EcrCredentialHelper => format!("  {}", "(ecr-login)".dimmed()),
    }
}

fn finish_registry_logins(failures: Vec<String>) -> Result<()> {
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn ecr_dns_suffix(region: &str) -> &'static str {
    const SUFFIXES: [(&str, &str); 7] = [
        ("cn-", "amazonaws.com.cn"),
        ("us-gov-", "amazonaws.com"),
        ("us-iso-", "c2s.ic.gov"),
        ("us-isob-", "sc2s.sgov.gov"),
        ("eu-isoe-", "cloud.adc-e.uk"),
        ("us-isof-", "csp.hci.ic.gov"),
        ("eusc-", "amazonaws.eu"),
    ];
    SUFFIXES
        .into_iter()
        .find_map(|(prefix, suffix)| region.starts_with(prefix).then_some(suffix))
        .unwrap_or("amazonaws.com")
}

fn ecr_registry(account: &str, region: &str) -> String {
    format!("{account}.dkr.ecr.{region}.{}", ecr_dns_suffix(region))
}

fn ecr_registries(profile: &str, config: &ProfileConfig, options: &Options) -> Result<Vec<String>> {
    match config.region.as_deref() {
        Some(region) => ecr_registries_for_region(profile, region, options),
        None => Ok(Vec::new()),
    }
}

fn ecr_registries_for_region(
    profile: &str,
    region: &str,
    options: &Options,
) -> Result<Vec<String>> {
    let configured = configured_ecr_registries(options);
    if configured.is_empty() {
        discover_account_registry(profile, region, options)
    } else {
        Ok(normalize_registries(configured, region))
    }
}

fn configured_ecr_registries(options: &Options) -> Vec<String> {
    if options.registries.is_empty() {
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
    }
}

fn normalize_registries(configured: Vec<String>, region: &str) -> Vec<String> {
    let mut registries = BTreeSet::new();
    for item in configured {
        registries.insert(normalize_registry_item(&item, region));
    }
    registries.into_iter().collect()
}

fn normalize_registry_item(item: &str, region: &str) -> String {
    if item.chars().all(|character| character.is_ascii_digit()) {
        ecr_registry(item, region)
    } else {
        normalize_registry(item)
    }
}

fn discover_account_registry(
    profile: &str,
    region: &str,
    options: &Options,
) -> Result<Vec<String>> {
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
    account_registry_from_output(profile, region, options, &output)
}

fn account_registry_from_output(
    profile: &str,
    region: &str,
    options: &Options,
    output: &Output,
) -> Result<Vec<String>> {
    if !output.status.success() {
        return Err(aws_command_error(
            "could not determine the AWS account",
            profile,
            output,
            options.verbose,
        ));
    }
    let account = String::from_utf8_lossy(&output.stdout).trim().to_string();
    registry_for_account(&account, region)
}

fn registry_for_account(account: &str, region: &str) -> Result<Vec<String>> {
    if account.is_empty() || account == "None" {
        return Err("AWS returned no account ID; skipped ECR login".into());
    }
    Ok(vec![ecr_registry(account, region)])
}

fn registry_login(
    client: RegistryClient,
    profile: &str,
    registry: &str,
    password: &[u8],
) -> Result<RegistryLoginMethod> {
    if uses_ecr_credential_helper(client, registry)? {
        validate_ecr_credential_helper(profile, registry)?;
        return Ok(RegistryLoginMethod::EcrCredentialHelper);
    }
    password_registry_login(client, registry, password)
}

fn uses_ecr_credential_helper(client: RegistryClient, registry: &str) -> Result<bool> {
    Ok(credential_helper(client, registry)?.as_deref() == Some("ecr-login"))
}

fn password_registry_login(
    client: RegistryClient,
    registry: &str,
    password: &[u8],
) -> Result<RegistryLoginMethod> {
    let output = run_password_registry_login(client, registry, password)?;
    if !should_retry_helm_login(client, &output) {
        return finish_registry_login(output, client.command(), registry);
    }

    retry_helm_login(registry, password, &output)
}

fn retry_helm_login(
    registry: &str,
    password: &[u8],
    first_output: &Output,
) -> Result<RegistryLoginMethod> {
    let first_error = command_error(&format!("helm login to {registry} failed"), first_output);
    clear_helm_registry_credential(registry).map_err(|logout_error| {
        format!("{first_error}; automatic Keychain cleanup failed: {logout_error}")
    })?;
    let retry = run_password_registry_login(RegistryClient::Helm, registry, password)?;
    finish_registry_login(retry, RegistryClient::Helm.command(), registry)
}

fn should_retry_helm_login(client: RegistryClient, output: &Output) -> bool {
    client == RegistryClient::Helm && !output.status.success() && is_duplicate_keychain_item(output)
}

fn run_password_registry_login(
    client: RegistryClient,
    registry: &str,
    password: &[u8],
) -> Result<Output> {
    let command = client.command();
    let mut child = Command::new(command)
        .args(client.login_args(registry))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start {command}: {error}"))?;
    send_registry_password(&mut child, command, password)?;
    child
        .wait_with_output()
        .map_err(|error| format!("could not wait for {command}: {error}"))
}

fn send_registry_password(
    child: &mut std::process::Child,
    command: &str,
    password: &[u8],
) -> Result<()> {
    child
        .stdin
        .take()
        .ok_or_else(|| format!("could not open {command} input"))?
        .write_all(password)
        .map_err(|error| format!("could not send credentials to {command}: {error}"))
}

fn finish_registry_login(
    output: Output,
    command: &str,
    registry: &str,
) -> Result<RegistryLoginMethod> {
    if output.status.success() {
        Ok(RegistryLoginMethod::Password)
    } else {
        Err(command_error(
            &format!("{command} login to {registry} failed"),
            &output,
        ))
    }
}

fn is_duplicate_keychain_item(output: &Output) -> bool {
    let detail = output_detail(output).to_ascii_lowercase();
    detail.contains("-25299")
        || detail.contains("the specified item already exists in the keychain")
}

fn clear_helm_registry_credential(registry: &str) -> Result<()> {
    let output = Command::new("helm")
        .args(["registry", "logout", registry])
        .output()
        .map_err(|error| format!("could not start helm: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error(
            &format!("helm logout from {registry} failed"),
            &output,
        ))
    }
}

fn credential_helper(client: RegistryClient, registry: &str) -> Result<Option<String>> {
    let Some(path) = client.config_path() else {
        return Ok(None);
    };
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    parse_credential_helper(&contents, registry)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))
}

fn parse_credential_helper(
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
    send_registry_to_helper(&mut child, command, registry)?;
    finish_credential_helper(child, command, registry)
}

fn send_registry_to_helper(
    child: &mut std::process::Child,
    command: &str,
    registry: &str,
) -> Result<()> {
    writeln!(
        child
            .stdin
            .take()
            .ok_or_else(|| format!("could not open {command} input"))?,
        "{registry}"
    )
    .map_err(|error| format!("could not send registry to {command}: {error}"))
}

fn finish_credential_helper(
    child: std::process::Child,
    command: &str,
    registry: &str,
) -> Result<()> {
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
    profiles.extend(discover_profiles_from_aws());
    profiles.extend(discover_profiles_from_files());
    Ok(profiles.into_iter().collect())
}

fn discover_profiles_from_aws() -> BTreeSet<String> {
    Command::new("aws")
        .args(["configure", "list-profiles"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| parse_profile_list(&output.stdout))
        .unwrap_or_default()
}

fn parse_profile_list(output: &[u8]) -> BTreeSet<String> {
    String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .filter(|profile| !profile.is_empty())
        .map(str::to_string)
        .collect()
}

fn discover_profiles_from_files() -> BTreeSet<String> {
    let mut profiles = BTreeSet::new();
    for path in [aws_config_path(), aws_credentials_path()] {
        if let Ok(contents) = fs::read_to_string(path) {
            profiles.extend(parse_profiles(&contents));
        }
    }
    profiles
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
    let target = profile_section(profile);
    let mut current_section = String::new();
    let mut config = ProfileConfig::default();

    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if let Some(section) = parse_section(line) {
            current_section = section.to_string();
        } else if current_section == target {
            apply_profile_line(&mut config, line);
        }
    }
    config
}

fn profile_section(profile: &str) -> String {
    if profile == "default" {
        "default".to_string()
    } else {
        format!("profile {profile}")
    }
}

fn apply_profile_line(config: &mut ProfileConfig, line: &str) {
    let Some((raw_key, raw_value)) = line.split_once('=') else {
        return;
    };
    let value = raw_value.trim();
    if value.is_empty() {
        return;
    }
    apply_profile_property(config, raw_key.trim(), value);
}

fn apply_profile_property(config: &mut ProfileConfig, key: &str, value: &str) {
    if apply_basic_profile_property(config, key, value) {
        return;
    }
    if apply_role_profile_property(config, key, value) {
        return;
    }
    apply_auth_profile_property(config, key);
}

fn apply_basic_profile_property(config: &mut ProfileConfig, key: &str, value: &str) -> bool {
    match key {
        "region" => config.region = Some(value.to_string()),
        "sso_account_id" => config.account_id = Some(value.to_string()),
        "source_profile" => config.source_profile = Some(value.to_string()),
        _ => return false,
    }
    true
}

fn apply_role_profile_property(config: &mut ProfileConfig, key: &str, value: &str) -> bool {
    match key {
        "role_name" => config.role_name = Some(value.to_string()),
        "role_arn" => apply_role_arn(config, value),
        _ => return false,
    }
    true
}

fn apply_role_arn(config: &mut ProfileConfig, value: &str) {
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

fn apply_auth_profile_property(config: &mut ProfileConfig, key: &str) {
    match key {
        "sso_session" | "sso_start_url" => config.is_sso = true,
        "login_session" => config.is_login = true,
        _ => {}
    }
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
    [
        expired_aws_error(context, profile, &normalized),
        unavailable_aws_error(context, profile, &normalized),
        denied_aws_error(context, profile, &normalized),
        network_aws_error(context, &normalized),
        region_aws_error(context, profile, &normalized),
        missing_profile_aws_error(context, profile, &normalized),
    ]
    .into_iter()
    .flatten()
    .next()
}

fn contains_any(value: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|pattern| value.contains(pattern))
}

fn expired_aws_error(context: &str, profile: &str, detail: &str) -> Option<String> {
    let expired = contains_any(
        detail,
        &["expiredtoken", "token has expired", "unauthorizedssotoken"],
    ) || detail.contains("sso session") && detail.contains("expired");
    expired.then(|| {
        format!("{context}: credentials for '{profile}' expired; run `awswap login {profile}`")
    })
}

fn unavailable_aws_error(context: &str, profile: &str, detail: &str) -> Option<String> {
    contains_any(
        detail,
        &[
            "unable to locate credentials",
            "invalidclienttokenid",
            "unrecognizedclientexception",
            "credentials could not be loaded",
        ],
    )
    .then(|| {
        format!(
            "{context}: credentials for '{profile}' are unavailable; run `awswap login {profile}`"
        )
    })
}

fn denied_aws_error(context: &str, profile: &str, detail: &str) -> Option<String> {
    contains_any(detail, &["accessdenied", "access denied", "not authorized"])
        .then(|| format!("{context}: access denied for '{profile}'; verify its IAM permissions"))
}

fn network_aws_error(context: &str, detail: &str) -> Option<String> {
    contains_any(
        detail,
        &[
            "could not connect",
            "endpoint url",
            "timed out",
            "name or service not known",
            "temporary failure in name resolution",
        ],
    )
    .then(|| {
        format!("{context}: could not reach AWS; check the network, proxy, and configured region")
    })
}

fn region_aws_error(context: &str, profile: &str, detail: &str) -> Option<String> {
    contains_any(detail, &["you must specify a region", "invalid region"]).then(|| {
        format!(
            "{context}: profile '{profile}' has no valid region; configure one with `aws configure set region <region> --profile {profile}`"
        )
    })
}

fn missing_profile_aws_error(context: &str, profile: &str, detail: &str) -> Option<String> {
    (detail.contains("config profile") && detail.contains("could not be found"))
        .then(|| format!("{context}: profile '{profile}' is missing from the AWS configuration"))
}

fn aws_command_error(context: &str, profile: &str, output: &Output, verbose: bool) -> String {
    let detail = output_detail(output);
    match classify_aws_error(context, profile, &detail) {
        Some(classified) => classified_aws_error(classified, &detail, verbose),
        None => command_error(context, output),
    }
}

fn classified_aws_error(classified: String, detail: &str, verbose: bool) -> String {
    if verbose && !detail.is_empty() {
        format!("{classified}\nAWS CLI: {detail}")
    } else {
        classified
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
    state_dir_from(
        env::var_os("AWSWAP_HOME"),
        env::var_os("XDG_STATE_HOME"),
        home_dir(),
    )
}

fn state_dir_from(
    awswap_home: Option<std::ffi::OsString>,
    xdg_state_home: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
) -> Result<PathBuf> {
    awswap_home
        .map(PathBuf::from)
        .or_else(|| {
            xdg_state_home
                .map(PathBuf::from)
                .map(|path| path.join("awswap"))
        })
        .or_else(|| home.map(|path| path.join(".local/state/awswap")))
        .ok_or_else(|| "could not determine the state directory; set AWSWAP_HOME".into())
}

fn state_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("state"))
}

fn load_state() -> Result<State> {
    let path = state_path()?;
    read_state(&path).map(|contents| parse_state(&contents))
}

fn read_state(path: &Path) -> Result<String> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    Ok(contents)
}

fn parse_state(contents: &str) -> State {
    let mut state = State::default();
    for line in contents.lines() {
        apply_state_line(&mut state, line);
    }
    state.recent.truncate(8);
    state
}

fn apply_state_line(state: &mut State, line: &str) {
    match state_entry(line) {
        Some(("current", value)) => state.current = Some(value.to_string()),
        Some(("previous", value)) => state.previous = Some(value.to_string()),
        Some(("recent", value)) => add_recent(state, value),
        _ => {}
    }
}

fn state_entry(line: &str) -> Option<(&'static str, &str)> {
    [
        ("current", "current="),
        ("previous", "previous="),
        ("recent", "recent="),
    ]
    .into_iter()
    .find_map(|(name, prefix)| {
        line.strip_prefix(prefix)
            .filter(|value| !value.is_empty())
            .map(|value| (name, value))
    })
}

fn add_recent(state: &mut State, value: &str) {
    if !state.recent.iter().any(|recent| recent == value) {
        state.recent.push(value.to_string());
    }
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

fn helm_registry_config_path() -> Option<PathBuf> {
    env::var_os("HELM_REGISTRY_CONFIG")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(helm_registry_config_from_helm)
}

fn helm_registry_config_from_helm() -> Option<PathBuf> {
    let output = Command::new("helm").arg("env").output().ok()?;
    output
        .status
        .success()
        .then(|| parse_helm_registry_config(&String::from_utf8_lossy(&output.stdout)))
        .flatten()
}

fn parse_helm_registry_config(contents: &str) -> Option<PathBuf> {
    contents
        .lines()
        .find_map(|line| line.trim().strip_prefix("HELM_REGISTRY_CONFIG="))
        .map(|value| value.trim().trim_matches('"'))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
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
    let profile = resolve_status_profile(requested)?;
    let report = build_status_report(profile, options)?;
    print_status(&report, options);
    Ok(())
}

fn resolve_status_profile(requested: Option<&String>) -> Result<String> {
    let profiles = discover_profiles()?;
    let state = load_state()?;
    resolve_profile(requested, &profiles, &state)
}

fn build_status_report(profile: String, options: &Options) -> Result<StatusReport> {
    let config = read_profile_config(&profile)?;
    let identity = validate_credentials(&profile, options)?;
    Ok(StatusReport {
        profile,
        config,
        identity,
        clients: installed_client_names(),
        shell_hook: shell_hook_active(),
    })
}

fn installed_client_names() -> Vec<&'static str> {
    [
        ("Docker", command_exists("docker")),
        ("Helm", command_exists("helm")),
    ]
    .into_iter()
    .filter_map(|(name, installed)| installed.then_some(name))
    .collect()
}

fn print_status(report: &StatusReport, options: &Options) {
    if options.json {
        print_status_json(report);
    } else if !options.quiet {
        print_status_text(report);
    }
}

fn print_status_json(report: &StatusReport) {
    println!(
        "{}",
        serde_json::json!({
            "profile": report.profile,
            "region": report.config.region,
            "account": report.identity.account,
            "arn": report.identity.arn,
            "identity": report.identity.display_name(),
            "credentials": "valid",
            "shell_hook": report.shell_hook,
            "ecr_clients": report.clients,
        })
    );
}

fn print_status_text(report: &StatusReport) {
    println!("{:<14} {}", "Profile", report.profile.green().bold());
    println!(
        "{:<14} {}",
        "Region",
        report.config.region.as_deref().unwrap_or("not configured")
    );
    println!("{:<14} {}", "Account", report.identity.account);
    println!("{:<14} {}", "Identity", report.identity.display_name());
    println!("{:<14} {}", "Credentials", "valid".green());
    println!(
        "{:<14} {}",
        "Shell hook",
        shell_hook_label(report.shell_hook)
    );
    println!(
        "{:<14} {}",
        "ECR clients",
        client_names_label(&report.clients)
    );
}

fn shell_hook_label(active: bool) -> &'static str {
    if active { "active" } else { "not installed" }
}

fn client_names_label(clients: &[&str]) -> String {
    if clients.is_empty() {
        "none".into()
    } else {
        clients.join(", ")
    }
}

struct DoctorContext {
    profiles: Vec<String>,
    profile: Option<String>,
    config_path: PathBuf,
    state_path: PathBuf,
    docker: bool,
    helm: bool,
}

#[derive(Default)]
struct ProfileDiagnosis {
    checks: Vec<DoctorCheck>,
    identity: Option<Identity>,
    config: Option<ProfileConfig>,
}

fn doctor(requested: Option<&String>, options: &Options) -> Result<()> {
    let checks = doctor_checks(requested, options)?;
    print_doctor_checks(&checks, options);
    doctor_result(&checks)
}

fn doctor_checks(requested: Option<&String>, options: &Options) -> Result<Vec<DoctorCheck>> {
    let context = doctor_context(requested)?;
    let diagnosis = diagnose_profile(context.profile.as_deref(), options)?;
    Ok(assemble_doctor_checks(&context, diagnosis))
}

fn doctor_context(requested: Option<&String>) -> Result<DoctorContext> {
    let profiles = discover_profiles()?;
    let state = load_state()?;
    Ok(DoctorContext {
        profile: resolve_profile(requested, &profiles, &state).ok(),
        profiles,
        config_path: aws_config_path(),
        state_path: state_path()?,
        docker: command_exists("docker"),
        helm: command_exists("helm"),
    })
}

fn diagnose_profile(profile: Option<&str>, options: &Options) -> Result<ProfileDiagnosis> {
    match profile {
        Some(profile) => diagnose_named_profile(profile, options),
        None => Ok(ProfileDiagnosis::default()),
    }
}

fn diagnose_named_profile(profile: &str, options: &Options) -> Result<ProfileDiagnosis> {
    let config = read_profile_config(profile)?;
    let (credentials, identity) = credentials_check(profile, options);
    Ok(ProfileDiagnosis {
        checks: vec![region_check(&config), credentials],
        identity,
        config: Some(config),
    })
}

fn credentials_check(profile: &str, options: &Options) -> (DoctorCheck, Option<Identity>) {
    match validate_credentials(profile, options) {
        Ok(identity) => (
            DoctorCheck {
                level: CheckLevel::Pass,
                name: "Credentials".into(),
                detail: format!("{} · {}", identity.account, identity.display_name()),
            },
            Some(identity),
        ),
        Err(error) => (
            DoctorCheck {
                level: CheckLevel::Fail,
                name: "Credentials".into(),
                detail: error,
            },
            None,
        ),
    }
}

fn assemble_doctor_checks(
    context: &DoctorContext,
    mut diagnosis: ProfileDiagnosis,
) -> Vec<DoctorCheck> {
    let mut checks = vec![
        aws_cli_check(),
        aws_config_check(&context.config_path),
        profiles_check(&context.profiles),
        active_profile_check(context.profile.as_deref()),
    ];
    checks.append(&mut diagnosis.checks);
    checks.extend([
        shell_hook_check(),
        state_check(&context.state_path),
        ecr_clients_check(context.docker, context.helm),
    ]);
    checks.extend(docker_helper_check(context, &diagnosis));
    checks.push(environment_check());
    checks
}

fn aws_cli_check() -> DoctorCheck {
    if command_exists("aws") {
        DoctorCheck {
            level: CheckLevel::Pass,
            name: "AWS CLI".into(),
            detail: aws_cli_version().unwrap_or_else(|| "installed".into()),
        }
    } else {
        DoctorCheck {
            level: CheckLevel::Fail,
            name: "AWS CLI".into(),
            detail: "not found in PATH".into(),
        }
    }
}

fn aws_cli_version() -> Option<String> {
    Command::new("aws")
        .arg("--version")
        .output()
        .ok()
        .map(|output| output_detail(&output))
        .filter(|detail| !detail.is_empty())
}

fn aws_config_check(path: &Path) -> DoctorCheck {
    DoctorCheck {
        level: pass_or_warn(path.is_file()),
        name: "AWS config".into(),
        detail: path.display().to_string(),
    }
}

fn profiles_check(profiles: &[String]) -> DoctorCheck {
    DoctorCheck {
        level: pass_or_fail(!profiles.is_empty()),
        name: "Profiles".into(),
        detail: format!("{} discovered", profiles.len()),
    }
}

fn active_profile_check(profile: Option<&str>) -> DoctorCheck {
    DoctorCheck {
        level: pass_or_fail(profile.is_some()),
        name: "Active profile".into(),
        detail: profile.unwrap_or("none selected").to_string(),
    }
}

fn region_check(config: &ProfileConfig) -> DoctorCheck {
    DoctorCheck {
        level: pass_or_warn(config.region.is_some()),
        name: "Region".into(),
        detail: config
            .region
            .clone()
            .unwrap_or_else(|| "not configured; ECR login will be skipped".into()),
    }
}

fn shell_hook_check() -> DoctorCheck {
    let active = shell_hook_active();
    DoctorCheck {
        level: pass_or_warn(active),
        name: "Shell hook".into(),
        detail: shell_hook_detail(active),
    }
}

fn shell_hook_detail(active: bool) -> String {
    if active {
        "active".into()
    } else {
        format!("not active; run {}", shell_setup_hint())
    }
}

fn state_check(path: &Path) -> DoctorCheck {
    DoctorCheck {
        level: CheckLevel::Pass,
        name: "State".into(),
        detail: path.display().to_string(),
    }
}

fn ecr_clients_check(docker: bool, helm: bool) -> DoctorCheck {
    DoctorCheck {
        level: pass_or_warn(docker || helm),
        name: "ECR clients".into(),
        detail: ecr_clients_detail(docker, helm).into(),
    }
}

fn ecr_clients_detail(docker: bool, helm: bool) -> &'static str {
    const DETAILS: [&str; 4] = [
        "Docker and Helm not found; ECR login is optional",
        "Helm",
        "Docker",
        "Docker, Helm",
    ];
    DETAILS[usize::from(docker) * 2 + usize::from(helm)]
}

fn docker_helper_check(
    context: &DoctorContext,
    diagnosis: &ProfileDiagnosis,
) -> Option<DoctorCheck> {
    context.docker.then_some(())?;
    let identity = diagnosis.identity.as_ref()?;
    let config = diagnosis.config.as_ref()?;
    let region = config.region.as_deref()?;
    let registry = ecr_registry(&identity.account, region);
    Some(docker_helper_result(
        &registry,
        credential_helper(RegistryClient::Docker, &registry),
    ))
}

fn docker_helper_result(registry: &str, result: Result<Option<String>>) -> DoctorCheck {
    match result {
        Ok(Some(helper)) => DoctorCheck {
            level: CheckLevel::Pass,
            name: "Docker helper".into(),
            detail: format!("{helper} for {registry}"),
        },
        Ok(None) => DoctorCheck {
            level: CheckLevel::Pass,
            name: "Docker helper".into(),
            detail: "not configured; awswap will use docker login".into(),
        },
        Err(error) => DoctorCheck {
            level: CheckLevel::Warn,
            name: "Docker helper".into(),
            detail: error,
        },
    }
}

fn environment_check() -> DoctorCheck {
    let overrides: Vec<&str> = [
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_SECURITY_TOKEN",
    ]
    .into_iter()
    .filter(|name| env::var_os(name).is_some())
    .collect();
    DoctorCheck {
        level: pass_or_warn(overrides.is_empty()),
        name: "Environment".into(),
        detail: environment_detail(&overrides),
    }
}

fn environment_detail(overrides: &[&str]) -> String {
    if overrides.is_empty() {
        "no static credential overrides".into()
    } else {
        format!(
            "{} override profiles; the shell hook clears them",
            overrides.join(", ")
        )
    }
}

fn pass_or_warn(passed: bool) -> CheckLevel {
    if passed {
        CheckLevel::Pass
    } else {
        CheckLevel::Warn
    }
}

fn pass_or_fail(passed: bool) -> CheckLevel {
    if passed {
        CheckLevel::Pass
    } else {
        CheckLevel::Fail
    }
}

fn print_doctor_checks(checks: &[DoctorCheck], options: &Options) {
    if options.json {
        print_doctor_json(checks);
    } else if !options.quiet {
        print_doctor_text(checks);
    }
}

fn print_doctor_json(checks: &[DoctorCheck]) {
    let values: Vec<_> = checks
        .iter()
        .map(|check| {
            serde_json::json!({
                "status": check_level_name(check.level),
                "name": check.name,
                "detail": check.detail,
            })
        })
        .collect();
    println!("{}", serde_json::Value::Array(values));
}

fn print_doctor_text(checks: &[DoctorCheck]) {
    for check in checks {
        println!(
            "{} {:<16} {}",
            check_level_symbol(check.level),
            check.name,
            check.detail
        );
    }
}

fn check_level_name(level: CheckLevel) -> &'static str {
    match level {
        CheckLevel::Pass => "pass",
        CheckLevel::Warn => "warn",
        CheckLevel::Fail => "fail",
    }
}

fn check_level_symbol(level: CheckLevel) -> String {
    match level {
        CheckLevel::Pass => "✓".green().to_string(),
        CheckLevel::Warn => "!".yellow().to_string(),
        CheckLevel::Fail => "✗".red().to_string(),
    }
}

fn doctor_result(checks: &[DoctorCheck]) -> Result<()> {
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
    shell_setup_command(&shell)
}

fn shell_setup_command(shell: &str) -> String {
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

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn parse_args(values: &[&str]) -> clap::error::Result<Cli> {
        Cli::try_parse_from(std::iter::once("awswap").chain(values.iter().copied()))
    }

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
            parse_credential_helper(config, "123.dkr.ecr.us-east-1.amazonaws.com").unwrap(),
            Some("ecr-login".into())
        );
        assert_eq!(
            parse_credential_helper(config, "registry.example.com").unwrap(),
            Some("osxkeychain".into())
        );
    }

    #[test]
    fn rejects_invalid_docker_config() {
        assert!(parse_credential_helper("not json", "registry.example.com").is_err());
    }

    #[test]
    fn reads_registry_config_path_from_helm_env() {
        assert_eq!(
            parse_helm_registry_config(
                "HELM_BIN=\"helm\"\nHELM_REGISTRY_CONFIG=\"/home/user/registry/config.json\"\n"
            ),
            Some(PathBuf::from("/home/user/registry/config.json"))
        );
        assert_eq!(parse_helm_registry_config("HELM_BIN=\"helm\"\n"), None);
        assert_eq!(
            parse_helm_registry_config("HELM_REGISTRY_CONFIG=\"\"\n"),
            None
        );
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
        let cli = parse_args(&[
            "login",
            "--no-ecr",
            "dev",
            "--registry=123456789012,registry.example.com",
            "--json",
        ])
        .unwrap();
        assert!(cli.options.no_ecr);
        assert!(cli.options.json);
        assert_eq!(
            cli.options.registries,
            ["123456789012", "registry.example.com"]
        );
        assert!(matches!(
            cli.command,
            Some(CliCommand::Login {
                profile: Some(ref profile)
            }) if profile == "dev"
        ));

        let cli = parse_args(&["--json", "current"]).unwrap();
        assert!(cli.options.json);
        assert!(matches!(cli.command, Some(CliCommand::Current)));
    }

    #[test]
    fn validates_command_line_arguments() {
        assert_eq!(
            parse_args(&["--quiet", "--verbose"]).unwrap_err().kind(),
            clap::error::ErrorKind::ArgumentConflict
        );
        assert_eq!(
            parse_args(&["--unknown"]).unwrap_err().kind(),
            clap::error::ErrorKind::UnknownArgument
        );
        assert_eq!(
            parse_args(&["--registry", "--json", "dev"])
                .unwrap_err()
                .kind(),
            clap::error::ErrorKind::InvalidValue
        );
    }

    #[test]
    fn supports_previous_profile_and_scoped_help() {
        let cli = parse_args(&["-"]).unwrap();
        assert_eq!(cli.profile.as_deref(), Some("-"));
        assert!(cli.command.is_none());

        assert_eq!(
            run(parse_args(&["dev", "current"]).unwrap()),
            Err("a profile cannot be used with a command".into())
        );

        assert_eq!(
            parse_args(&["status", "--help"]).unwrap_err().kind(),
            clap::error::ErrorKind::DisplayHelp
        );
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

    #[test]
    fn covers_pure_output_and_error_helpers() {
        configure_color();
        assert_eq!(exit_code(Ok(())), ExitCode::SUCCESS);
        assert_eq!(exit_code(Err(String::new())), ExitCode::SUCCESS);
        assert_eq!(exit_code(Err("failed".into())), ExitCode::FAILURE);

        assert_eq!(RegistryClient::Docker.command(), "docker");
        assert_eq!(RegistryClient::Helm.command(), "helm");
        assert_eq!(RegistryClient::Docker.label(), "Docker");
        assert_eq!(RegistryClient::Helm.label(), "Helm");

        let configs = [
            (
                ProfileConfig {
                    is_sso: true,
                    ..ProfileConfig::default()
                },
                "SSO",
            ),
            (
                ProfileConfig {
                    is_login: true,
                    ..ProfileConfig::default()
                },
                "login",
            ),
            (
                ProfileConfig {
                    role_name: Some("Admin".into()),
                    ..ProfileConfig::default()
                },
                "role",
            ),
            (ProfileConfig::default(), "credentials"),
        ];
        for (config, expected) in configs {
            assert_eq!(config.auth_label(), expected);
        }

        let options = Options {
            verbose: true,
            ..Options::default()
        };
        options.progress("working");
        options.trace("aws", &["--version"]);

        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            print_completions(shell);
            print_shell_hook(shell);
        }
        assert_eq!(shell_setup_command("fish"), "awswap init fish | source");
        assert!(shell_setup_command("zsh").contains("eval"));
        assert_eq!(shell_hook_label(true), "active");
        assert_eq!(shell_hook_label(false), "not installed");
        assert_eq!(client_names_label(&[]), "none");
        assert_eq!(client_names_label(&["Docker", "Helm"]), "Docker, Helm");

        assert_eq!(selection_error(InquireError::OperationCanceled), "");
        assert_eq!(selection_error(InquireError::OperationInterrupted), "");
        assert!(selection_error(InquireError::NotTTY).contains("could not read"));

        assert!(parse_registry(" ").is_err());
        assert_eq!(parse_registry(" host ").unwrap(), "host");
        assert!(parse_identity(b"not json").is_err());
        assert_eq!(parse_section(" [profile dev] "), Some("profile dev"));
        assert_eq!(parse_section("profile dev"), None);
        assert!(ensure_profile_exists("dev", &["dev".into()]).is_ok());
        assert!(
            ensure_profile_exists("de", &["dev".into()])
                .unwrap_err()
                .contains("did you mean")
        );
        assert!(ensure_profile_exists("other", &["dev".into()]).is_err());

        for (detail, expected) in [
            ("Unable to locate credentials", "unavailable"),
            ("Invalid region", "no valid region"),
            ("Config profile could not be found", "missing"),
        ] {
            assert!(
                classify_aws_error("check", "dev", detail)
                    .unwrap()
                    .contains(expected)
            );
        }
        assert_eq!(classify_aws_error("check", "dev", "other"), None);

        assert_eq!(
            state_dir_from(Some("/state".into()), None, None).unwrap(),
            PathBuf::from("/state")
        );
        assert_eq!(
            state_dir_from(None, Some("/xdg".into()), None).unwrap(),
            PathBuf::from("/xdg/awswap")
        );
        assert_eq!(
            state_dir_from(None, None, Some(PathBuf::from("/home"))).unwrap(),
            PathBuf::from("/home/.local/state/awswap")
        );
        assert!(state_dir_from(None, None, None).is_err());

        assert!(registry_for_account("", "us-east-1").is_err());
        assert!(registry_for_account("None", "us-east-1").is_err());
        assert_eq!(
            registry_for_account("123", "us-east-1").unwrap(),
            ["123.dkr.ecr.us-east-1.amazonaws.com"]
        );

        for result in [
            Ok(Some("ecr-login".into())),
            Ok(None),
            Err("bad config".into()),
        ] {
            let check = docker_helper_result("registry.example.com", result);
            assert_eq!(check.name, "Docker helper");
        }
    }

    #[test]
    fn covers_report_rendering_helpers() {
        let choices = [
            ProfileChoice {
                name: "current".into(),
                marker: "● ",
                region: "us-east-1".into(),
                auth: "SSO",
                account: "1".into(),
            },
            ProfileChoice {
                name: "previous".into(),
                marker: "↩ ",
                region: "us-west-2".into(),
                auth: "role",
                account: "2".into(),
            },
            ProfileChoice {
                name: "other".into(),
                marker: "  ",
                region: "—".into(),
                auth: "credentials",
                account: String::new(),
            },
        ];
        for choice in &choices {
            print_profile_choice(choice);
        }

        let checks = [
            DoctorCheck {
                level: CheckLevel::Pass,
                name: "pass".into(),
                detail: "ok".into(),
            },
            DoctorCheck {
                level: CheckLevel::Warn,
                name: "warn".into(),
                detail: "check".into(),
            },
            DoctorCheck {
                level: CheckLevel::Fail,
                name: "fail".into(),
                detail: "bad".into(),
            },
        ];
        print_doctor_text(&checks);
        assert_eq!(check_level_name(CheckLevel::Pass), "pass");
        assert_eq!(check_level_name(CheckLevel::Warn), "warn");
        assert_eq!(check_level_name(CheckLevel::Fail), "fail");
        assert!(doctor_result(&checks).is_err());

        assert_eq!(pass_or_warn(true), CheckLevel::Pass);
        assert_eq!(pass_or_warn(false), CheckLevel::Warn);
        assert_eq!(pass_or_fail(true), CheckLevel::Pass);
        assert_eq!(pass_or_fail(false), CheckLevel::Fail);
        assert_eq!(
            ecr_clients_detail(false, false),
            "Docker and Helm not found; ECR login is optional"
        );
        assert_eq!(ecr_clients_detail(false, true), "Helm");
        assert_eq!(ecr_clients_detail(true, false), "Docker");
        assert_eq!(ecr_clients_detail(true, true), "Docker, Helm");
        assert!(environment_detail(&[]).contains("no static"));
        assert!(environment_detail(&["AWS_ACCESS_KEY_ID"]).contains("override profiles"));
    }

    #[cfg(unix)]
    #[test]
    fn covers_local_command_and_file_workflows() {
        let root = env::temp_dir().join(format!("awswap-test-{}", std::process::id()));
        let bin = root.join("bin");
        let state = root.join("state");
        let config = root.join("config");
        let credentials = root.join("credentials");
        let docker = root.join("docker");
        let helm_config = root.join("helm-registry.json");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&docker).unwrap();
        fs::write(
            &config,
            "[profile dev]\nregion=us-east-1\nsso_session=company\n[profile prod]\nregion=us-west-2\n",
        )
        .unwrap();
        fs::write(&credentials, "[legacy]\naws_access_key_id=test\n").unwrap();

        write_executable(
            &bin.join("aws"),
            r#"#!/bin/sh
case "$1 $2" in
  "--version ") echo "aws-cli/2.test" ;;
  "configure list-profiles") printf "dev\nprod\n" ;;
  "sts get-caller-identity")
    case "$*" in
      *--query*) echo "123456789012" ;;
      *) echo '{"UserId":"ARO:test","Account":"123456789012","Arn":"arn:aws:sts::123456789012:assumed-role/Developer/test"}' ;;
    esac ;;
  "ecr get-login-password") echo "secret" ;;
  "sso login") exit 0 ;;
  *) exit 1 ;;
esac
"#,
        );
        write_executable(&bin.join("docker"), "#!/bin/sh\n/bin/cat >/dev/null\n");
        write_executable(
            &bin.join("helm"),
            r#"#!/bin/sh
case "$*" in
  "registry login duplicate.example.com "*)
    /bin/cat >/dev/null
    if [ ! -f "$AWSWAP_HOME/helm-logout" ]; then
      echo "Error: The specified item already exists in the keychain. (-25299)" >&2
      exit 1
    fi ;;
  "registry logout duplicate.example.com")
    /usr/bin/touch "$AWSWAP_HOME/helm-logout" ;;
  "registry login denied.example.com "*)
    /bin/cat >/dev/null
    echo "access denied" >&2
    exit 1 ;;
  "registry logout denied.example.com")
    /usr/bin/touch "$AWSWAP_HOME/unexpected-helm-logout" ;;
  *) /bin/cat >/dev/null ;;
esac
"#,
        );
        write_executable(
            &bin.join("docker-credential-ecr-login"),
            "#!/bin/sh\n/bin/cat >/dev/null\n",
        );

        let variables = [
            ("PATH", Some(bin.as_os_str())),
            ("AWSWAP_HOME", Some(state.as_os_str())),
            ("AWS_CONFIG_FILE", Some(config.as_os_str())),
            ("AWS_SHARED_CREDENTIALS_FILE", Some(credentials.as_os_str())),
            ("DOCKER_CONFIG", Some(docker.as_os_str())),
            ("HELM_REGISTRY_CONFIG", Some(helm_config.as_os_str())),
            ("AWS_PROFILE", None),
            ("AWSWAP_NO_ECR", None),
            ("AWSWAP_ECR_REGISTRIES", None),
            ("AWSWAP_SHELL_HOOK", Some(std::ffi::OsStr::new("1"))),
        ];
        let _environment = EnvironmentGuard::set(&variables);

        assert!(command_exists("aws"));
        assert!(!command_exists("missing"));
        assert!(require_command("aws").is_ok());
        assert!(require_command("missing").is_err());
        assert_eq!(state_dir().unwrap(), state);
        assert_eq!(state_path().unwrap(), state.join("state"));
        assert_eq!(aws_config_path(), config);
        assert_eq!(aws_credentials_path(), credentials);
        assert_eq!(docker_config_path(), Some(docker.join("config.json")));
        assert_eq!(helm_registry_config_path(), Some(helm_config.clone()));
        assert_eq!(load_state().unwrap(), State::default());

        let initial = State {
            current: Some("prod".into()),
            previous: None,
            recent: vec!["prod".into()],
        };
        save_state(&initial).unwrap();
        assert_eq!(load_state().unwrap(), initial);
        assert_eq!(discover_profiles().unwrap(), ["dev", "legacy", "prod"]);
        assert_eq!(
            read_profile_config("dev").unwrap().region.as_deref(),
            Some("us-east-1")
        );

        let plain = Options::default();
        let json = Options {
            json: true,
            ..Options::default()
        };
        assert!(switch(Some(&"dev".into()), &plain).is_ok());
        assert!(run(parse_args(&["dev"]).unwrap()).is_ok());
        assert!(switch_previous(&plain).is_ok());
        assert!(run(parse_args(&["-"]).unwrap()).is_ok());
        assert!(profile_for_switch(None, &["dev".into()], &State::default()).is_err());
        assert!(current_profile(&plain).is_ok());
        assert!(current_profile(&json).is_ok());
        assert!(list_profiles(&plain).is_ok());
        assert!(list_profiles(&json).is_ok());
        assert!(status(Some(&"dev".into()), &json).is_ok());
        assert!(doctor(Some(&"dev".into()), &json).is_ok());
        assert!(login(Some(&"dev".into()), &json).is_ok());
        assert!(validate_credentials("dev", &plain).is_ok());
        assert!(refresh_credentials("dev", &plain).is_ok());
        assert!(refresh_credentials("prod", &plain).is_err());
        assert_eq!(
            refresh_expired_credentials("dev", &plain, "other error".into()).unwrap_err(),
            "other error"
        );
        assert!(
            refresh_expired_credentials("dev", &plain, "run `awswap login dev`".into()).is_ok()
        );

        let profile_config = read_profile_config("dev").unwrap();
        assert!(login_ecr("dev", &profile_config, &plain).is_ok());
        assert_eq!(
            credential_helper(RegistryClient::Docker, "registry.example.com").unwrap(),
            None
        );
        fs::write(
            docker.join("config.json"),
            r#"{"credHelpers":{"registry.example.com":"ecr-login"}}"#,
        )
        .unwrap();
        assert_eq!(
            registry_login(
                RegistryClient::Docker,
                "dev",
                "registry.example.com",
                b"secret"
            )
            .unwrap(),
            RegistryLoginMethod::EcrCredentialHelper
        );
        assert_eq!(
            registry_login(
                RegistryClient::Helm,
                "dev",
                "registry.example.com",
                b"secret"
            )
            .unwrap(),
            RegistryLoginMethod::Password
        );
        assert_eq!(
            registry_login(
                RegistryClient::Helm,
                "dev",
                "duplicate.example.com",
                b"secret"
            )
            .unwrap(),
            RegistryLoginMethod::Password
        );
        assert!(state.join("helm-logout").is_file());
        assert!(
            registry_login(RegistryClient::Helm, "dev", "denied.example.com", b"secret")
                .unwrap_err()
                .contains("access denied")
        );
        assert!(!state.join("unexpected-helm-logout").exists());
        assert_eq!(
            credential_helper(RegistryClient::Helm, "registry.example.com").unwrap(),
            None
        );
        fs::write(
            &helm_config,
            r#"{"credHelpers":{"registry.example.com":"ecr-login"}}"#,
        )
        .unwrap();
        assert_eq!(
            registry_login(
                RegistryClient::Helm,
                "dev",
                "registry.example.com",
                b"secret"
            )
            .unwrap(),
            RegistryLoginMethod::EcrCredentialHelper
        );

        let configured = Options {
            registries: vec!["123".into(), "https://registry.example.com/repo".into()],
            ..Options::default()
        };
        assert_eq!(
            ecr_registries("dev", &profile_config, &configured).unwrap(),
            [
                "123.dkr.ecr.us-east-1.amazonaws.com",
                "registry.example.com"
            ]
        );
        print_profiles_table(
            &["dev".into(), "prod".into()],
            &load_state().unwrap(),
            Some("dev"),
        )
        .unwrap();

        let output = Command::new("/bin/sh")
            .args(["-c", "printf 'ExpiredToken' >&2; exit 1"])
            .output()
            .unwrap();
        assert!(command_error("failed", &output).contains("ExpiredToken"));
        assert!(aws_command_error("failed", "dev", &output, true).contains("AWS CLI"));
        let empty_output = Command::new("/bin/sh")
            .args(["-c", "exit 1"])
            .output()
            .unwrap();
        assert!(command_error("failed", &empty_output).contains("exit status"));
        assert!(aws_command_error("failed", "dev", &empty_output, false).contains("exit status"));

        let unreadable_state = root.join("unreadable-state");
        fs::create_dir_all(unreadable_state.join("state")).unwrap();
        assert!(read_state(&unreadable_state.join("state")).is_err());

        for args in [
            &["current"][..],
            &["list", "--json"][..],
            &["status", "dev", "--json"][..],
            &["doctor", "dev", "--json"][..],
            &["login", "dev", "--json"][..],
            &["init", "zsh"][..],
            &["completions", "fish"][..],
            &["version"][..],
        ] {
            assert!(run(parse_args(args).unwrap()).is_ok());
        }

        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    struct EnvironmentGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    #[cfg(unix)]
    impl EnvironmentGuard {
        fn set(values: &[(&'static str, Option<&std::ffi::OsStr>)]) -> Self {
            let previous = values
                .iter()
                .map(|(name, _)| (*name, env::var_os(name)))
                .collect();
            for (name, value) in values {
                // SAFETY: this test is the only test that changes these process variables.
                unsafe {
                    match value {
                        Some(value) => env::set_var(name, value),
                        None => env::remove_var(name),
                    }
                }
            }
            Self(previous)
        }
    }

    #[cfg(unix)]
    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                // SAFETY: this restores the variables changed by EnvironmentGuard::set.
                unsafe {
                    match value {
                        Some(value) => env::set_var(name, value),
                        None => env::remove_var(name),
                    }
                }
            }
        }
    }
}
