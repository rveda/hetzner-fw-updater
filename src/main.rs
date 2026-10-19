//! Command hetzner-fw-updater manages the rules of Hetzner Cloud firewalls
//! described by a JSON config file.

use std::cell::OnceCell;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use ureq::config::IpFamily;
use ureq::Agent;

const API_BASE: &str = "https://api.hetzner.cloud/v1";
const IP_LOOKUP: &str = "https://ip.hetzner.com";
/// Config file used when the command line does not name one.
const DEFAULT_CONFIG: &str = "/etc/hetzner-fw/config.json";
/// Placeholder in `source_ips` / `destination_ips` that update replaces
/// with the caller's public IP addresses.
const MY_PUBLIC_IP: &str = "{{my-public-ip}}";
/// Rule fields in which the placeholder is replaced.
const ADDRESS_FIELDS: [&str; 2] = ["source_ips", "destination_ips"];
/// How long check waits for a health endpoint before treating it as blocked.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(15);
/// How long check waits after the new rules are applied before probing again.
const HEALTH_RECHECK_DELAY: Duration = Duration::from_secs(3);
/// How long set_rules waits for Hetzner to apply the rules.
const ACTION_TIMEOUT: Duration = Duration::from_secs(60);
/// How often set_rules asks whether the rules have been applied.
const ACTION_POLL_INTERVAL: Duration = Duration::from_secs(1);

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn usage(prog: &str) -> ExitCode {
    eprintln!(
        "Usage:
  {prog} download [<config file>] <firewall name>
  {prog} update [<config file>]
  {prog} check [<config file>]

Commands:
  download        Download a firewall's rules into the config file, adding the
                  firewall to the file if it is not there yet
  update          Update every firewall in the config file with its rules.
                  A firewall's rules are replaced completely: rules that are
                  not in the config file are removed
  check           Probe the health-endpoint of each firewall that has one, and
                  update the firewall's rules if the endpoint times out
                  or refuses the connection

The config file defaults to {DEFAULT_CONFIG}.

Example:
  {prog} update config.json"
    );
    ExitCode::from(1)
}

#[derive(Debug)]
enum Command {
    Download { firewall_name: String },
    Update,
    Check,
}

impl Command {
    /// Parses the command name and the arguments after it into the command
    /// and the config file path, which defaults to `DEFAULT_CONFIG`.
    fn parse(name: &str, rest: &[String]) -> std::result::Result<(Self, PathBuf), String> {
        let (config, rest) = match (name, rest) {
            ("download", [config, rest @ ..]) if !rest.is_empty() => (Some(config), rest),
            ("update" | "check", [config, rest @ ..]) => (Some(config), rest),
            _ => (None, rest),
        };
        let command = match (name, rest) {
            ("download", [firewall_name]) => Command::Download {
                firewall_name: firewall_name.clone(),
            },
            ("download", []) => return Err("download requires a firewall name".into()),
            ("update", []) => Command::Update,
            ("check", []) => Command::Check,
            ("download" | "update" | "check", _) => return Err(format!("too many arguments for {name}")),
            _ => return Err(format!("unknown command {name:?}")),
        };
        Ok((command, PathBuf::from(config.map_or(DEFAULT_CONFIG, String::as_str))))
    }
}

/// Configuration of one firewall. Rules are kept as generic JSON so they are
/// passed to the API as written.
#[derive(Debug, Deserialize, Serialize)]
struct FirewallConfig {
    #[serde(rename = "firewall-name")]
    firewall_name: String,
    /// URL that check requests to tell whether the firewall lets us through.
    #[serde(rename = "health-endpoint", default, skip_serializing_if = "Option::is_none")]
    health_endpoint: Option<String>,
    /// `None` when the entry has no `rules` key (or it is null). update
    /// refuses to run then, so a missing key never wipes a firewall.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rules: Option<Vec<Value>>,
    /// Any other keys in the entry, kept so saving does not drop them.
    #[serde(flatten)]
    other: Map<String, Value>,
}

/// Contents of the JSON config file: an array of firewall configurations.
#[derive(Debug, Default)]
struct Config {
    firewalls: Vec<FirewallConfig>,
}

impl Config {
    fn load(path: &Path) -> Result<Self> {
        let data = fs::read_to_string(path)?;
        let firewalls: Vec<FirewallConfig> = serde_json::from_str(&data)?;
        let mut seen = HashSet::new();
        for fw in &firewalls {
            if !seen.insert(&fw.firewall_name) {
                return Err(format!("firewall {:?} is configured more than once", fw.firewall_name).into());
            }
        }
        Ok(Config { firewalls })
    }

    /// Writes the config as JSON indented with 4 spaces. The file is written
    /// to a temporary file first, synced to disk and then renamed, so neither
    /// a failed write nor a crash leaves a truncated config behind.
    fn save(&self, path: &Path) -> Result<()> {
        let mut data = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
        let mut ser = serde_json::Serializer::with_formatter(&mut data, formatter);
        self.firewalls.serialize(&mut ser)?;
        data.push(b'\n');

        // The process ID keeps concurrent runs from sharing a temporary file.
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(format!(".{}.tmp", std::process::id()));
        let tmp = Path::new(&tmp);
        let written = File::create(tmp)
            .and_then(|mut file| {
                file.write_all(&data)?;
                file.sync_all()
            })
            .and_then(|()| fs::rename(tmp, path));
        if written.is_err() {
            let _ = fs::remove_file(tmp);
        }
        written?;

        // Sync the directory too, so the rename itself survives a crash.
        #[cfg(unix)]
        {
            let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
            File::open(dir)?.sync_all()?;
        }
        Ok(())
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().map_or("hetzner-fw-updater", String::as_str);
    if args.len() < 2 {
        return usage(prog);
    }
    let (command, config_path) = match Command::parse(&args[1], &args[2..]) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return usage(prog);
        }
    };
    ExitCode::from(run(command, &config_path))
}

fn run(command: Command, config_path: &Path) -> u8 {
    let config = match Config::load(config_path) {
        Ok(c) => c,
        // download may start a new config file.
        Err(e) if matches!(command, Command::Download { .. }) && is_not_found(&*e) => Config::default(),
        Err(e) => {
            eprintln!("could not read config file {}: {e}", config_path.display());
            return 1;
        }
    };
    if let Err(e) = validate(&command, &config) {
        eprintln!("invalid config file {}: {e}", config_path.display());
        return 1;
    }
    let token = match std::env::var("API_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => {
            eprintln!("API_TOKEN environment variable is not set");
            return 1;
        }
    };
    let api = Client::new(token);

    let result = match command {
        Command::Download { firewall_name } => download_rules(&api, config, config_path, &firewall_name),
        Command::Update => update_rules(&api, &config),
        Command::Check => check(&api, &config),
    };
    if let Err(e) = result {
        eprintln!("{e}");
        return 2;
    }
    0
}

fn is_not_found(e: &(dyn std::error::Error + 'static)) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

/// Checks the entries whose rules the command may upload, so a config error
/// is reported before anything is looked up or changed.
fn validate(command: &Command, config: &Config) -> std::result::Result<(), String> {
    match command {
        Command::Download { .. } => return Ok(()),
        Command::Update if config.firewalls.is_empty() => return Err("contains no firewalls".into()),
        _ => {}
    }
    // check only uploads the rules of firewalls with a health-endpoint.
    let uploaded = |fw: &&FirewallConfig| !matches!(command, Command::Check) || fw.health_endpoint.is_some();
    for fw in config.firewalls.iter().filter(uploaded) {
        rules_to_upload(fw).map_err(|e| format!("firewall {:?}: {e}", fw.firewall_name))?;
    }
    Ok(())
}

/// Returns the firewall's configured rules. Fails if the entry has no rules
/// or uses the placeholder wrongly.
fn rules_to_upload(fw: &FirewallConfig) -> Result<&[Value]> {
    let rules = fw.rules.as_deref().ok_or("has no \"rules\" key")?;
    check_placeholders(rules)?;
    Ok(rules)
}

/// Replaces the rules of every firewall in the config file with the rules
/// configured for it. The config must have passed `validate`, so a config
/// error never leaves the firewalls half updated. If updating one firewall
/// fails, the others are still updated.
fn update_rules(api: &Client, config: &Config) -> Result<()> {
    let updates = prepare_updates(config.firewalls.iter(), &PublicIps::default())?;

    let mut failed = 0;
    for (name, rules) in updates {
        if let Err(e) = upload_rules(api, name, rules) {
            eprintln!("firewall {name:?}: {e}");
            failed += 1;
        }
    }
    if failed > 0 {
        return Err(format!("failed to update {failed} of {} firewalls", config.firewalls.len()).into());
    }
    Ok(())
}

/// The caller's public addresses, looked up at most once per run. A failed
/// lookup is not retried either.
#[derive(Default)]
struct PublicIps(OnceCell<std::result::Result<Vec<String>, String>>);

impl PublicIps {
    fn get(&self) -> Result<&[String]> {
        self.0
            .get_or_init(|| {
                let ips = my_public_ips().map_err(|e| e.to_string())?;
                println!("Replacing {MY_PUBLIC_IP} with {}", ips.join(", "));
                Ok(ips)
            })
            .as_deref()
            .map_err(|e| e.as_str().into())
    }
}

/// Returns the rules to upload for each of `firewalls`, with placeholders
/// replaced. The firewalls must have passed `validate`.
fn prepare_updates<'a>(
    firewalls: impl IntoIterator<Item = &'a FirewallConfig>,
    public_ips: &PublicIps,
) -> Result<Vec<(&'a str, Vec<Value>)>> {
    let mut updates = Vec::new();
    for fw in firewalls {
        updates.push((fw.firewall_name.as_str(), rules_to_upload(fw)?.to_vec()));
    }

    if updates.iter().any(|(_, rules)| uses_public_ip_placeholder(rules)) {
        let my_ips = public_ips.get()?;
        for (_, rules) in &mut updates {
            replace_public_ip_placeholder(rules, my_ips);
        }
    }
    Ok(updates)
}

/// Uploads the rules unless the firewall already has them. Returns whether
/// the firewall was changed.
fn upload_rules(api: &Client, name: &str, rules: Vec<Value>) -> Result<bool> {
    let count = rules.len();
    let (firewall_id, current) = api.get_firewall(name)?;
    if same_rules(&current, &rules) {
        println!("Firewall {name:?} already has the configured {count} rules");
        return Ok(false);
    }
    api.set_rules(firewall_id, rules)?;
    println!("Uploaded {count} rules to firewall {name:?}");
    Ok(true)
}

/// Tells whether two rule sets are equivalent. Rule order, key order and the
/// order of addresses do not matter, and a null or empty value is the same
/// as a missing key, because the API returns those for keys the config may
/// leave out. Anything else that differs counts as a change, so a rule set is
/// at worst uploaded needlessly.
fn same_rules(a: &[Value], b: &[Value]) -> bool {
    fn normalize(rules: &[Value]) -> Vec<String> {
        let mut normalized: Vec<String> = rules
            .iter()
            .map(|rule| {
                let mut rule = rule.clone();
                if let Some(fields) = rule.as_object_mut() {
                    fields.retain(|_, v| !v.is_null() && v.as_array().is_none_or(|a| !a.is_empty()));
                    for field in ADDRESS_FIELDS {
                        if let Some(ips) = fields.get_mut(field).and_then(Value::as_array_mut) {
                            ips.sort_by_key(Value::to_string);
                        }
                    }
                    fields.sort_keys();
                }
                rule.to_string()
            })
            .collect();
        normalized.sort();
        normalized
    }
    normalize(a) == normalize(b)
}

/// Result of requesting a health endpoint.
#[derive(Debug)]
enum Health {
    Ok,
    /// The request timed out or the connection was refused, which is what
    /// the firewall does to us. Holds a description of which one.
    Blocked(&'static str),
    Failed(String),
}

/// Requests each configured health endpoint. A firewall whose endpoint times
/// out or refuses the connection has its rules updated, and the endpoint is requested again after a
/// short delay to confirm that the update let us through.
fn check(api: &Client, config: &Config) -> Result<()> {
    let public_ips = PublicIps::default();
    let mut checked = 0;
    let mut failed = 0;
    for fw in &config.firewalls {
        let Some(url) = &fw.health_endpoint else {
            continue;
        };
        checked += 1;
        let name = &fw.firewall_name;
        let reason = match probe(url) {
            Health::Ok => {
                println!("firewall {name:?}: {url} is reachable");
                continue;
            }
            Health::Failed(e) => {
                eprintln!("firewall {name:?}: {url}: {e}");
                failed += 1;
                continue;
            }
            Health::Blocked(reason) => {
                println!("firewall {name:?}: {url}: {reason}; updating rules");
                reason
            }
        };

        let updated = prepare_updates([fw], &public_ips).and_then(|updates| {
            updates
                .into_iter()
                .try_fold(false, |changed, (name, rules)| Ok(upload_rules(api, name, rules)? || changed))
        });
        match updated {
            Ok(true) => {}
            // The firewall already let us through, so something else blocks
            // the endpoint, such as the service not running.
            Ok(false) => {
                eprintln!("error: firewall {name:?}: {url}: {reason}, but the firewall already has the configured rules");
                failed += 1;
                continue;
            }
            Err(e) => {
                eprintln!("firewall {name:?}: {e}");
                failed += 1;
                continue;
            }
        }

        thread::sleep(HEALTH_RECHECK_DELAY);
        match probe(url) {
            Health::Ok => println!("firewall {name:?}: {url} is reachable after the update"),
            Health::Blocked(reason) => {
                eprintln!("error: firewall {name:?}: {url} after updating the rules: {reason}");
                failed += 1;
            }
            Health::Failed(e) => {
                eprintln!("error: firewall {name:?}: {url} after updating the rules: {e}");
                failed += 1;
            }
        }
    }

    if checked == 0 {
        println!("no firewall in the config file has a health-endpoint");
    }
    if failed > 0 {
        return Err(format!("{failed} of {checked} health checks failed").into());
    }
    Ok(())
}

/// Sends a GET request to `url` over IPv4 and IPv6 at the same time. The
/// endpoint is healthy if either answers with 200, and blocked if neither
/// does and at least one timed out or was refused.
///
/// The families are tried separately because ureq reports only the error for
/// the last address it tried: an IPv4 timeout followed by an instant IPv6
/// "network unreachable" would otherwise not count as a timeout.
fn probe(url: &str) -> Health {
    let results = thread::scope(|s| {
        [("IPv4", IpFamily::Ipv4Only), ("IPv6", IpFamily::Ipv6Only)]
            .map(|(label, family)| (label, s.spawn(move || probe_family(url, family))))
            .map(|(label, handle)| (label, handle.join().unwrap_or_else(|_| Health::Failed("panicked".into()))))
    });
    combine(results)
}

/// Combines the results for each IP family: reachable over either beats
/// blocked over either, which beats failed.
fn combine(results: [(&str, Health); 2]) -> Health {
    if results.iter().any(|(_, h)| matches!(h, Health::Ok)) {
        return Health::Ok;
    }
    if let Some(reason) = results.iter().find_map(|(_, h)| match h {
        Health::Blocked(reason) => Some(*reason),
        _ => None,
    }) {
        return Health::Blocked(reason);
    }
    let errors: Vec<String> = results
        .iter()
        .filter_map(|(label, h)| match h {
            Health::Failed(e) => Some(format!("{label}: {e}")),
            _ => None,
        })
        .collect();
    Health::Failed(errors.join("; "))
}

/// Sends a GET request to `url` using only the given IP family. Only a 200
/// response counts as healthy.
fn probe_family(url: &str, family: IpFamily) -> Health {
    let agent: Agent = Agent::config_builder()
        .ip_family(family)
        .timeout_global(Some(HEALTH_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into();
    match agent.get(url).call() {
        Ok(resp) if resp.status() == 200 => Health::Ok,
        Ok(resp) => Health::Failed(format!("unexpected status {}", resp.status())),
        Err(ureq::Error::Timeout(_)) => Health::Blocked("timed out"),
        Err(ureq::Error::Io(e)) if e.kind() == std::io::ErrorKind::TimedOut => Health::Blocked("timed out"),
        Err(ureq::Error::Io(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            Health::Blocked("connection refused")
        }
        Err(e) => Health::Failed(e.to_string()),
    }
}

fn uses_public_ip_placeholder(rules: &[Value]) -> bool {
    rules.iter().any(|rule| {
        ADDRESS_FIELDS.iter().any(|field| {
            rule[*field]
                .as_array()
                .is_some_and(|ips| ips.iter().any(|ip| ip.as_str().is_some_and(|s| s.contains(MY_PUBLIC_IP))))
        })
    })
}

/// Returns the caller's public addresses in CIDR form (`<ipv4>/32`,
/// `<ipv6>/128`). A failed lookup for one family is only a warning.
fn my_public_ips() -> Result<Vec<String>> {
    let mut ips = Vec::new();
    match public_ip(IpFamily::Ipv4Only) {
        Ok(ip) => ips.push(format!("{ip}/32")),
        Err(e) => eprintln!("warning: could not determine IPv4 address: {e}"),
    }
    match public_ip(IpFamily::Ipv6Only) {
        Ok(ip) => ips.push(format!("{ip}/128")),
        Err(e) => eprintln!("warning: could not determine IPv6 address: {e}"),
    }
    if ips.is_empty() {
        return Err("could not determine any public IP address".into());
    }
    Ok(ips)
}

/// Rejects the placeholder combined with other text (such as
/// `{{my-public-ip}}/24`), because the result would not be a valid address
/// for both IP families.
fn check_placeholders(rules: &[Value]) -> Result<()> {
    for rule in rules {
        for field in ADDRESS_FIELDS {
            for ip in rule[field].as_array().into_iter().flatten() {
                if let Some(s) = ip.as_str().filter(|s| *s != MY_PUBLIC_IP && s.contains(MY_PUBLIC_IP)) {
                    return Err(
                        format!("invalid address {s:?} in {field}: {MY_PUBLIC_IP} must be the whole entry").into(),
                    );
                }
            }
        }
    }
    Ok(())
}

/// Replaces each address entry that is exactly the placeholder with all of
/// `my_ips`.
fn replace_public_ip_placeholder(rules: &mut [Value], my_ips: &[String]) {
    for rule in rules {
        for field in ADDRESS_FIELDS {
            let Some(ips) = rule.get_mut(field).and_then(Value::as_array_mut) else {
                continue;
            };
            let mut replaced = Vec::with_capacity(ips.len());
            for ip in ips.drain(..) {
                if ip == MY_PUBLIC_IP {
                    replaced.extend(my_ips.iter().cloned().map(Value::String));
                } else {
                    replaced.push(ip);
                }
            }
            *ips = replaced;
        }
    }
}

/// Replaces the rules of the named firewall in the config file with the
/// firewall's current rules. The firewall is appended to the config file if it
/// is not there yet.
fn download_rules(api: &Client, mut config: Config, config_path: &Path, firewall_name: &str) -> Result<()> {
    let (_, rules) = api.get_firewall(firewall_name)?;
    let count = rules.len();
    if set_firewall_rules(&mut config, firewall_name, rules) {
        println!("Added firewall {firewall_name:?} to {}", config_path.display());
    }
    config
        .save(config_path)
        .map_err(|e| format!("could not write config file {}: {e}", config_path.display()))?;
    println!(
        "Downloaded {count} rules from firewall {firewall_name:?} to {}",
        config_path.display()
    );
    Ok(())
}

/// Replaces the rules of the named firewall in `config`, appending the
/// firewall if it is not there yet. Returns whether it was appended.
fn set_firewall_rules(config: &mut Config, firewall_name: &str, rules: Vec<Value>) -> bool {
    if let Some(fw) = config.firewalls.iter_mut().find(|fw| fw.firewall_name == firewall_name) {
        fw.rules = Some(rules);
        return false;
    }
    config.firewalls.push(FirewallConfig {
        firewall_name: firewall_name.to_string(),
        health_endpoint: None,
        rules: Some(rules),
        other: Map::new(),
    });
    true
}

/// Asks ip.hetzner.com for our address, forcing the given IP family
/// like curl -4 / curl -6.
fn public_ip(family: IpFamily) -> Result<IpAddr> {
    let agent: Agent = Agent::config_builder()
        .ip_family(family)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into();
    // ip.hetzner.com only answers with the bare address for known CLI user
    // agents; anything else gets an HTML page.
    let body = agent
        .get(IP_LOOKUP)
        .header("User-Agent", "curl/8.0")
        .call()?
        .body_mut()
        .read_to_string()?;
    let ip = body.trim();
    ip.parse()
        .map_err(|_| format!("unexpected response {ip:?}").into())
}

/// Turns a raw API response into the parsed JSON body, or an error if the
/// API reported one or the status indicates failure.
fn api_body(mut resp: ureq::http::Response<ureq::Body>) -> Result<Value> {
    let status = resp.status();
    let data = resp.body_mut().read_to_string()?;
    parse_api_body(status, &data)
}

/// The part of `api_body` that does not need a response, so it can be tested.
fn parse_api_body(status: ureq::http::StatusCode, data: &str) -> Result<Value> {
    let parsed: Option<Value> = serde_json::from_str(data).ok();

    if let Some(err) = parsed.as_ref().and_then(|v| v.get("error")).filter(|e| !e.is_null()) {
        let code = err["code"].as_str().unwrap_or_default();
        let message = err["message"].as_str().unwrap_or_default();
        return Err(format!("API error ({code}): {message}").into());
    }
    if status.as_u16() >= 300 {
        return Err(format!("API request failed: {status}: {data}").into());
    }
    Ok(parsed.unwrap_or(Value::Null))
}

struct Client {
    /// Value of the Authorization header.
    auth: String,
    agent: Agent,
}

impl Client {
    fn new(token: String) -> Self {
        let agent = Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            .build()
            .into();
        Client {
            auth: format!("Bearer {token}"),
            agent,
        }
    }

    /// Returns the ID and rules of the named firewall. Rules are kept as
    /// generic JSON so they are written back as received.
    fn get_firewall(&self, name: &str) -> Result<(i64, Vec<Value>)> {
        let resp = self
            .agent
            .get(format!("{API_BASE}/firewalls"))
            .query("name", name)
            .header("Authorization", self.auth.as_str())
            .call()?;
        let body = api_body(resp)?;

        let fw = body["firewalls"]
            .get(0)
            .ok_or_else(|| format!("firewall {name:?} not found"))?;
        let id = fw["id"]
            .as_i64()
            .ok_or("API response is missing the firewall id")?;
        let rules = fw["rules"].as_array().cloned().unwrap_or_default();
        Ok((id, rules))
    }

    /// Replaces the firewall's rules and waits until Hetzner has applied them.
    fn set_rules(&self, firewall_id: i64, rules: Vec<Value>) -> Result<()> {
        let resp = self
            .agent
            .post(format!("{API_BASE}/firewalls/{firewall_id}/actions/set_rules"))
            .header("Authorization", self.auth.as_str())
            .send_json(json!({ "rules": rules }))?;
        let mut body = api_body(resp)?;
        if let Some(actions) = body["actions"].as_array_mut() {
            for action in actions.drain(..) {
                self.wait_for_action(action)?;
            }
        }
        Ok(())
    }

    /// Polls a firewall action until it has finished. Fails if it reports an
    /// error or does not finish within `ACTION_TIMEOUT`.
    fn wait_for_action(&self, mut action: Value) -> Result<()> {
        let deadline = Instant::now() + ACTION_TIMEOUT;
        loop {
            match action["status"].as_str() {
                Some("success") => return Ok(()),
                Some("error") => {
                    let code = action["error"]["code"].as_str().unwrap_or_default();
                    let message = action["error"]["message"].as_str().unwrap_or_default();
                    return Err(format!("applying the rules failed ({code}): {message}").into());
                }
                _ if Instant::now() >= deadline => {
                    return Err(format!("rules were not applied within {} s", ACTION_TIMEOUT.as_secs()).into());
                }
                _ => {}
            }
            thread::sleep(ACTION_POLL_INTERVAL);
            let id = action["id"].as_i64().ok_or("API response is missing the action id")?;
            let resp = self
                .agent
                .get(format!("{API_BASE}/firewalls/actions/{id}"))
                .header("Authorization", self.auth.as_str())
                .call()?;
            action = api_body(resp)?["action"].take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_replaces_rules_and_keeps_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, r#"[{"firewall-name": "fw", "note": "keep", "rules": [{"old": 1}]}]"#).unwrap();

        let mut config = Config::load(&path).unwrap();
        config.firewalls[0].rules = Some(vec![json!({"direction": "in", "port": "22"})]);
        config.save(&path).unwrap();

        let saved = fs::read_to_string(&path).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1, "temporary file left behind");
        assert_eq!(
            saved,
            r#"[
    {
        "firewall-name": "fw",
        "rules": [
            {
                "direction": "in",
                "port": "22"
            }
        ],
        "note": "keep"
    }
]
"#
        );
    }

    #[test]
    fn duplicate_firewall_names_are_rejected() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), r#"[{"firewall-name": "fw"}, {"firewall-name": "fw"}]"#).unwrap();
        assert!(Config::load(file.path()).is_err());
    }

    #[test]
    fn command_arguments_are_checked() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let default = Path::new(DEFAULT_CONFIG);
        assert!(matches!(
            Command::parse("download", &args(&["fw"])),
            Ok((Command::Download { firewall_name }, path)) if firewall_name == "fw" && path == default
        ));
        assert!(matches!(
            Command::parse("download", &args(&["c.json", "fw"])),
            Ok((Command::Download { firewall_name }, path)) if firewall_name == "fw" && path == Path::new("c.json")
        ));
        assert!(Command::parse("download", &[]).is_err());
        assert!(Command::parse("download", &args(&["c.json", "fw", "x"])).is_err());
        assert!(matches!(Command::parse("update", &[]), Ok((Command::Update, path)) if path == default));
        assert!(matches!(
            Command::parse("update", &args(&["c.json"])),
            Ok((Command::Update, path)) if path == Path::new("c.json")
        ));
        assert!(Command::parse("update", &args(&["c.json", "fw"])).is_err());
        assert!(matches!(Command::parse("check", &[]), Ok((Command::Check, path)) if path == default));
        assert!(matches!(
            Command::parse("check", &args(&["c.json"])),
            Ok((Command::Check, path)) if path == Path::new("c.json")
        ));
        assert!(Command::parse("check", &args(&["c.json", "fw"])).is_err());
        assert!(Command::parse("delete-rules", &[]).is_err());
    }

    #[test]
    fn placeholder_is_replaced_with_all_public_ips() {
        let mut rules = vec![
            json!({"source_ips": ["{{my-public-ip}}", "10.0.0.0/8"], "destination_ips": []}),
            json!({"source_ips": ["0.0.0.0/0"], "destination_ips": ["{{my-public-ip}}"]}),
        ];
        assert!(uses_public_ip_placeholder(&rules));

        assert!(check_placeholders(&rules).is_ok());

        let my_ips = vec!["1.2.3.4/32".to_string(), "2001:db8::1/128".to_string()];
        replace_public_ip_placeholder(&mut rules, &my_ips);

        assert_eq!(
            rules,
            vec![
                json!({"source_ips": ["1.2.3.4/32", "2001:db8::1/128", "10.0.0.0/8"], "destination_ips": []}),
                json!({"source_ips": ["0.0.0.0/0"], "destination_ips": ["1.2.3.4/32", "2001:db8::1/128"]}),
            ]
        );
        assert!(!uses_public_ip_placeholder(&rules));
    }

    #[test]
    fn placeholder_with_other_text_is_rejected() {
        let rules = vec![json!({"source_ips": ["{{my-public-ip}}/24"]})];
        assert!(check_placeholders(&rules).is_err());
    }

    fn config(json: &str) -> Config {
        Config {
            firewalls: serde_json::from_str(json).unwrap(),
        }
    }

    #[test]
    fn update_validates_every_entry() {
        let config = config(
            r#"[{"firewall-name": "ok", "rules": []},
                {"firewall-name": "bad", "rules": [{"source_ips": ["{{my-public-ip}}/24"]}]}]"#,
        );
        let err = validate(&Command::Update, &config).unwrap_err();
        assert!(err.starts_with(r#"firewall "bad": invalid address"#), "{err}");

        let config = self::config(r#"[{"firewall-name": "fw"}]"#);
        assert_eq!(
            validate(&Command::Update, &config).unwrap_err(),
            r#"firewall "fw": has no "rules" key"#
        );
        assert!(validate(&Command::Update, &Config::default()).is_err());
    }

    #[test]
    fn check_validates_only_entries_with_health_endpoint() {
        let config = config(
            r#"[{"firewall-name": "skipped"},
                {"firewall-name": "probed", "health-endpoint": "https://example.com", "rules": []}]"#,
        );
        assert!(validate(&Command::Check, &config).is_ok());
        assert!(validate(&Command::Check, &Config::default()).is_ok());

        let config = self::config(r#"[{"firewall-name": "fw", "health-endpoint": "https://example.com"}]"#);
        assert!(validate(&Command::Check, &config).is_err());
    }

    #[test]
    fn probe_results_are_combined() {
        let blocked = || Health::Blocked("timed out");
        let failed = || Health::Failed("network unreachable".into());
        assert!(matches!(combine([("IPv4", blocked()), ("IPv6", Health::Ok)]), Health::Ok));
        assert!(matches!(
            combine([("IPv4", Health::Blocked("connection refused")), ("IPv6", blocked())]),
            Health::Blocked("connection refused")
        ));
        assert!(matches!(combine([("IPv4", failed()), ("IPv6", blocked())]), Health::Blocked("timed out")));
        assert!(matches!(
            combine([("IPv4", failed()), ("IPv6", Health::Failed("dns".into()))]),
            Health::Failed(e) if e == "IPv4: network unreachable; IPv6: dns"
        ));
    }

    #[test]
    fn equivalent_rule_sets_are_the_same() {
        let configured = vec![
            json!({"direction": "in", "protocol": "tcp", "port": "22", "source_ips": ["10.0.0.0/8", "1.2.3.4/32"]}),
            json!({"direction": "in", "protocol": "icmp", "source_ips": ["0.0.0.0/0"]}),
        ];
        let from_api = vec![
            json!({"description": null, "direction": "in", "port": null, "protocol": "icmp",
                   "source_ips": ["0.0.0.0/0"], "destination_ips": []}),
            json!({"description": null, "direction": "in", "port": "22", "protocol": "tcp",
                   "source_ips": ["1.2.3.4/32", "10.0.0.0/8"], "destination_ips": []}),
        ];
        assert!(same_rules(&configured, &from_api));
        assert!(same_rules(&[], &[]));

        let mut changed = from_api.clone();
        changed[1]["port"] = json!("2222");
        assert!(!same_rules(&configured, &changed));
        assert!(!same_rules(&configured, &from_api[..1]));
        assert!(!same_rules(&[], &from_api));
    }

    #[test]
    fn download_skips_validation() {
        let config = config(r#"[{"firewall-name": "fw"}]"#);
        assert!(validate(&Command::Download { firewall_name: "fw".into() }, &config).is_ok());
    }

    #[test]
    fn placeholder_outside_address_fields_is_ignored() {
        let rules = vec![json!({"description": "{{my-public-ip}}", "source_ips": ["0.0.0.0/0"]})];
        assert!(!uses_public_ip_placeholder(&rules));
    }

    #[test]
    fn api_errors_are_reported() {
        use ureq::http::StatusCode;
        let err = parse_api_body(
            StatusCode::NOT_FOUND,
            r#"{"error": {"code": "not_found", "message": "firewall not found"}}"#,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "API error (not_found): firewall not found");

        let err = parse_api_body(StatusCode::BAD_GATEWAY, "<html>bad gateway</html>").unwrap_err();
        assert_eq!(err.to_string(), "API request failed: 502 Bad Gateway: <html>bad gateway</html>");

        assert_eq!(parse_api_body(StatusCode::NO_CONTENT, "").unwrap(), Value::Null);
        assert_eq!(
            parse_api_body(StatusCode::OK, r#"{"error": null, "firewalls": []}"#).unwrap(),
            json!({"error": null, "firewalls": []})
        );
    }

    #[test]
    fn prepare_updates_refuses_entries_without_rules() {
        let config = config(r#"[{"firewall-name": "ok", "rules": []}, {"firewall-name": "bad"}]"#);
        let err = prepare_updates(&config.firewalls, &PublicIps::default()).unwrap_err();
        assert_eq!(err.to_string(), r#"has no "rules" key"#);
    }

    #[test]
    fn prepare_updates_replaces_placeholder() {
        let config = config(
            r#"[{"firewall-name": "a", "rules": [{"source_ips": ["{{my-public-ip}}"]}]},
                {"firewall-name": "b", "rules": [{"source_ips": ["0.0.0.0/0"]}]}]"#,
        );
        // A filled cell keeps the lookup from going to the network.
        let public_ips = PublicIps(OnceCell::from(Ok(vec!["1.2.3.4/32".to_string()])));
        let updates = prepare_updates(&config.firewalls, &public_ips).unwrap();
        assert_eq!(
            updates,
            vec![
                ("a", vec![json!({"source_ips": ["1.2.3.4/32"]})]),
                ("b", vec![json!({"source_ips": ["0.0.0.0/0"]})]),
            ]
        );
    }

    #[test]
    fn downloaded_rules_replace_or_add_entry() {
        let mut config = config(r#"[{"firewall-name": "fw", "health-endpoint": "https://example.com", "rules": []}]"#);
        let rules = vec![json!({"direction": "in"})];

        assert!(!set_firewall_rules(&mut config, "fw", rules.clone()));
        assert_eq!(config.firewalls.len(), 1);
        assert_eq!(config.firewalls[0].rules.as_ref(), Some(&rules));
        assert!(config.firewalls[0].health_endpoint.is_some());

        assert!(set_firewall_rules(&mut config, "new", rules.clone()));
        assert_eq!(config.firewalls.len(), 2);
        assert_eq!(config.firewalls[1].firewall_name, "new");
        assert_eq!(config.firewalls[1].rules.as_ref(), Some(&rules));
    }
}
