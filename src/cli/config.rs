use anyhow::{Context, Result, anyhow};
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};
use std::str::FromStr;
use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    hash::BuildHasher,
    path::{Path, PathBuf},
    time::Duration,
};
use strum::{Display, EnumString};
use tracing::warn;
use url::Url;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub services: HashMap<String, ServiceDetails>,
}

impl Config {
    /// Creates a new `Config` from a YAML file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, parsed, or contains invalid service configurations.
    pub fn new(config_path: PathBuf) -> Result<Self> {
        let file = File::open(config_path)?;

        let config: Self =
            serde_yaml_ng::from_reader(file).context("Failed to parse config file")?;

        // Validate all services after loading
        for (name, service) in &config.services {
            service
                .validate()
                .with_context(|| format!("Invalid configuration for service '{name}'"))?;
        }

        config.warn_about_ungrouped_conflicts();

        Ok(config)
    }

    /// Logs the services whose recovery commands look like they will collide
    /// once they run concurrently.
    ///
    /// A fallback command with no `group` takes no lock, so several can run at
    /// once. That is the point - unrelated services should not queue behind
    /// each other - but it silently reinstates the interleaved output that a
    /// process-wide lock used to prevent, for configs written before groups
    /// existed and therefore declaring none. This says so at start-up instead
    /// of leaving it to be discovered in a scrambled log file.
    fn warn_about_ungrouped_conflicts(&self) {
        for conflict in conflicting_fallback_commands(&self.services) {
            warn!(
                "Services {} {} but are not all in the same 'if_not.group', so their recovery commands will run concurrently and their output may interleave. Give them the same 'if_not.group' to run them one at a time.",
                conflict.services.join(", "),
                conflict.reason.describe()
            );
        }
    }

    #[must_use]
    pub fn get_service(&self, service_name: &str) -> Option<&ServiceDetails> {
        self.services.get(service_name)
    }
}

/// Why a set of ungrouped recovery commands is reported as likely to collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    /// The `cmd` strings are byte-identical, so it is certainly one script.
    IdenticalCommand,

    /// The commands invoke the same script with different arguments. Caught
    /// separately because the strings differ, so [`Self::IdenticalCommand`]
    /// cannot see it - yet the script, and typically its log file, are shared.
    SharedScript,
}

impl ConflictReason {
    const fn describe(self) -> &'static str {
        match self {
            Self::IdenticalCommand => "run an identical command",
            Self::SharedScript => "invoke the same script",
        }
    }
}

/// A set of services whose recovery commands look like they collide without a
/// shared group to serialise them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackConflict {
    pub reason: ConflictReason,

    /// Sorted, so the reported message is stable. `Config::services` is a
    /// `HashMap`, so any order derived from iteration is otherwise arbitrary
    /// and changes between runs.
    pub services: Vec<String>,

    pub key: String,
}

/// Tokens that stand in front of the command they run. They have to be stepped
/// over before the real target can be identified: `sudo /opt/restart.sh a` and
/// `sudo /opt/restart.sh b` name one script, and reading only the first token
/// sees `sudo` and misses it. Privilege escalation and interpreters are routine
/// in a recovery command, so overlooking them would leave the warning blind to
/// one of its most common shapes.
const COMMAND_WRAPPERS: [&str; 12] = [
    "sudo", "doas", "exec", "env", "nice", "sh", "bash", "zsh", "python", "python3", "perl", "ruby",
];

/// Directories holding distribution-managed utilities. A binary living here is
/// not an operator's script, so two services invoking one are no more related
/// than if they had spelled it without a path.
const SYSTEM_BIN_DIRS: [&str; 4] = ["/bin/", "/sbin/", "/usr/bin/", "/usr/sbin/"];

/// Extensions that name a script outright, whichever directory it sits in.
const SCRIPT_EXTENSIONS: [&str; 5] = ["sh", "bash", "py", "pl", "rb"];

/// True when `token` is a wrapper, compared on its file name so that both
/// `env` and `/usr/bin/env` are recognised.
fn is_command_wrapper(token: &str) -> bool {
    let name = Path::new(token)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(token);

    COMMAND_WRAPPERS.contains(&name)
}

/// True for a leading `KEY=value` environment assignment, which prefixes the
/// command rather than being it.
fn is_env_assignment(token: &str) -> bool {
    token
        .split_once('=')
        .is_some_and(|(key, _)| !key.is_empty() && !key.contains('/'))
}

/// The script a command invokes, but only when it names an operator's script
/// rather than a system utility.
///
/// The qualification is what keeps the warning worth reading. Keying on the
/// first token alone would report every pair of `systemctl restart <unit>`
/// services as conflicting, which is both usually wrong and frequent enough
/// that the warning would be tuned out - and then the real cases go unread
/// too. A path alone is not enough of a signal either: `/usr/bin/systemctl` is
/// the same utility as `systemctl`, and treating it as a shared script would
/// reintroduce exactly the noise the extension test was added to avoid.
fn script_key(cmd: &str) -> Option<&str> {
    let mut tokens = cmd.split_whitespace();

    let target = loop {
        let token = tokens.next()?;

        // A flag belongs to the wrapper, and its value may follow as a
        // separate token, so there is no way to know what to skip next.
        // Guessing would key on an argument and report unrelated services, so
        // give up instead: a missed warning is recoverable, a mistrusted one
        // is not.
        if token.starts_with('-') {
            return None;
        }

        if is_env_assignment(token) || is_command_wrapper(token) {
            continue;
        }

        break token;
    };

    let has_script_extension = Path::new(target).extension().is_some_and(|ext| {
        SCRIPT_EXTENSIONS
            .iter()
            .any(|script| ext.eq_ignore_ascii_case(script))
    });

    let is_operator_path = target.contains('/')
        && !SYSTEM_BIN_DIRS
            .iter()
            .any(|dir| target.starts_with(dir) && target.len() > dir.len());

    (has_script_extension || is_operator_path).then_some(target)
}

/// A service whose `if_not.cmd` is a candidate for collision, with the group
/// that does or does not serialise it.
#[derive(Clone, Copy)]
struct Candidate<'a> {
    name: &'a str,
    group: Option<&'a str>,
}

/// True when every candidate shares one declared group, and is therefore
/// already run one at a time.
///
/// A group only protects the services that are actually in it. One service
/// carrying `group: restarts` while another invoking the same script carries
/// none - or a different group - still leaves the two free to overlap, so
/// anything short of unanimity is reported.
fn serialized_together(candidates: &[Candidate<'_>]) -> bool {
    let mut groups = candidates.iter().map(|candidate| candidate.group);

    match groups.next() {
        Some(Some(first)) => groups.all(|group| group == Some(first)),
        _ => false,
    }
}

/// Finds services whose `if_not.cmd` looks like it collides with another's
/// without a shared group to serialise them.
///
/// Split out from the logging so the heuristic can be tested directly on its
/// results rather than by capturing log output.
///
/// Two hazards hid under the process-wide lock, and only this one is visible
/// from the configuration: services sharing a script share its log file, and
/// running them at once interleaves it. The other - a restart storm against a
/// shared resource - cannot be detected here, since nothing in
/// `systemctl restart mariadb` reveals what else lives on that host. That one
/// is left to the changelog.
#[must_use]
pub fn conflicting_fallback_commands<S: BuildHasher>(
    services: &HashMap<String, ServiceDetails, S>,
) -> Vec<FallbackConflict> {
    let mut by_command: BTreeMap<&str, Vec<Candidate<'_>>> = BTreeMap::new();
    let mut by_script: BTreeMap<&str, Vec<Candidate<'_>>> = BTreeMap::new();

    for (name, service) in services {
        let Some(if_not) = &service.expect.if_not else {
            continue;
        };

        let Some(cmd) = if_not.cmd.as_deref() else {
            continue;
        };

        let candidate = Candidate {
            name,
            group: if_not.group_name(),
        };

        by_command.entry(cmd).or_default().push(candidate);

        if let Some(script) = script_key(cmd) {
            by_script.entry(script).or_default().push(candidate);
        }
    }

    let mut conflicts = Vec::new();

    for (cmd, candidates) in by_command {
        if candidates.len() > 1 && !serialized_together(&candidates) {
            conflicts.push(build_conflict(
                ConflictReason::IdenticalCommand,
                cmd,
                &candidates,
            ));
        }
    }

    for (script, candidates) in by_script {
        // An identical command is already reported, and reporting the same set
        // twice would just be noise.
        if candidates.len() > 1
            && !serialized_together(&candidates)
            && !conflicts.iter().any(|c| covers(c, &candidates))
        {
            conflicts.push(build_conflict(
                ConflictReason::SharedScript,
                script,
                &candidates,
            ));
        }
    }

    conflicts
}

fn build_conflict(
    reason: ConflictReason,
    key: &str,
    candidates: &[Candidate<'_>],
) -> FallbackConflict {
    let mut services: Vec<String> = candidates
        .iter()
        .map(|candidate| candidate.name.to_string())
        .collect();
    services.sort();

    FallbackConflict {
        reason,
        services,
        key: key.to_string(),
    }
}

/// True when an already-reported conflict names every service in `candidates`.
fn covers(conflict: &FallbackConflict, candidates: &[Candidate<'_>]) -> bool {
    candidates.iter().all(|candidate| {
        conflict
            .services
            .iter()
            .any(|listed| listed == candidate.name)
    })
}

#[derive(Default, Debug, Clone, Copy, EnumString, Display, Serialize, PartialEq, Eq)]
#[strum(serialize_all = "UPPERCASE")] // Ensures correct casing for HTTP methods
pub enum HttpMethod {
    Connect,
    Delete,

    #[default]
    Get,

    Head,
    Options,
    Patch,
    Post,
    Put,
    Trace,
}

// Custom deserialization for case-insensitive HTTP methods
impl<'de> Deserialize<'de> for HttpMethod {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let method = String::deserialize(deserializer)?;
        Self::from_str(&method.to_uppercase()).map_err(serde::de::Error::custom)
    }
}

const fn default_http_method() -> HttpMethod {
    HttpMethod::Get
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", untagged)]
pub enum BodyType {
    Json(serde_json::Value),       // Covers structured JSON data
    Form(HashMap<String, String>), // Covers form-encoded data
    Text(String),                  // Covers plain text, XML, and other string-based data
}

impl<'de> Deserialize<'de> for BodyType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;

        if let Some(json_value) = value.get("json") {
            return Ok(Self::Json(json_value.clone()));
        }

        if let Some(form) = value.get("form") {
            let form_map = serde_json::from_value::<HashMap<String, String>>(form.clone())
                .map_err(serde::de::Error::custom)?;
            return Ok(Self::Form(form_map));
        }

        if let Some(text) = value.as_str() {
            return Ok(Self::Text(text.to_string()));
        }

        serde_json::from_value(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ServiceDetails {
    #[serde(deserialize_with = "parse_duration")]
    pub every: Duration,

    pub expect: Expect,

    pub follow_redirects: Option<bool>,

    pub headers: Option<HashMap<String, String>>,

    // When unset, matcher-aware defaults apply: 64KB sliding window for
    // body/body_not scans, 512KB buffer for json checks.
    #[serde(rename = "max_bytes", default)]
    pub max_bytes: Option<usize>,

    pub test: Option<String>,

    #[serde(deserialize_with = "parse_duration", default = "default_timeout")]
    pub timeout: Duration,

    pub url: Option<String>,

    #[serde(default = "default_http_method")]
    pub method: HttpMethod,

    #[serde(default)]
    pub body: Option<BodyType>,
}

impl ServiceDetails {
    /// Validates the service configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration is invalid (e.g., missing both URL and test command,
    /// a blank test command, or a `url` that is not a usable http(s) address).
    pub fn validate(&self) -> Result<()> {
        match (&self.url, &self.test) {
            (Some(_), Some(_)) => {
                return Err(anyhow!("Service cannot have both 'url' and 'test'."));
            }
            (None, None) => return Err(anyhow!("Service must have either 'url' or 'test'.")),
            _ => {}
        }

        if let Some(test) = &self.test
            && test.trim().is_empty()
        {
            return Err(anyhow!(
                "'test' cannot be empty; configure a command or remove the service."
            ));
        }

        if self.url.is_none() && self.test.is_some() && self.expect.status.is_none() {
            return Err(anyhow!(
                "Command checks using 'test' must configure 'expect.status'."
            ));
        }

        if let Some(url) = &self.url {
            validate_http_url("url", url)?;
        }

        self.expect.validate()
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    pub status: Option<u16>, // Use for both HTTP & text exit codes
    pub header: Option<HashMap<String, String>>,
    pub body: Option<String>,
    #[serde(rename = "body_not")]
    pub body_not: Option<String>,
    pub json: Option<serde_json::Value>,

    #[serde(rename = "if_not")]
    pub if_not: Option<Action>,
}

impl Expect {
    /// Whether a failed check has a command or HTTP fallback available.
    ///
    /// Exported as `epazote_fallback_configured` so an exhausted fallback can
    /// be distinguished from a service that has no fallback at all.
    #[must_use]
    pub fn has_fallback_action(&self) -> bool {
        self.if_not
            .as_ref()
            .is_some_and(|action| action.cmd.is_some() || action.http.is_some())
    }

    #[must_use]
    pub fn status_matches(&self, actual_status: u16) -> bool {
        self.status.is_none_or(|status| status == actual_status)
    }

    #[must_use]
    pub fn expected_status_i32(&self) -> Option<i32> {
        self.status.map(i32::from)
    }

    /// Validates that the response expectation is internally consistent.
    ///
    /// # Errors
    ///
    /// Returns an error if incompatible expectation types are configured together.
    pub fn validate(&self) -> Result<()> {
        if self.body.is_some() && self.json.is_some() {
            return Err(anyhow!(
                "Expect cannot have both 'body' and 'json' configured."
            ));
        }

        if self.status.is_none()
            && self.body.is_none()
            && self.body_not.is_none()
            && self.json.is_none()
        {
            return Err(anyhow!(
                "Expect must configure at least one of 'status', 'body', 'body_not', or 'json'."
            ));
        }

        if let Some(body) = &self.body {
            validate_body_pattern("expect.body", body)?;
        }

        if let Some(body_not) = &self.body_not {
            validate_body_pattern("expect.body_not", body_not)?;
        }

        if let Some(if_not) = &self.if_not {
            if_not.validate()?;
        }

        Ok(())
    }
}

/// Builds the regex pattern string for a `body`/`body_not` matcher value:
/// `r"..."` values are used as raw regexes (trailing `"` stripped), everything
/// else is escaped to a literal substring match.
pub(crate) fn regex_source(input: &str) -> Result<String> {
    let trimmed_input = input.trim();

    if trimmed_input.is_empty() {
        return Err(anyhow!("Input regex pattern cannot be empty"));
    }

    let raw = trimmed_input.strip_prefix("r\"");

    let pattern = raw.map_or_else(
        // Escape the input to prevent regex injection
        || regex::escape(trimmed_input),
        // If prefix exists, strip suffix and use raw regex
        |raw| raw.strip_suffix('"').unwrap_or(raw).to_string(),
    );

    Ok(pattern)
}

// Reject a URL that can never be requested, at start-up rather than silently on
// every check.
//
// A malformed `url` deserialized cleanly and only failed when the request was
// built - inside the scan, before the failure was recorded and before any
// `if_not` ran. The service reported down forever, its failure streak stayed at
// `0`, and recovery was never attempted, with nothing in the configuration or
// the metrics saying why. The scheme is checked too: reqwest builds an `ftp://`
// request and then refuses to send it, which fails the same way on every check.
fn validate_http_url(field: &str, value: &str) -> Result<()> {
    let url = Url::parse(value)
        .map_err(|source| anyhow!("'{field}' is not a valid URL ({source}): {value:?}"))?;

    match url.scheme() {
        "http" | "https" => {}
        scheme => {
            return Err(anyhow!(
                "'{field}' must use http or https, not '{scheme}': {value:?}"
            ));
        }
    }

    if url.host_str().is_none_or(str::is_empty) {
        return Err(anyhow!("'{field}' has no host: {value:?}"));
    }

    Ok(())
}

// Compile the pattern at config load so an invalid regex halts startup with a
// clear error instead of failing every check at runtime.
fn validate_body_pattern(field: &str, input: &str) -> Result<()> {
    let pattern =
        regex_source(input).with_context(|| format!("invalid '{field}' pattern: {input}"))?;

    Regex::new(&pattern).with_context(|| format!("invalid regex in '{field}': {input}"))?;

    Ok(())
}

#[derive(Default, Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Action {
    // Every key here is refused when written without a value, rather than read
    // as absent - see `reject_valueless_key`. This block is where that matters
    // most: a `cmd:` with nothing after it is not a fallback that does less, it
    // is a service that is never repaired, and the config looks like recovery
    // was configured.
    #[serde(default, deserialize_with = "require_cmd_value")]
    pub cmd: Option<String>,

    #[serde(default, deserialize_with = "require_http_value")]
    pub http: Option<String>,

    #[serde(default, deserialize_with = "require_stop_value")]
    pub stop: Option<usize>,

    #[serde(default, deserialize_with = "require_threshold_value")]
    pub threshold: Option<usize>,

    // Serializes this service's `cmd` against every other service declaring the
    // same group, and against nothing else. Absent means the command takes no
    // lock at all and runs as soon as the check fails.
    //
    // Only `cmd` is affected: `http` alerts never queue.
    #[serde(default, deserialize_with = "require_group_value")]
    pub group: Option<String>,

    // Recovery actions are not health probes: a restart legitimately takes far
    // longer than the `timeout` used to decide whether a service answers.
    // Applies to both `cmd` and `http`. Kept optional so `Action::default()`
    // cannot silently mean "no time at all".
    #[serde(default, deserialize_with = "parse_optional_duration")]
    pub timeout: Option<Duration>,
}

impl Action {
    /// Validates the recovery configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if no action is configured, `cmd` or `group` is blank,
    /// `group` is present without a command, or `http` is not a usable http(s)
    /// URL.
    pub fn validate(&self) -> Result<()> {
        if let Some(cmd) = &self.cmd
            && cmd.trim().is_empty()
        {
            return Err(anyhow!(
                "'if_not.cmd' cannot be empty; configure a command or remove the key."
            ));
        }

        // A blank group is almost certainly a half-finished edit, and it would
        // otherwise create a real lock under a name nothing else can readably
        // join. Serializing against `""` is never what was meant.
        if let Some(group) = &self.group
            && group.trim().is_empty()
        {
            return Err(anyhow!(
                "'if_not.group' cannot be empty; remove it to run the command concurrently."
            ));
        }

        // An unusable alert URL reports `outcome="failure"` on every attempt.
        // Caught here, it is one start-up error instead.
        if let Some(http) = &self.http {
            validate_http_url("if_not.http", http)?;
        }

        if self.cmd.is_none() && self.http.is_none() {
            return Err(anyhow!(
                "'if_not' must configure at least one action: 'cmd' or 'http'."
            ));
        }

        // `group` only serializes `cmd`. With no command to serialize it is a
        // lock nothing takes: the key reads as protection that is not there,
        // which is the surprise groups exist to remove.
        if self.group.is_some() && self.cmd.is_none() {
            return Err(anyhow!(
                "'if_not.group' only serializes 'if_not.cmd', and no 'cmd' is configured; add one, or remove 'group' - 'http' actions never queue."
            ));
        }

        Ok(())
    }

    /// The group this fallback command serializes against, if any.
    ///
    /// Trimmed, so `mysql` and `"mysql "` are the same group. The name is used
    /// verbatim as a lock key, and stray whitespace is invisible in an editor:
    /// left significant, it would pass validation and then silently serialize
    /// against nothing, which is the one outcome a declared group must never
    /// produce. Blank groups are rejected at startup, so the result is never
    /// empty.
    #[must_use]
    pub fn group_name(&self) -> Option<&str> {
        self.group.as_deref().map(str::trim)
    }

    /// Time budget for this fallback's actions, falling back to a generous
    /// default so a slow restart completes while a hung one still cannot
    /// stall the service forever.
    #[must_use]
    pub fn fallback_timeout(&self) -> Duration {
        self.timeout.unwrap_or(DEFAULT_FALLBACK_TIMEOUT)
    }
}

// Default time budget for `if_not` recovery commands.
const DEFAULT_FALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

// Default timeout value
const fn default_timeout() -> Duration {
    Duration::from_secs(5)
}

/// Rejects a field that must carry a value whenever its key is written.
///
/// YAML has three spellings of nothing - `key:`, `key: null` and `key: ~` -
/// and serde maps all of them onto the same `None` an absent key produces. For
/// `if_not.group` that collapse is dangerous: an operator who typed the key
/// meant to serialise this command, and silently getting no group is exactly
/// the failure this release exists to make loud. It is also inconsistent, since
/// `group: ""` is already refused as a half-finished edit, and a value-less key
/// is the same mistake with less to see.
///
/// The distinction works because `#[serde(default, deserialize_with = ...)]`
/// only calls this when the key is present, leaving `default` to supply `None`
/// when it is absent. So a `None` arriving here can only be an explicit null.
///
/// `key` is passed in because serde reports the path only as far as the struct.
/// Without it the error reads `if_not: has no value`, which invites the reader
/// to delete the whole recovery block rather than the one empty line.
fn require_value<'de, D, T>(deserializer: D, key: &str) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)?.map_or_else(
        || {
            Err(serde::de::Error::custom(format!(
                "'{key}' has no value. Give it one, or remove the key entirely"
            )))
        },
        |value| Ok(Some(value)),
    )
}

/// Defines the `deserialize_with` hook for one value-less-key guard.
///
/// serde requires a fixed function signature, so the key name cannot be passed
/// as an argument at the call site and each field needs its own function.
macro_rules! require_value_for {
    ($name:ident, $key:literal) => {
        fn $name<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
        where
            D: serde::Deserializer<'de>,
            T: Deserialize<'de>,
        {
            require_value(deserializer, $key)
        }
    };
}

require_value_for!(require_cmd_value, "cmd");
require_value_for!(require_http_value, "http");
require_value_for!(require_stop_value, "stop");
require_value_for!(require_threshold_value, "threshold");
require_value_for!(require_group_value, "group");

/// Parses a duration string (e.g., "5s", "3m", "1h", "2d") into a Duration.
fn parse_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    parse_duration_str(&s).map_err(serde::de::Error::custom)
}

/// Parses an optional duration, so an absent field stays `None` instead of
/// collapsing to a zero `Duration`.
///
/// A key written with no value is refused for the same reason as the rest of
/// the recovery block: silently falling back to the default is indistinguishable
/// from never having set it.
fn parse_optional_duration<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = require_value::<D, String>(deserializer, "timeout")?;

    value
        .map(|s| parse_duration_str(&s).map_err(serde::de::Error::custom))
        .transpose()
}

/// Converts a string like "5s", "3m", "1h", "2d" into `Duration`.
///
/// Values are whole numbers: a fraction such as `1.5h` is rejected rather than
/// rounded, since silently turning it into 1h or 2h would be a schedule the
/// operator never wrote. Sub-second units are deliberately absent too: the
/// shortest field this parses is a probe `timeout`, where a network round trip
/// makes millisecond precision meaningless, and the same parser serves `every`,
/// where it would invite a service to be scanned into the ground.
fn parse_duration_str(input: &str) -> Result<Duration> {
    let boundary = input
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(input.len());
    let (value, unit) = input.split_at(boundary);

    let value: u64 = value
        .parse()
        .map_err(|_| anyhow!("Invalid number in duration: {input}"))?;

    // The unit is matched whole and reported on its own. Stripping a one-byte
    // suffix instead read "200ms" as the number "200m" and blamed the digits,
    // which points away from the actual mistake.
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 60 * 60 * 24,
        "" => {
            return Err(anyhow!(
                "Duration is missing a unit, expected 's', 'm', 'h' or 'd': {input}"
            ));
        }
        // A decimal separator is a fractional value, not an unknown unit.
        // Reported apart from both so "1.5h" is not told that units below a
        // second are unsupported, which is true but has nothing to do with it.
        _ if unit.starts_with(['.', ',']) => {
            return Err(anyhow!(
                "Duration must be a whole number: {input}. Use a smaller unit instead of a fraction, for example '90s' rather than '1.5m'"
            ));
        }
        _ => {
            return Err(anyhow!(
                "Invalid duration unit '{unit}' in: {input}. Expected 's', 'm', 'h' or 'd'; units below a second are not supported"
            ));
        }
    };

    // Zero is rejected rather than accepted as an instant: `every: 0s` reaches
    // `tokio::time::interval`, which panics on a zero period and takes the
    // whole supervisor down at start-up. A zero `timeout` is no more useful -
    // it expires before the request it is meant to bound. Neither is worth
    // supporting, so both are refused here where the message can name the
    // minimum.
    if value == 0 {
        return Err(anyhow!(
            "Duration must be greater than zero: {input}. The shortest duration epazote accepts is '1s'"
        ));
    }

    let seconds = value
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("Duration is too large: {input}"))?;

    Ok(Duration::from_secs(seconds))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    /// A recovery command must not inherit the health-probe timeout: a
    /// restart routinely outlives the few seconds allowed for a probe.
    #[test]
    fn test_fallback_timeout_defaults_to_generous_budget() {
        let action = Action::default();

        assert_eq!(action.fallback_timeout(), Duration::from_secs(300));
        assert!(
            action.fallback_timeout() > default_timeout(),
            "fallback budget must exceed the health-probe timeout"
        );
    }

    /// `Action::default()` must not mean "no time at all".
    #[test]
    fn test_fallback_timeout_default_is_not_zero() {
        assert!(!Action::default().fallback_timeout().is_zero());
    }

    #[test]
    fn test_fallback_timeout_is_configurable() {
        let config: ServiceDetails = serde_yaml_ng::from_str(
            r#"
every: 1m
test: "true"
expect:
  status: 0
  if_not:
    cmd: "systemctl restart app"
    timeout: 15m
"#,
        )
        .expect("config should parse");

        let if_not = config.expect.if_not.expect("if_not should be set");

        assert_eq!(if_not.timeout, Some(Duration::from_mins(15)));
        assert_eq!(if_not.fallback_timeout(), Duration::from_mins(15));
    }

    #[test]
    fn test_fallback_timeout_rejects_unitless_value() {
        let result: Result<ServiceDetails, _> = serde_yaml_ng::from_str(
            r#"
every: 1m
test: "true"
expect:
  status: 0
  if_not:
    cmd: "true"
    timeout: 30
"#,
        );

        assert!(result.is_err(), "a unitless duration must be rejected");
    }

    use serde_json::json;
    use std::io::Write;

    // Helper to create config from YAML
    fn create_config(yaml: &str) -> tempfile::NamedTempFile {
        let mut tmp_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
        tmp_file
            .write_all(yaml.as_bytes())
            .expect("Failed to write to temp file");
        tmp_file.flush().expect("Failed to flush temp file");
        tmp_file
    }

    /// Builds a service whose `if_not` runs `cmd`, optionally in `group`.
    fn service_with_fallback(cmd: &str, group: Option<&str>) -> ServiceDetails {
        ServiceDetails {
            every: Duration::from_secs(30),
            expect: Expect {
                status: Some(200),
                header: None,
                body: None,
                body_not: None,
                json: None,
                if_not: Some(Action {
                    cmd: Some(cmd.to_string()),
                    http: None,
                    stop: None,
                    threshold: None,
                    group: group.map(ToString::to_string),
                    timeout: None,
                }),
            },
            follow_redirects: None,
            headers: None,
            max_bytes: None,
            test: None,
            timeout: default_timeout(),
            url: Some("https://epazote.io".to_string()),
            method: HttpMethod::Get,
            body: None,
        }
    }

    fn services_from(entries: Vec<(&str, ServiceDetails)>) -> HashMap<String, ServiceDetails> {
        entries
            .into_iter()
            .map(|(name, service)| (name.to_string(), service))
            .collect()
    }

    #[test]
    fn test_group_is_parsed() {
        let yaml = r"
services:
  db:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
      if_not:
        group: mysql
        cmd: systemctl restart mariadb
";

        let tmp_file = create_config(yaml);
        let config = Config::new(tmp_file.path().to_path_buf()).expect("Failed to load config");

        let if_not = config
            .services
            .get("db")
            .expect("Service not found")
            .expect
            .if_not
            .as_ref()
            .expect("if_not not found");

        assert_eq!(if_not.group_name(), Some("mysql"));
    }

    /// No group is the default, and it means the command takes no lock at all.
    #[test]
    fn test_group_defaults_to_none() {
        let action = Action {
            cmd: Some("true".to_string()),
            ..Action::default()
        };

        assert_eq!(action.group_name(), None);
        assert!(action.validate().is_ok());
    }

    /// Regression: surrounding whitespace must not split a group in two.
    ///
    /// The name is used verbatim as a lock key, so an untrimmed `"mysql "`
    /// would pass validation - it is not blank - and then serialize against
    /// nothing, silently leaving the service unprotected while the config
    /// plainly says otherwise. Whitespace is invisible in an editor, so nothing
    /// would point at the cause.
    #[test]
    fn test_group_name_is_trimmed_so_stray_whitespace_cannot_split_a_group() {
        let padded = Action {
            cmd: Some("true".to_string()),
            group: Some("  mysql  ".to_string()),
            ..Action::default()
        };
        let plain = Action {
            cmd: Some("true".to_string()),
            group: Some("mysql".to_string()),
            ..Action::default()
        };

        assert!(padded.validate().is_ok());
        assert_eq!(padded.group_name(), Some("mysql"));
        assert_eq!(
            padded.group_name(),
            plain.group_name(),
            "a padded group must lock against the same name as an unpadded one"
        );
    }

    /// A blank group is almost certainly a half-finished edit. Serializing
    /// against `""` is never what was meant, so it must not start.
    #[test]
    fn test_blank_group_is_rejected_at_startup() {
        for blank in ["", "   ", "\t"] {
            let action = Action {
                group: Some(blank.to_string()),
                ..Default::default()
            };

            let error = action
                .validate()
                .expect_err("a blank group must be rejected");

            assert!(
                error.to_string().contains("cannot be empty"),
                "the error must explain the blank group, got: {error}"
            );
        }
    }

    /// The blank group must be caught through the same path `Config::new`
    /// uses, not merely by calling `Action::validate` directly - `Expect`
    /// previously never validated `if_not` at all.
    #[test]
    fn test_blank_group_fails_config_load() {
        let yaml = r#"
services:
  db:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
      if_not:
        group: ""
        cmd: systemctl restart mariadb
"#;

        let tmp_file = create_config(yaml);
        let error = Config::new(tmp_file.path().to_path_buf())
            .expect_err("a blank group must halt startup");

        assert!(
            format!("{error:#}").contains("cannot be empty"),
            "the error must explain the blank group, got: {error:#}"
        );
    }

    /// A group protects only commands. Accepting it beside an HTTP-only action
    /// would make the config look serialized while the request runs
    /// immediately and takes no lock.
    #[test]
    fn test_group_without_a_command_fails_config_load() {
        let yaml = r"
services:
  alerts:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
      if_not:
        group: database
        http: https://alerts.example.com/hook
";

        let tmp_file = create_config(yaml);
        let error = Config::new(tmp_file.path().to_path_buf())
            .expect_err("a group with no command must halt startup");
        let message = format!("{error:#}");

        assert!(
            message.contains("'if_not.group'") && message.contains("'if_not.cmd'"),
            "the error must name the inert group and the missing command, got: {message}"
        );
        assert!(
            message.contains("'http' actions never queue"),
            "the error must explain why HTTP cannot use the group, got: {message}"
        );
    }

    /// The new validation must reject only an inert group: a command and an
    /// HTTP action remain valid together, with only the command taking the
    /// group lock.
    #[test]
    fn test_group_with_a_command_and_http_still_loads() {
        let yaml = r"
services:
  database:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
      if_not:
        group: database
        cmd: systemctl restart mariadb
        http: https://alerts.example.com/hook
";

        let tmp_file = create_config(yaml);
        let config = Config::new(tmp_file.path().to_path_buf())
            .expect("a group is valid when there is a command to serialize");
        let action = config
            .get_service("database")
            .and_then(|service| service.expect.if_not.as_ref())
            .expect("the fallback action must parse");

        assert_eq!(action.group_name(), Some("database"));
        assert!(action.cmd.is_some());
        assert!(action.http.is_some());
    }

    /// The rule covers the whole recovery block, not just `group`. A `cmd:`
    /// with nothing after it is the worst of these: the config reads as though
    /// recovery were configured while the service is never actually repaired.
    #[test]
    fn test_valueless_recovery_keys_fail_config_load() {
        for key in ["cmd", "http", "stop", "threshold", "timeout", "group"] {
            let yaml = format!(
                r"
services:
  db:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
      if_not:
        {key}:
"
            );

            let tmp_file = create_config(&yaml);
            let error = Config::new(tmp_file.path().to_path_buf())
                .expect_err("a valueless recovery key must halt startup");

            assert!(
                format!("{error:#}").contains("has no value"),
                "'{key}:' must be refused for carrying no value, got: {error:#}"
            );
        }
    }

    /// A quoted empty command deserializes as `Some`, unlike a valueless key.
    /// It must still be rejected because `sh -c` treats it as a successful
    /// no-op, which otherwise makes a broken check or fallback look healthy.
    #[test]
    fn test_blank_check_and_fallback_commands_fail_config_load() {
        for blank in [r#""""#, r#""   ""#, r#""\t""#] {
            let check_yaml = format!(
                r"
services:
  db:
    test: {blank}
    every: 30s
    expect:
      status: 0
"
            );
            let tmp_file = create_config(&check_yaml);
            let error = Config::new(tmp_file.path().to_path_buf())
                .expect_err("a blank check command must halt startup");
            assert!(
                format!("{error:#}").contains("'test' cannot be empty"),
                "the error must name the blank check command, got: {error:#}"
            );

            let fallback_yaml = format!(
                r"
services:
  db:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
      if_not:
        cmd: {blank}
"
            );
            let tmp_file = create_config(&fallback_yaml);
            let error = Config::new(tmp_file.path().to_path_buf())
                .expect_err("a blank fallback command must halt startup");
            assert!(
                format!("{error:#}").contains("'if_not.cmd' cannot be empty"),
                "the error must name the blank fallback command, got: {error:#}"
            );
        }
    }

    /// YAML spells nothing three ways, and serde maps all of them onto the
    /// `None` an absent key produces. Writing the key is an intent to
    /// serialise, so collapsing it into "no group" would be the silent flip
    /// this release exists to prevent - and it would be inconsistent with
    /// `group: ""`, which is already refused as a half-finished edit.
    #[test]
    fn test_valueless_group_fails_config_load() {
        for spelling in ["group:", "group: null", "group: ~"] {
            let yaml = format!(
                r"
services:
  db:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
      if_not:
        {spelling}
        cmd: systemctl restart mariadb
"
            );

            let tmp_file = create_config(&yaml);
            let error = Config::new(tmp_file.path().to_path_buf())
                .expect_err("a group written with no value must halt startup");

            assert!(
                format!("{error:#}").contains("has no value"),
                "'{spelling}' must be refused for carrying no value, got: {error:#}"
            );
        }
    }

    /// A URL that cannot be requested used to deserialize cleanly and then fail
    /// while the request was built - inside the scan, before the failure was
    /// counted and before any `if_not` ran.
    ///
    /// Observed against the release binary with `url: "not a url"` and a
    /// fallback configured: `epazote_status 0` and `epazote_failures_total 7`,
    /// but `epazote_consecutive_failures 0` and every
    /// `epazote_fallback_executions_total` outcome `0`. Down forever, no
    /// streak, no recovery attempted, and nothing naming the cause.
    #[test]
    fn test_unusable_service_url_halts_startup() {
        for url in [
            // Not a URL at all.
            "not a url",
            // A missing scheme is read by the parser as one: this is the
            // scheme `localhost`, not a host with a port.
            "localhost:8080",
            // Parses, but reqwest builds the request and then refuses to send
            // it - the same permanent failure, one step later.
            "ftp://files.example.com",
            "file:///etc/passwd",
            // Right scheme, nothing to connect to.
            "http://",
        ] {
            let yaml = format!(
                r#"
services:
  db:
    url: "{url}"
    every: 30s
    expect:
      status: 200
      if_not:
        cmd: systemctl restart mariadb
"#
            );

            let tmp_file = create_config(&yaml);
            let error = Config::new(tmp_file.path().to_path_buf())
                .expect_err("a URL that can never be requested must halt startup");

            assert!(
                format!("{error:#}").contains("'url'"),
                "'{url}' must be refused by name, got: {error:#}"
            );
        }
    }

    /// The same hazard on the recovery side. An unusable alert URL is not
    /// silent - it records `outcome="failure"` on every attempt - but it still
    /// repairs nothing, and one start-up error is better than a permanent
    /// failing series.
    #[test]
    fn test_unusable_fallback_http_url_halts_startup() {
        for url in ["not a url", "ftp://alerts.example.com"] {
            let yaml = format!(
                r#"
services:
  db:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
      if_not:
        http: "{url}"
"#
            );

            let tmp_file = create_config(&yaml);
            let error = Config::new(tmp_file.path().to_path_buf())
                .expect_err("an alert URL that can never be requested must halt startup");

            assert!(
                format!("{error:#}").contains("'if_not.http'"),
                "'{url}' must be refused by name, got: {error:#}"
            );
        }
    }

    /// The rejection is worth nothing if it also turns away addresses that
    /// work. Every form here is one a running deployment can be using today.
    #[test]
    fn test_usable_service_urls_still_parse() {
        for url in [
            "http://epazote.io",
            "https://epazote.io",
            "https://epazote.io:8443/health?verbose=1",
            "http://127.0.0.1:8080/",
            "http://[::1]:8080/health",
            "https://user:pass@epazote.io/health",
        ] {
            let yaml = format!(
                r#"
services:
  db:
    url: "{url}"
    every: 30s
    expect:
      status: 200
      if_not:
        http: "{url}"
"#
            );

            let tmp_file = create_config(&yaml);
            assert!(
                Config::new(tmp_file.path().to_path_buf()).is_ok(),
                "'{url}' is a usable address and must keep loading"
            );
        }
    }

    /// What `epazote_fallback_configured` is derived from.
    #[test]
    fn test_has_fallback_action_tracks_command_or_http() {
        for (if_not, expected) in [
            ("        cmd: systemctl restart mariadb", true),
            ("        http: https://alerts.example.com/hook", true),
            ("", false),
        ] {
            let yaml = format!(
                r"
services:
  db:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
{}
",
                if if_not.is_empty() {
                    String::new()
                } else {
                    format!("      if_not:\n{if_not}")
                }
            );

            let tmp_file = create_config(&yaml);
            let config =
                Config::new(tmp_file.path().to_path_buf()).expect("config must load: {yaml}");
            let service = config.services.get("db").expect("service 'db' must exist");

            assert_eq!(
                service.expect.has_fallback_action(),
                expected,
                "wrong fallback verdict for:\n{yaml}"
            );
        }
    }

    /// A budget or serialization setting without an action used to run a
    /// successful no-op on every failed check. Refuse the block at startup.
    #[test]
    fn test_if_not_without_an_action_fails_config_load() {
        for if_not in [
            "      if_not: {}",
            "      if_not:\n        stop: 3",
            "      if_not:\n        threshold: 2",
            "      if_not:\n        timeout: 30s",
            "      if_not:\n        group: database",
        ] {
            let yaml = format!(
                r"
services:
  db:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
{if_not}
"
            );

            let tmp_file = create_config(&yaml);
            let error = Config::new(tmp_file.path().to_path_buf())
                .expect_err("an actionless if_not block must halt startup");
            assert!(
                format!("{error:#}").contains("'cmd' or 'http'"),
                "the error must say which actions are required, got: {error:#}"
            );
        }
    }

    /// The counterpart the rejection must not break: omitting the key is how
    /// an ungrouped command is declared, and it has to keep parsing as `None`.
    #[test]
    fn test_absent_group_still_parses_as_none() {
        let yaml = r"
services:
  db:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
      if_not:
        cmd: systemctl restart mariadb
";

        let tmp_file = create_config(yaml);
        let config = Config::new(tmp_file.path().to_path_buf())
            .expect("an absent group is how an ungrouped command is written");

        let group = config
            .get_service("db")
            .and_then(|service| service.expect.if_not.as_ref())
            .expect("the fallback should have parsed")
            .group_name();

        assert!(group.is_none(), "an absent group must stay absent");
    }

    /// and running it concurrently interleaves its log.
    #[test]
    fn test_identical_ungrouped_commands_conflict() {
        let services = services_from(vec![
            ("a", service_with_fallback("/opt/restart.sh", None)),
            ("b", service_with_fallback("/opt/restart.sh", None)),
        ]);

        let conflicts = conflicting_fallback_commands(&services);

        assert_eq!(conflicts.len(), 1, "expected one conflict: {conflicts:?}");
        assert_eq!(conflicts[0].reason, ConflictReason::IdenticalCommand);
        assert_eq!(
            conflicts[0].services,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    /// Rule B: the same script with different arguments still shares the
    /// script - and typically its log file - but the strings differ, so Rule A
    /// cannot see it.
    #[test]
    fn test_same_script_with_different_arguments_conflicts() {
        let services = services_from(vec![
            ("a", service_with_fallback("/opt/restart.sh alpha", None)),
            ("b", service_with_fallback("/opt/restart.sh beta", None)),
        ]);

        let conflicts = conflicting_fallback_commands(&services);

        assert_eq!(conflicts.len(), 1, "expected one conflict: {conflicts:?}");
        assert_eq!(conflicts[0].reason, ConflictReason::SharedScript);
        assert_eq!(
            conflicts[0].services,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    /// The heuristic has to stay quiet on unrelated services or it becomes
    /// noise operators tune out - and then the real warnings go unread too.
    /// A bare utility name is not evidence of anything shared.
    #[test]
    fn test_same_utility_with_different_targets_does_not_conflict() {
        let services = services_from(vec![
            (
                "db",
                service_with_fallback("systemctl restart mariadb", None),
            ),
            (
                "cache",
                service_with_fallback("systemctl restart varnish", None),
            ),
        ]);

        assert!(
            conflicting_fallback_commands(&services).is_empty(),
            "unrelated units must not be reported as conflicting"
        );
    }

    /// A declared group already serializes these, so there is nothing to warn
    /// about.
    #[test]
    fn test_grouped_services_never_conflict() {
        let services = services_from(vec![
            ("a", service_with_fallback("/opt/restart.sh", Some("mysql"))),
            ("b", service_with_fallback("/opt/restart.sh", Some("mysql"))),
        ]);

        assert!(
            conflicting_fallback_commands(&services).is_empty(),
            "services declaring a group are already serialized"
        );
    }

    /// A group on only one side protects nobody: the grouped command takes its
    /// group's lock, the ungrouped one takes none, and the two still overlap.
    /// Reporting it is the whole point - this is the shape an operator reaches
    /// by grouping the services they remembered and missing one.
    #[test]
    fn test_group_on_only_one_side_still_conflicts() {
        let services = services_from(vec![
            ("a", service_with_fallback("/opt/restart.sh", Some("mysql"))),
            ("b", service_with_fallback("/opt/restart.sh", None)),
        ]);

        let conflicts = conflicting_fallback_commands(&services);

        assert_eq!(
            conflicts.len(),
            1,
            "a group only serializes the services actually in it"
        );
        assert_eq!(conflicts[0].services, vec!["a", "b"]);
    }

    /// Different groups serialize within themselves and not against each
    /// other, so two services sharing a script across them still interleave.
    #[test]
    fn test_different_groups_sharing_a_script_conflict() {
        let services = services_from(vec![
            ("a", service_with_fallback("/opt/restart.sh one", Some("x"))),
            ("b", service_with_fallback("/opt/restart.sh two", Some("y"))),
        ]);

        let conflicts = conflicting_fallback_commands(&services);

        assert_eq!(conflicts.len(), 1, "separate groups do not exclude");
        assert_eq!(conflicts[0].reason, ConflictReason::SharedScript);
    }

    /// Spelling a system utility with its absolute path does not make it a
    /// shared script. `/usr/bin/systemctl restart a` and
    /// `/usr/bin/systemctl restart b` are as unrelated as the bare form, and
    /// reporting them would restore exactly the noise the heuristic exists to
    /// avoid - operators who follow that advice and group them recreate the
    /// starvation this release removes.
    #[test]
    fn test_absolute_system_utility_is_not_a_shared_script() {
        for utility in [
            "/usr/bin/systemctl",
            "/bin/systemctl",
            "/sbin/service",
            "/usr/sbin/service",
        ] {
            let services = services_from(vec![
                (
                    "a",
                    service_with_fallback(&format!("{utility} restart alpha"), None),
                ),
                (
                    "b",
                    service_with_fallback(&format!("{utility} restart beta"), None),
                ),
            ]);

            assert!(
                conflicting_fallback_commands(&services).is_empty(),
                "{utility} is a system utility, not a shared script"
            );
        }
    }

    /// A script outside the distribution's binary directories is an operator's
    /// own, even without an extension.
    #[test]
    fn test_operator_path_outside_system_dirs_is_a_script() {
        let services = services_from(vec![
            ("a", service_with_fallback("/opt/bin/recover alpha", None)),
            ("b", service_with_fallback("/opt/bin/recover beta", None)),
        ]);

        let conflicts = conflicting_fallback_commands(&services);

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].reason, ConflictReason::SharedScript);
    }

    /// Recovery commands routinely need privileges, so a shared script behind
    /// `sudo` is one of the likeliest forms of this conflict - and reading
    /// only the first token sees `sudo` and reports nothing.
    #[test]
    fn test_wrapper_prefixed_shared_script_is_found() {
        for prefix in ["sudo", "/usr/bin/sudo", "bash", "sh", "env", "nice"] {
            let services = services_from(vec![
                (
                    "a",
                    service_with_fallback(&format!("{prefix} /opt/restart.sh alpha"), None),
                ),
                (
                    "b",
                    service_with_fallback(&format!("{prefix} /opt/restart.sh beta"), None),
                ),
            ]);

            let conflicts = conflicting_fallback_commands(&services);

            assert_eq!(
                conflicts.len(),
                1,
                "{prefix} should be stepped over to reach the script"
            );
            assert_eq!(conflicts[0].reason, ConflictReason::SharedScript);
        }
    }

    /// A leading `KEY=value` prefixes the command rather than being it, so it
    /// has to be stepped over like any other wrapper - and differing values
    /// must not hide that the script behind them is the same.
    #[test]
    fn test_environment_assignment_prefix_is_stepped_over() {
        let services = services_from(vec![
            (
                "a",
                service_with_fallback("PGUSER=alpha /opt/restart.sh alpha", None),
            ),
            (
                "b",
                service_with_fallback("PGUSER=beta /opt/restart.sh beta", None),
            ),
        ]);

        let conflicts = conflicting_fallback_commands(&services);

        assert_eq!(
            conflicts.len(),
            1,
            "an environment assignment is not the command"
        );
        assert_eq!(conflicts[0].reason, ConflictReason::SharedScript);
    }

    /// Stepping over a wrapper must not stop at the wrapper itself: two
    /// different scripts run through `/usr/bin/env` share only the wrapper.
    #[test]
    fn test_shared_wrapper_with_different_scripts_is_not_a_conflict() {
        let services = services_from(vec![
            (
                "a",
                service_with_fallback("/usr/bin/env python3 /opt/alpha.py", None),
            ),
            (
                "b",
                service_with_fallback("/usr/bin/env bash /opt/beta.sh", None),
            ),
        ]);

        assert!(
            conflicting_fallback_commands(&services).is_empty(),
            "a shared interpreter is not a shared script"
        );
    }

    /// A flag's value may be a separate token, so there is no way to know what
    /// to skip next. Reporting nothing is correct: a missed warning costs a
    /// scrambled log, a wrong one costs the operator's trust in all of them.
    #[test]
    fn test_wrapper_flags_suppress_the_guess() {
        let services = services_from(vec![
            (
                "a",
                service_with_fallback("sudo -u postgres /opt/restart.sh alpha", None),
            ),
            (
                "b",
                service_with_fallback("sudo -u mysql /opt/restart.sh beta", None),
            ),
        ]);

        assert!(
            conflicting_fallback_commands(&services).is_empty(),
            "an unparseable wrapper invocation must not be guessed at"
        );
    }

    /// An identical command is still caught whatever it looks like, since Rule
    /// A compares the whole string and never consults the script heuristic.
    #[test]
    fn test_identical_command_is_caught_regardless_of_shape() {
        let services = services_from(vec![
            (
                "a",
                service_with_fallback("sudo -u postgres /opt/restart.sh", None),
            ),
            (
                "b",
                service_with_fallback("sudo -u postgres /opt/restart.sh", None),
            ),
        ]);

        let conflicts = conflicting_fallback_commands(&services);

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].reason, ConflictReason::IdenticalCommand);
    }

    #[test]
    fn test_single_service_never_conflicts_with_itself() {
        let services = services_from(vec![("a", service_with_fallback("/opt/restart.sh", None))]);

        assert!(conflicting_fallback_commands(&services).is_empty());
    }

    /// An identical command is already reported by Rule A; Rule B must not
    /// report the very same set again.
    #[test]
    fn test_identical_commands_are_reported_once() {
        let services = services_from(vec![
            ("a", service_with_fallback("/opt/restart.sh", None)),
            ("b", service_with_fallback("/opt/restart.sh", None)),
        ]);

        let conflicts = conflicting_fallback_commands(&services);

        assert_eq!(
            conflicts.len(),
            1,
            "a shared script that is also an identical command must be reported once: {conflicts:?}"
        );
    }

    /// `Config::services` is a `HashMap`, so anything derived from iteration
    /// order is arbitrary. The reported names must be sorted or the warning
    /// text changes between runs.
    #[test]
    fn test_conflicting_services_are_reported_in_sorted_order() {
        let services = services_from(vec![
            ("zulu", service_with_fallback("/opt/restart.sh", None)),
            ("alpha", service_with_fallback("/opt/restart.sh", None)),
            ("mike", service_with_fallback("/opt/restart.sh", None)),
        ]);

        let conflicts = conflicting_fallback_commands(&services);

        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            conflicts[0].services,
            vec!["alpha".to_string(), "mike".to_string(), "zulu".to_string()]
        );
    }

    /// A command with no `if_not.cmd` has nothing to serialize.
    #[test]
    fn test_services_without_a_command_never_conflict() {
        let mut alert_only = service_with_fallback("/opt/restart.sh", None);
        if let Some(if_not) = alert_only.expect.if_not.as_mut() {
            if_not.cmd = None;
            if_not.http = Some("http://127.0.0.1/alert".to_string());
        }

        let services = services_from(vec![
            ("a", alert_only),
            ("b", service_with_fallback("/opt/restart.sh", None)),
        ]);

        assert!(
            conflicting_fallback_commands(&services).is_empty(),
            "an http-only fallback never queues, so it cannot conflict"
        );
    }

    /// Records the target and message of every event, so a test can assert on
    /// what an operator would actually read in the journal.
    #[derive(Default, Clone)]
    struct CapturedWarnings(std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>);

    impl tracing::field::Visit for CapturedWarnings {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0
                    .lock()
                    .expect("failed to record message")
                    .push((String::new(), format!("{value:?}")));
            }
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturedWarnings {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut visitor = self.clone();
            event.record(&mut visitor);

            if let Some(last) = self.0.lock().expect("failed to record target").last_mut() {
                last.0 = event.metadata().target().to_string();
            }
        }
    }

    fn warnings_from_loading(yaml: &str) -> Vec<(String, String)> {
        use tracing_subscriber::layer::SubscriberExt;

        let tmp_file = create_config(yaml);
        let captured = CapturedWarnings::default();

        tracing::subscriber::with_default(
            <tracing_subscriber::registry::Registry as Default>::default().with(captured.clone()),
            || {
                Config::new(tmp_file.path().to_path_buf()).expect("Failed to load config");
            },
        );

        captured
            .0
            .lock()
            .expect("failed to read captured events")
            .clone()
    }
    /// The tests above check the heuristic in isolation. This one checks that
    /// loading a real file actually says something: `Config::new` has to reach
    /// the warning, and it has to be emitted under the `epazote::cli::config`
    /// target that [`crate::cli::telemetry`] whitelists at the default level.
    /// A warning that is computed correctly and then never logged, or logged
    /// under a target the filter drops, protects nobody.
    #[test]
    fn test_loading_a_conflicting_config_warns_the_operator() {
        let warnings = warnings_from_loading(
            "
services:
  db-primary:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        cmd: /opt/restart.sh primary
  db-replica:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        cmd: /opt/restart.sh replica
",
        );

        assert_eq!(warnings.len(), 1, "expected one warning, got {warnings:?}");

        let (target, message) = &warnings[0];

        assert_eq!(
            target, "epazote::cli::config",
            "the warning must carry the target the default log filter lets through"
        );
        assert!(
            message.contains("db-primary, db-replica"),
            "the warning must name the services in sorted order: {message}"
        );
        assert!(
            message.contains("invoke the same script"),
            "the warning must say why they were flagged: {message}"
        );
        assert!(
            message.contains("if_not.group"),
            "the warning must say how to fix it: {message}"
        );
    }

    /// The other branch of the heuristic, through the same public path: an
    /// identical command is reported as such rather than as a shared script.
    #[test]
    fn test_loading_a_config_with_identical_commands_warns_the_operator() {
        let warnings = warnings_from_loading(
            "
services:
  worker-a:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        cmd: systemctl restart queue
  worker-b:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        cmd: systemctl restart queue
",
        );

        assert_eq!(warnings.len(), 1, "expected one warning, got {warnings:?}");
        assert!(
            warnings[0].1.contains("run an identical command"),
            "an identical command must be reported as such: {}",
            warnings[0].1
        );
    }

    /// The safety net must stay quiet for a config that declares its groups,
    /// or operators learn to ignore it and it stops being a safety net.
    #[test]
    fn test_loading_a_grouped_config_warns_about_nothing() {
        let warnings = warnings_from_loading(
            "
services:
  db-primary:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        group: mysql
        cmd: /opt/restart.sh primary
  db-replica:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        group: mysql
        cmd: /opt/restart.sh replica
  edge-cache:
    test: true
    every: 30s
    expect:
      status: 0
      if_not:
        cmd: systemctl restart varnish
",
        );

        assert!(
            warnings.is_empty(),
            "a config that declares its groups must load silently, got {warnings:?}"
        );
    }

    /// A `.sh` script invoked without a path is still an operator script, not
    /// a system utility.
    #[test]
    fn test_bare_shell_script_name_is_treated_as_a_script() {
        assert_eq!(script_key("restart.sh alpha"), Some("restart.sh"));
        assert_eq!(script_key("/opt/restart alpha"), Some("/opt/restart"));
        assert_eq!(script_key("systemctl restart mariadb"), None);
        assert_eq!(script_key(""), None);
    }

    #[test]
    fn test_parse_duration() {
        assert_eq!(
            parse_duration_str("5s").expect("Failed to parse duration"),
            Duration::from_secs(5)
        );
        assert_eq!(
            parse_duration_str("3m").expect("Failed to parse duration"),
            Duration::from_mins(3)
        );
        assert_eq!(
            parse_duration_str("1h").expect("Failed to parse duration"),
            Duration::from_hours(1)
        );
        assert_eq!(
            parse_duration_str("2d").expect("Failed to parse duration"),
            Duration::from_hours(48)
        );
    }

    #[test]
    fn test_parse_duration_rejects_empty_value() {
        assert!(parse_duration_str("").is_err());
        assert!(parse_duration_str("s").is_err());
    }

    /// A sub-second duration must be refused for the unit, not the number.
    ///
    /// `200ms` used to have its trailing `s` stripped and was then reported as
    /// `Invalid number in duration: 200ms` - which sends the reader to look at
    /// the digits, the one part of the value that was correct. The message has
    /// to name the unit and say what is accepted instead, because that is the
    /// only place a config file can learn the vocabulary.
    #[test]
    fn test_sub_second_duration_is_rejected_for_its_unit() {
        for input in ["200ms", "5us", "10ns"] {
            let error = parse_duration_str(input)
                .expect_err("a sub-second duration must be rejected")
                .to_string();

            assert!(
                error.contains("Invalid duration unit"),
                "the unit must be blamed, not the number: {error}"
            );
            assert!(
                error.contains("units below a second are not supported"),
                "the message must say why: {error}"
            );
            assert!(
                error.contains("'s', 'm', 'h' or 'd'"),
                "the message must list what is accepted: {error}"
            );
        }
    }

    /// A fractional duration is rejected for being fractional.
    ///
    /// It must not be rounded: silently reading `1.5h` as one hour or two is a
    /// schedule the operator never wrote. It must not be blamed on the unit
    /// either - `.5h` is not a unit, and telling someone who asked for an hour
    /// and a half that units below a second are unsupported is true but
    /// unrelated, which is worse than saying nothing.
    #[test]
    fn test_fractional_duration_is_rejected_as_a_whole_number_problem() {
        for input in ["0.5s", "1.5h", "2.0s", "1,5m", "1.5"] {
            let error = parse_duration_str(input)
                .expect_err("a fractional duration must be rejected")
                .to_string();

            assert!(
                error.contains("must be a whole number"),
                "the fraction must be blamed, not the unit: {error}"
            );
            assert!(
                !error.contains("below a second"),
                "an unrelated fact about sub-second units must not be offered: {error}"
            );
            assert!(
                error.contains("90s"),
                "the message must show the way out of a fraction: {error}"
            );
        }
    }

    /// A fraction with no leading digit has no number to read at all, so it is
    /// the number that is at fault rather than its precision.
    #[test]
    fn test_fraction_without_a_leading_digit_is_a_number_problem() {
        let error = parse_duration_str(".5s")
            .expect_err("a duration with no leading digit must be rejected")
            .to_string();

        assert!(
            error.contains("Invalid number"),
            "unexpected error: {error}"
        );
    }

    /// A bare number is a missing unit, which is a different mistake from an
    /// unrecognized one and reads better said plainly.
    #[test]
    fn test_duration_without_a_unit_says_the_unit_is_missing() {
        let error = parse_duration_str("30")
            .expect_err("a duration with no unit must be rejected")
            .to_string();

        assert!(
            error.contains("missing a unit"),
            "a bare number must be reported as a missing unit: {error}"
        );
    }

    /// Whatever the message, an unusable duration must still be refused - and
    /// refused while loading, so it can never reach a scan.
    #[test]
    fn test_config_with_a_sub_second_duration_is_rejected_at_startup() {
        let tmp_file = create_config(
            "
services:
  probe:
    url: https://example.com
    every: 500ms
    expect:
      status: 200
",
        );

        let error = Config::new(tmp_file.path().to_path_buf())
            .expect_err("a sub-second 'every' must not load")
            .to_string();

        assert!(
            error.contains("Failed to parse config file"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_parse_duration_rejects_overflow() {
        let input = format!("{}d", u64::MAX);
        assert!(parse_duration_str(&input).is_err());
    }

    /// `every: 0s` used to parse, then reach `tokio::time::interval`, which
    /// panics on a zero period - so a typo took the whole supervisor down at
    /// start-up rather than being reported as the configuration error it is.
    #[test]
    fn test_parse_duration_rejects_zero() {
        for input in ["0s", "0m", "0h", "0d"] {
            let error = parse_duration_str(input)
                .expect_err("zero is not a usable duration")
                .to_string();

            assert!(
                error.contains("greater than zero"),
                "{input} should be refused for being zero, got: {error}"
            );
            assert!(
                error.contains("1s"),
                "{input} should name the minimum, got: {error}"
            );
        }
    }

    /// The zero check must not swallow a value that merely starts with a zero.
    #[test]
    fn test_parse_duration_accepts_leading_zero_value() {
        assert_eq!(
            parse_duration_str("05s").expect("05s is five seconds"),
            Duration::from_secs(5)
        );
    }

    /// A zero duration has to be refused where it is read, not just where it
    /// is used, so the whole config is rejected before any service starts.
    #[test]
    fn test_config_rejects_zero_interval() {
        let yaml = r"
services:
  crash-me:
    every: 0s
    url: http://127.0.0.1:9/nope
    expect:
      status: 200
";

        let tmp_file = create_config(yaml);
        let error = Config::new(tmp_file.path().to_path_buf())
            .expect_err("a zero interval must not build a config");

        assert!(
            format!("{error:#}").contains("greater than zero"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn test_config() {
        let yaml = r"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    headers:
      X-Custom-Header: TestValue
    expect:
      status: 200
      ";

        let tmp_file = create_config(yaml);
        let config_file = tmp_file.path().to_path_buf();
        let config = Config::new(config_file).expect("Failed to load config");

        assert_eq!(config.services.len(), 1);
        assert_eq!(
            config.services.get("test").expect("Service not found").url,
            Some("https://epazote.io".to_string())
        );
        assert_eq!(
            config
                .services
                .get("test")
                .expect("Service not found")
                .every,
            Duration::from_secs(30)
        );
        assert_eq!(
            config
                .services
                .get("test")
                .expect("Service not found")
                .headers
                .as_ref()
                .expect("Headers not found")
                .get("X-Custom-Header")
                .expect("Header not found"),
            "TestValue"
        );
        assert_eq!(
            config
                .services
                .get("test")
                .expect("Service not found")
                .expect
                .status,
            Some(200)
        );

        // check method
        assert_eq!(
            config
                .services
                .get("test")
                .expect("Service not found")
                .method,
            HttpMethod::Get
        );

        // follow_redirects is not set
        assert_eq!(
            config
                .services
                .get("test")
                .expect("Service not found")
                .follow_redirects,
            None
        );

        assert_eq!(
            config
                .services
                .get("test")
                .expect("Service not found")
                .max_bytes,
            None // matcher-aware defaults are applied at scan time
        );
    }

    #[test]
    fn test_bad_config_url_and_test() {
        let yaml = r#"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    headers:
      X-Custom-Header: TestValue
    expect:
      status: 200
    test: "echo test"
      "#;

        let tmp_file = create_config(yaml);
        let config_file = tmp_file.path().to_path_buf();
        let config = Config::new(config_file);

        assert!(config.is_err());
    }

    #[test]
    fn test_bad_config_missing_url_and_test() {
        let yaml = r"
---
services:
  test:
    every: 30s
    headers:
      X-Custom-Header: TestValue
    expect:
      status: 200
      ";

        let tmp_file = create_config(yaml);
        let config_file = tmp_file.path().to_path_buf();
        let config = Config::new(config_file);

        assert!(config.is_err());
    }

    #[test]
    fn test_invalid_body_regex_is_rejected_at_startup() {
        let yaml = r#"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
      body: r"(unclosed"
      "#;

        let tmp_file = create_config(yaml);
        let config = Config::new(tmp_file.path().to_path_buf());

        let err = config.expect_err("Invalid regex should be rejected at startup");
        assert!(format!("{err:?}").contains("expect.body"));
    }

    #[test]
    fn test_invalid_body_not_regex_is_rejected_at_startup() {
        let yaml = r#"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    expect:
      body_not: r"[a-"
      "#;

        let tmp_file = create_config(yaml);
        let config = Config::new(tmp_file.path().to_path_buf());

        let err = config.expect_err("Invalid regex should be rejected at startup");
        assert!(format!("{err:?}").contains("expect.body_not"));
    }

    #[test]
    fn test_empty_body_pattern_is_rejected_at_startup() {
        let yaml = r#"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
      body: ""
      "#;

        let tmp_file = create_config(yaml);
        let config = Config::new(tmp_file.path().to_path_buf());

        assert!(config.is_err());
    }

    #[test]
    fn test_valid_raw_regex_passes_startup_validation() {
        let yaml = r#"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    expect:
      body: r"(?m)^pg_up 1$"
      body_not: r"error|failure|Fatal"
      "#;

        let tmp_file = create_config(yaml);
        let config = Config::new(tmp_file.path().to_path_buf());

        assert!(config.is_ok());
    }

    #[test]
    fn test_unknown_service_key_is_rejected() {
        // Regression for issue #20: `max_size` (instead of `max_bytes`) was
        // silently ignored, leaving the user with the 512KB default.
        let yaml = r"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    max_size: 10485760
    expect:
      status: 200
      ";

        let tmp_file = create_config(yaml);
        let config = Config::new(tmp_file.path().to_path_buf());

        let err = config.expect_err("Unknown key should be rejected");
        assert!(format!("{err:?}").contains("max_size"));
    }

    #[test]
    fn test_unknown_expect_key_is_rejected() {
        let yaml = r"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
      bodi: typo
      ";

        let tmp_file = create_config(yaml);
        let config = Config::new(tmp_file.path().to_path_buf());

        let err = config.expect_err("Unknown expect key should be rejected");
        assert!(format!("{err:?}").contains("bodi"));
    }

    #[test]
    fn test_all_http_methods_case_insensitive() {
        let methods = [
            "GET", "get", "Get", "POST", "post", "Post", "PUT", "put", "Put", "DELETE", "delete",
            "Delete", "PATCH", "patch", "Patch", "HEAD", "head", "Head", "OPTIONS", "options",
            "Options", "CONNECT", "connect", "Connect", "TRACE", "trace", "Trace",
        ];

        for method in methods {
            let yaml = format!(
                r"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    method: {method}
    expect:
      status: 200
"
            );

            let tmp_file = create_config(&yaml);
            let config_file = tmp_file.path().to_path_buf();
            let config = Config::new(config_file).expect("Failed to load config");

            assert_eq!(
                config
                    .services
                    .get("test")
                    .expect("Service not found")
                    .method
                    .to_string(),
                method.to_uppercase(),
                "Failed for method: {method}"
            );
        }
    }

    #[test]
    fn test_body_type_json() {
        let yaml = r"
---
services:
  test:
    url: https://epazote.io
    method: POST
    body:
      json:
        key: value
        oi: hola
    every: 30s
    expect:
      status: 200
    ";

        let expected_json = json!({
            "key": "value",
            "oi": "hola"
        });

        let tmp_file = create_config(yaml);
        let config_file = tmp_file.path().to_path_buf();
        let config = Config::new(config_file).expect("Failed to load config");

        let service = config.services.get("test").expect("Service not found");
        let body = service.body.as_ref().expect("Body not found");

        assert_eq!(body, &BodyType::Json(expected_json));
    }

    #[test]
    fn test_expect_json() {
        let yaml = r"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
      json:
        status: success
        data:
          activeTargets:
            - health: up
    ";

        let tmp_file = create_config(yaml);
        let config_file = tmp_file.path().to_path_buf();
        let config = Config::new(config_file).expect("Failed to load config");

        let expected_json = json!({
            "status": "success",
            "data": {
                "activeTargets": [
                    { "health": "up" }
                ]
            }
        });

        let service = config.services.get("test").expect("Service not found");
        let body = service
            .expect
            .json
            .as_ref()
            .expect("JSON expectation not found");

        assert_eq!(body, &expected_json);
    }

    #[test]
    fn test_expect_body_not() {
        let yaml = r"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    expect:
      body_not: Failure
    ";

        let tmp_file = create_config(yaml);
        let config_file = tmp_file.path().to_path_buf();
        let config = Config::new(config_file).expect("Failed to load config");

        let service = config.services.get("test").expect("Service not found");

        assert_eq!(service.expect.body_not.as_deref(), Some("Failure"));
        assert_eq!(service.expect.status, None);
    }

    #[test]
    fn test_command_expect_requires_status() {
        let yaml = r"
---
services:
  test:
    test: pgrep -x nginx
    every: 30s
    expect:
      body_not: Failure
    ";

        let tmp_file = create_config(yaml);
        let config_file = tmp_file.path().to_path_buf();
        let config = Config::new(config_file);

        assert!(config.is_err());
    }

    #[test]
    fn test_expect_body_and_json_are_mutually_exclusive() {
        let yaml = r"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
      body: success
      json:
        status: success
    ";

        let tmp_file = create_config(yaml);
        let config_file = tmp_file.path().to_path_buf();
        let config = Config::new(config_file);

        assert!(config.is_err());
    }

    #[test]
    fn test_expect_if_not_threshold_and_stop() {
        let yaml = r"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
      json:
        status: success
      if_not:
        threshold: 3
        stop: 2
        cmd: systemctl restart test
    ";

        let tmp_file = create_config(yaml);
        let config_file = tmp_file.path().to_path_buf();
        let config = Config::new(config_file).expect("Failed to load config");

        let service = config.services.get("test").expect("Service not found");
        let if_not = service.expect.if_not.as_ref().expect("if_not not found");

        assert_eq!(if_not.threshold, Some(3));
        assert_eq!(if_not.stop, Some(2));
        assert_eq!(if_not.cmd.as_deref(), Some("systemctl restart test"));
    }
}
