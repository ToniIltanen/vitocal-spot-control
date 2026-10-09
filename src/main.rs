use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use directories::ProjectDirs;
use rand::{Rng, distr::Alphanumeric};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use url::Url;

mod dashboard;

const API_BASE_URL: &str = "https://api.viessmann-climatesolutions.com/iot/v2";
const AUTHORIZE_URL: &str = "https://iam.viessmann-climatesolutions.com/idp/v3/authorize";
const TOKEN_URL: &str = "https://iam.viessmann-climatesolutions.com/idp/v3/token";
const DEFAULT_REDIRECT_URI: &str = "http://localhost:51125";
const INSTALLATION_ENERGY_FEATURE_FILTERS: &[&str] = &[
    "device.power.statusReport.consumption",
    "device.power.statusReport.production",
    "heating.compressors.0.power.consumption.current",
    "heating.compressors.0.power.consumption.total",
    "heating.compressors.0.power.consumption.heating",
    "heating.compressors.0.power.consumption.dhw",
    "heating.compressors.0.heat.production.current",
    "heating.heat.production.dhw",
    "heating.heat.production.heating",
    "heating.heat.production.summary.dhw",
    "heating.heat.production.summary.heating",
    "heating.heat.production.currentYear.dhw",
    "heating.heat.production.currentYear.heating",
    "heating.heat.production.total",
    "heating.heat.production.current",
    "heating.power.consumption.current",
    "heating.power.consumption.total",
    "heating.power.consumption.heating",
    "heating.power.consumption.dhw",
    "heating.power.consumption.cooling",
    "heating.power.consumption.summary.heating",
    "heating.power.consumption.summary.dhw",
    "heating.power.production.demandCoverage.current",
    "heating.power.production.productionCoverage.current",
    "heating.heatingRod.power.consumption.current",
    "heating.heatingRod.power.consumption.total",
    "heating.heatingRod.power.consumption.heating",
    "heating.heatingRod.power.consumption.dhw",
    "heating.heatingRod.power.consumption.summary.heating",
    "heating.heatingRod.power.consumption.summary.dhw",
    "heating.heatingRod.heat.production.current",
    "heating.solar.power.production",
];
const POWER_ENERGY_FEATURE_CANDIDATES: &[&str] = &[
    "device.power.statusReport.consumption",
    "device.power.statusReport.production",
    "heating.compressors.0.power.consumption.current",
    "heating.compressors.0.power.consumption.total",
    "heating.compressors.0.power.consumption.heating",
    "heating.compressors.0.power.consumption.dhw",
    "heating.compressors.0.power.consumption.cooling",
    "heating.compressors.0.power.consumption.heating.week",
    "heating.compressors.0.power.consumption.dhw.week",
    "heating.compressors.0.power.consumption.cooling.week",
    "heating.power.consumption.current",
    "heating.power.consumption.total",
    "heating.power.consumption.heating",
    "heating.power.consumption.dhw",
    "heating.power.consumption.cooling",
    "heating.power.consumption.summary.dhw",
    "heating.power.consumption.summary.heating",
    "heating.power.consumption.summary.cooling",
    "heating.power.production",
    "heating.power.production.current",
    "heating.power.production.heating",
    "heating.power.production.dhw",
    "heating.power.production.demandCoverage.current",
    "heating.power.production.productionCoverage.current",
    "heating.compressors.0.heat.production.current",
    "heating.compressors.0.heat.production.heating.week",
    "heating.compressors.0.heat.production.dhw.week",
    "heating.compressors.0.heat.production.cooling.week",
    "heating.heat.production.cooling",
    "heating.heat.production.heating",
    "heating.heat.production.dhw",
    "heating.heat.production.total",
    "heating.heat.production.current",
    "heating.heat.production.summary.cooling",
    "heating.heat.production.summary.heating",
    "heating.heat.production.summary.dhw",
    "heating.heat.production.currentYear.cooling",
    "heating.heat.production.currentYear.heating",
    "heating.heat.production.currentYear.dhw",
    "heating.heatingRod.power.consumption.current",
    "heating.heatingRod.power.consumption.heating",
    "heating.heatingRod.power.consumption.dhw",
    "heating.heatingRod.power.consumption.total",
    "heating.heatingRod.power.consumption.summary.dhw",
    "heating.heatingRod.power.consumption.summary.heating",
    "heating.heatingRod.heat.production.current",
    "heating.solar.power.production",
];

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StoredToken {
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
    pub(crate) expires_at: u64,
}

pub(crate) async fn api_post(
    http: &Client,
    access_token: &str,
    path: &str,
    body: &Value,
) -> Result<Value> {
    let response = http
        .post(format!("{API_BASE_URL}{path}"))
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .header("Accept", "application/vnd.siren+json")
        .json(body)
        .send()
        .await
        .with_context(|| format!("Viessmann API command failed: {path}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("Could not read Viessmann command response: {path}"))?;
    if !status.is_success() {
        bail!("Viessmann API returned {status} for {path}: {body}");
    }
    let value: Value = serde_json::from_str(&body)
        .with_context(|| format!("Viessmann API returned invalid command JSON: {path}"))?;
    if value
        .get("statusCode")
        .and_then(Value::as_u64)
        .is_some_and(|code| code >= 400)
    {
        bail!("Viessmann command was rejected for {path}: {value}");
    }
    Ok(value)
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

#[derive(Debug, Serialize)]
struct CommandSummary {
    name: String,
    executable: Option<bool>,
    parameters: Vec<String>,
    allowed_values: Vec<String>,
}

#[derive(Debug, Serialize)]
struct FeatureSummary {
    feature: String,
    enabled: Option<bool>,
    properties: Vec<String>,
    commands: Vec<CommandSummary>,
}

#[derive(Debug, Default)]
struct RoomTelemetry {
    temperature: Option<f64>,
    sensor_status: Option<String>,
    normal_heating: Option<f64>,
    reduced_heating: Option<f64>,
    comfort_heating: Option<f64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let monitor_mode = env::args().any(|argument| argument == "--monitor");
    let client_id = env::var("VIESSMANN_CLIENT_ID")
        .context("Set VIESSMANN_CLIENT_ID to your Viessmann Developer Portal client ID")?;
    let redirect_uri =
        env::var("VIESSMANN_REDIRECT_URI").unwrap_or_else(|_| DEFAULT_REDIRECT_URI.to_owned());
    let redirect = validate_redirect_uri(&redirect_uri)?;
    println!("OAuth redirect URI: {redirect}");

    let http = Client::builder()
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .context("Could not create HTTP client")?;
    let token_path = token_path()?;
    let mut token = get_valid_token(&http, &client_id, &redirect, &token_path).await?;

    let installations = api_get(
        &http,
        &token.access_token,
        "/equipment/installations?limit=1000",
    )
    .await?;
    probe_installation_energy_features(&http, &token.access_token, &installations).await?;
    let devices = fetch_v2_devices(&http, &token.access_token, &installations).await?;
    if devices.is_empty() {
        println!("The ViCare account has no registered installations or devices.");
        return Ok(());
    }

    if monitor_mode {
        let heat_pump = devices
            .iter()
            .find(|device| {
                device
                    .get("modelId")
                    .and_then(Value::as_str)
                    .zip(device.get("deviceType").and_then(Value::as_str))
                    .is_some_and(|(model, kind)| heat_pump_match_reason(model, kind).is_some())
            })
            .context("No supported Vitocal S-series heat pump found for power monitoring")?;
        let room_control = devices
            .iter()
            .find(|device| {
                device
                    .get("modelId")
                    .and_then(Value::as_str)
                    .zip(device.get("deviceType").and_then(Value::as_str))
                    .is_some_and(|(model, kind)| is_room_control(model, kind))
            })
            .context("No Smart_RoomControl room-temperature sensor found for safe price control")?;
        dashboard::run_monitor(
            &http,
            &mut token,
            &client_id,
            &token_path,
            heat_pump,
            room_control,
        )
        .await?;
        return Ok(());
    }

    let mut found_target = false;
    for device in devices {
        let model = device
            .get("modelId")
            .and_then(Value::as_str)
            .unwrap_or("unknown model");
        let device_type = device
            .get("deviceType")
            .and_then(Value::as_str)
            .unwrap_or("unknown type");
        println!("Device: {model} (type: {device_type})");

        if is_room_control(model, device_type) {
            println!("Read-only room-temperature feature discovery for {model}");
            let path = device_features_path(&device)?;
            let features = api_get(&http, &token.access_token, &path).await?;
            for measurement in summarize_power_measurements(&features)? {
                println!("Power/energy candidate: {measurement}");
            }
            for (room, telemetry) in summarize_room_telemetry(&features)? {
                let temperature = telemetry
                    .temperature
                    .map(|value| format!("{value:.1} C"))
                    .unwrap_or_else(|| "unavailable".to_owned());
                let status = telemetry.sensor_status.as_deref().unwrap_or("unknown");
                let normal = format_optional_temperature(telemetry.normal_heating);
                let reduced = format_optional_temperature(telemetry.reduced_heating);
                let comfort = format_optional_temperature(telemetry.comfort_heating);
                println!(
                    "{room}: sensor {temperature} ({status}); heating setpoints normal={normal}, reduced={reduced}, comfort={comfort}"
                );
            }
            continue;
        }

        let Some(match_reason) = heat_pump_match_reason(model, device_type) else {
            if device_type == "vitoconnect" {
                println!("Read-only power/energy feature discovery for {model}");
                let path = device_features_path(&device)?;
                let features = api_get(&http, &token.access_token, &path).await?;
                for measurement in summarize_power_measurements(&features)? {
                    println!("Power/energy candidate: {measurement}");
                }
                probe_power_energy_paths(&http, &token.access_token, &path).await;
            }
            continue;
        };
        found_target = true;
        println!(
            "Read-only feature discovery for {model} ({match_reason}; exact model not confirmed)"
        );

        let path = device_features_path(&device)?;
        let features = api_get(&http, &token.access_token, &path).await?;
        for measurement in summarize_power_measurements(&features)? {
            println!("Power/energy candidate: {measurement}");
        }
        probe_power_energy_paths(&http, &token.access_token, &path).await;
        for reference in summarize_heating_references(&features)? {
            println!("{reference}");
        }
        let summaries = summarize_features(&features)?;
        for summary in summaries.iter().filter(|summary| {
            summary.feature.starts_with("heating.") || !summary.commands.is_empty()
        }) {
            let enabled = match summary.enabled {
                Some(true) => "enabled",
                Some(false) => "disabled",
                None => "unknown",
            };
            let properties = if summary.properties.is_empty() {
                "-".to_owned()
            } else {
                summary.properties.join(", ")
            };
            let commands = if summary.commands.is_empty() {
                "-".to_owned()
            } else {
                summary
                    .commands
                    .iter()
                    .map(|command| {
                        let mut description = if command.parameters.is_empty() {
                            command.name.clone()
                        } else {
                            format!("{}({})", command.name, command.parameters.join(", "))
                        };
                        if !command.allowed_values.is_empty() {
                            description.push_str(" allowed=");
                            description.push_str(&command.allowed_values.join("|"));
                        }
                        description
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            println!(
                "{} [{enabled}] | properties: {properties} | commands: {commands}",
                summary.feature
            );
        }
    }

    if !found_target {
        eprintln!(
            "No Vitocal 222-S model or CU401B_S heat-pump alias was found. \
             Check the model names and device types listed above."
        );
    }
    Ok(())
}

async fn probe_power_energy_paths(http: &Client, access_token: &str, device_features_path: &str) {
    println!("Direct read-only probes for Viessmann power/energy features:");
    for feature in POWER_ENERGY_FEATURE_CANDIDATES {
        let path = format!("{}{feature}", device_features_path);
        match api_get(http, access_token, &path).await {
            Ok(response) => {
                println!("{feature}: {}", summarize_probe_response(&response));
            }
            Err(error) => {
                println!("{feature}: unavailable ({error:#})");
            }
        }
    }
}

async fn probe_installation_energy_features(
    http: &Client,
    access_token: &str,
    installations: &Value,
) -> Result<()> {
    let installations = response_data_array(
        installations,
        "Viessmann installations response did not contain a data array",
    )?;
    let request = json!({
        "filter": INSTALLATION_ENERGY_FEATURE_FILTERS
    });

    for installation in installations {
        let installation_id = optional_identifier(installation, &["id", "installationId"])
            .context("Viessmann installation record has no id")?;
        let path = format!(
            "/features/installations/{}/features/filter",
            path_segment(&installation_id)
        );
        println!("Installation-wide energy feature search (read-only): {path}");
        match api_post(http, access_token, &path, &request).await {
            Ok(response) => {
                for result in summarize_installation_energy_response(&response)? {
                    println!("{result}");
                }
            }
            Err(error) => println!("Installation-wide feature search failed: {error:#}"),
        }
    }
    Ok(())
}

fn summarize_installation_energy_response(response: &Value) -> Result<Vec<String>> {
    let Some(data) = response.get("data") else {
        bail!("Installation feature response did not contain a data field");
    };
    let Some(features) = data.as_array() else {
        return Ok(vec![format!(
            "Installation feature search returned {}",
            serde_json::to_string(data).context("Could not serialize installation feature data")?
        )]);
    };
    let mut output = Vec::new();
    for feature in features {
        let Some(name) = feature.get("feature").and_then(Value::as_str) else {
            continue;
        };
        let summary = summarize_power_measurements(&json!({"data": [feature]}))?;
        let Some(summary) = summary.first() else {
            continue;
        };
        let gateway_id = feature
            .get("gatewayId")
            .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "?".to_owned()))
            .unwrap_or_else(|| "unknown".to_owned());
        let device_id = feature
            .get("deviceId")
            .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "?".to_owned()))
            .unwrap_or_else(|| "unknown".to_owned());
        output.push(format!(
            "Installation feature {name}: gatewayId={gateway_id}, deviceId={device_id}; {summary}"
        ));
    }
    if output.is_empty() {
        output.push(format!(
            "Installation feature search returned no matching energy features ({} feature records received).",
            features.len()
        ));
    }
    Ok(output)
}

fn summarize_probe_response(response: &Value) -> String {
    let data = response.get("data").unwrap_or(response);
    let serialized = serde_json::to_string(data)
        .unwrap_or_else(|error| format!("could not serialize response data: {error}"));
    const MAX_CHARS: usize = 1200;
    if serialized.chars().count() <= MAX_CHARS {
        serialized
    } else {
        let shortened: String = serialized.chars().take(MAX_CHARS).collect();
        format!("{shortened}… [truncated]")
    }
}

fn token_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("fi", "rust-viessman-control", "rust-viessman-control")
        .context("Could not determine a per-user config directory")?;
    fs::create_dir_all(dirs.config_dir()).context("Could not create config directory")?;
    Ok(dirs.config_dir().join("vicare.token.json"))
}

fn validate_redirect_uri(value: &str) -> Result<String> {
    let redirect = Url::parse(value).context("VIESSMANN_REDIRECT_URI must be a valid URL")?;
    if redirect.scheme() != "http"
        || redirect.host_str() != Some("localhost")
        || redirect.port().is_none()
        || redirect.path() != "/"
        || redirect.query().is_some()
        || redirect.fragment().is_some()
        || !redirect.username().is_empty()
        || redirect.password().is_some()
    {
        bail!("OAuth redirect URI must be http://localhost:<port> with no path or query");
    }
    Ok(value.to_owned())
}

async fn get_valid_token(
    http: &Client,
    client_id: &str,
    redirect: &str,
    token_path: &Path,
) -> Result<StoredToken> {
    let mut token = match read_token(token_path)? {
        Some(token) => token,
        None => authorize_in_browser(http, client_id, redirect).await?,
    };

    let now = unix_time()?;
    if token.expires_at <= now + 30 {
        let refresh_token = token
            .refresh_token
            .as_deref()
            .context("Stored ViCare token has expired and contains no refresh token")?;
        token = refresh_access_token(http, client_id, refresh_token).await?;
    }

    write_token(token_path, &token)?;
    Ok(token)
}

fn read_token(path: &Path) -> Result<Option<StoredToken>> {
    match fs::read(path) {
        Ok(bytes) => {
            let token = serde_json::from_slice(&bytes).with_context(|| {
                format!("Could not parse saved OAuth token at {}", path.display())
            })?;
            Ok(Some(token))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Could not read {}", path.display())),
    }
}

async fn authorize_in_browser(
    http: &Client,
    client_id: &str,
    redirect: &str,
) -> Result<StoredToken> {
    let port = Url::parse(redirect)?
        .port()
        .context("OAuth redirect URI must include a port")?;
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("Could not listen on localhost:{port} for OAuth callback"))?;

    let state = random_url_token(32);
    let verifier = random_url_token(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut authorization = Url::parse(AUTHORIZE_URL)?;
    authorization
        .query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect)
        .append_pair("scope", "IoT User offline_access")
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");

    println!("Viessmann sign-in URL: {}", authorization.as_str());
    if let Err(error) = webbrowser::open(authorization.as_str()) {
        eprintln!(
            "Could not open a browser here ({error}); open the sign-in URL on another device instead."
        );
    }
    println!("Complete sign-in and consent; waiting for the OAuth callback on port {port}.");

    let code = tokio::time::timeout(
        std::time::Duration::from_secs(180),
        receive_authorization_code(listener, &state),
    )
    .await
    .context("Timed out waiting for Viessmann browser authorization")??;
    let response = http
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("redirect_uri", redirect),
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await
        .context("OAuth token exchange request failed")?;
    parse_token_response(response, None).await
}

async fn receive_authorization_code(listener: TcpListener, expected_state: &str) -> Result<String> {
    let (mut stream, _) = listener.accept().await.context("OAuth callback failed")?;
    let mut request = vec![0; 8192];
    let length = stream
        .read(&mut request)
        .await
        .context("Could not read OAuth callback")?;
    let request = String::from_utf8_lossy(&request[..length]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("Invalid OAuth callback request")?;
    let callback =
        Url::parse(&format!("http://localhost{target}")).context("Invalid OAuth callback URL")?;
    let parameters: Map<String, Value> = callback
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), Value::String(value.into_owned())))
        .collect();

    let result = (|| {
        let state = parameters
            .get("state")
            .and_then(Value::as_str)
            .context("OAuth callback did not include state")?;
        if state != expected_state {
            bail!("OAuth callback state did not match");
        }
        if let Some(error) = parameters.get("error").and_then(Value::as_str) {
            bail!("Viessmann authorization failed: {error}");
        }
        parameters
            .get("code")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("OAuth callback did not include an authorization code")
    })();

    let (status, message) = match &result {
        Ok(_) => (
            "200 OK",
            "Viessmann sign-in completed. You can close this browser tab.",
        ),
        Err(_) => (
            "400 Bad Request",
            "Viessmann sign-in failed. Return to the terminal for details.",
        ),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}",
        message.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .context("Could not complete OAuth callback response")?;
    result
}

pub(crate) async fn refresh_access_token(
    http: &Client,
    client_id: &str,
    refresh_token: &str,
) -> Result<StoredToken> {
    let response = http
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .context("OAuth token refresh request failed")?;
    parse_token_response(response, Some(refresh_token.to_owned())).await
}

async fn parse_token_response(
    response: reqwest::Response,
    previous_refresh_token: Option<String>,
) -> Result<StoredToken> {
    let status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read OAuth response")?;
    if !status.is_success() {
        bail!("OAuth request returned {status}: {body}");
    }
    let response: TokenResponse =
        serde_json::from_str(&body).context("OAuth response did not contain a valid token")?;
    Ok(StoredToken {
        access_token: response.access_token,
        refresh_token: response.refresh_token.or(previous_refresh_token),
        expires_at: unix_time()?.saturating_add(response.expires_in),
    })
}

pub(crate) fn write_token(path: &Path, token: &StoredToken) -> Result<()> {
    let bytes = serde_json::to_vec(token)?;
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("Could not save OAuth token to {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .context("Could not restrict saved token file permissions")?;
    }
    use std::io::Write;
    let mut file = file;
    file.write_all(&bytes)
        .with_context(|| format!("Could not write {}", path.display()))
}

pub(crate) async fn api_get(http: &Client, access_token: &str, path: &str) -> Result<Value> {
    let response = http
        .get(format!("{API_BASE_URL}{path}"))
        .bearer_auth(access_token)
        .send()
        .await
        .with_context(|| format!("Viessmann API request failed: {path}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("Could not read Viessmann API response: {path}"))?;
    if !status.is_success() {
        bail!("Viessmann API returned {status} for {path}: {body}");
    }
    serde_json::from_str(&body)
        .with_context(|| format!("Viessmann API returned invalid JSON for {path}"))
}

async fn fetch_v2_devices(
    http: &Client,
    access_token: &str,
    installations: &Value,
) -> Result<Vec<Value>> {
    let installation_list = response_data_array(
        installations,
        "Viessmann installations response did not contain a data array",
    )?;
    let mut devices = Vec::new();

    for installation in installation_list {
        let installation_id = optional_identifier(installation, &["id", "installationId"])
            .context("Viessmann installation record has no id")?;
        let gateways_path = format!(
            "/equipment/installations/{}/gateways?includeDevices=true&limit=1000",
            path_segment(&installation_id)
        );
        let gateways_response = api_get(http, access_token, &gateways_path).await?;
        let gateways = response_data_array(
            &gateways_response,
            &format!("Gateway response did not contain a data array: {gateways_path}"),
        )?;

        for gateway in gateways {
            let gateway_serial = required_identifier(gateway, "serial")
                .with_context(|| format!("Gateway response is missing serial: {gateways_path}"))?;
            let gateway_devices =
                if let Some(gateway_devices) = gateway.get("devices").and_then(Value::as_array) {
                    gateway_devices.clone()
                } else {
                    let devices_path = format!(
                        "/equipment/installations/{}/gateways/{}/devices?limit=1000",
                        path_segment(&installation_id),
                        path_segment(&gateway_serial)
                    );
                    let response = api_get(http, access_token, &devices_path).await?;
                    response_data_array(
                        &response,
                        &format!(
                            "Gateway devices response did not contain a data array: {devices_path}"
                        ),
                    )?
                    .to_vec()
                };

            devices.extend(normalize_gateway_devices(
                &installation_id,
                &gateway_serial,
                &gateway_devices,
            )?);
        }
    }

    Ok(devices)
}

fn normalize_gateway_devices(
    installation_id: &str,
    gateway_serial: &str,
    gateway_devices: &[Value],
) -> Result<Vec<Value>> {
    gateway_devices
        .iter()
        .map(|device| {
            let mut device = device.clone();
            let object = device
                .as_object_mut()
                .context("Viessmann device record is not an object")?;
            object.insert(
                "installationId".to_owned(),
                Value::String(installation_id.to_owned()),
            );
            object.insert(
                "gatewaySerial".to_owned(),
                Value::String(gateway_serial.to_owned()),
            );
            Ok(device)
        })
        .collect()
}

fn response_data_array<'a>(response: &'a Value, error: &str) -> Result<&'a [Value]> {
    response
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .with_context(|| error.to_owned())
}

fn optional_identifier(value: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| value.get(name))
        .and_then(|identifier| match identifier {
            Value::String(identifier) => Some(identifier.clone()),
            Value::Number(identifier) if identifier.is_u64() || identifier.is_i64() => {
                Some(identifier.to_string())
            }
            _ => None,
        })
}

fn heat_pump_match_reason(model: &str, device_type: &str) -> Option<&'static str> {
    if model.to_ascii_lowercase().contains("222-s") {
        Some("exact model name")
    } else if model.eq_ignore_ascii_case("CU401B_S") && device_type == "heating" {
        Some("Vitocal S-series model alias")
    } else {
        None
    }
}

fn is_room_control(model: &str, device_type: &str) -> bool {
    device_type == "roomControl" || model.eq_ignore_ascii_case("Smart_RoomControl")
}

pub(crate) fn device_features_path(device: &Value) -> Result<String> {
    let installation_id = required_identifier(device, "installationId")?;
    let gateway_serial = required_identifier(device, "gatewaySerial")?;
    let device_id = required_identifier(device, "id")?;
    Ok(format!(
        "/features/installations/{}/gateways/{}/devices/{}/features/",
        path_segment(&installation_id),
        path_segment(&gateway_serial),
        path_segment(&device_id)
    ))
}

fn required_identifier(value: &Value, name: &str) -> Result<String> {
    let field = value
        .get(name)
        .with_context(|| format!("ViCare device record is missing the {name} field"))?;
    match field {
        Value::String(identifier) => Ok(identifier.clone()),
        Value::Number(identifier) if identifier.is_u64() || identifier.is_i64() => {
            Ok(identifier.to_string())
        }
        _ => {
            bail!("ViCare device record has an invalid {name} field; expected a string or integer")
        }
    }
}

fn path_segment(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

fn summarize_features(response: &Value) -> Result<Vec<FeatureSummary>> {
    let features = response
        .get("data")
        .and_then(Value::as_array)
        .context("Viessmann feature response did not contain a data array")?;
    Ok(features.iter().map(summarize_feature).collect())
}

fn summarize_power_measurements(response: &Value) -> Result<Vec<String>> {
    let features = response
        .get("data")
        .and_then(Value::as_array)
        .context("Viessmann feature response did not contain a data array")?;
    Ok(features
        .iter()
        .filter_map(|feature| {
            let name = feature.get("feature").and_then(Value::as_str)?;
            let lower_name = name.to_ascii_lowercase();
            if ![
                "power",
                "energy",
                "consumption",
                "production",
                "electrical",
                "compressor",
            ]
            .iter()
            .any(|term| lower_name.contains(term))
            {
                return None;
            }
            let properties = feature.get("properties").and_then(Value::as_object)?;
            let readings = properties
                .iter()
                .filter_map(|(property_name, property)| {
                    let reading = property.get("value")?;
                    let value = reading.get("value").unwrap_or(reading);
                    let value = match value {
                        Value::String(value) => value.clone(),
                        Value::Number(value) => value.to_string(),
                        Value::Bool(value) => value.to_string(),
                        Value::Array(values) => {
                            let sample = values
                                .iter()
                                .take(3)
                                .map(|value| {
                                    serde_json::to_string(value).unwrap_or_else(|_| "?".to_owned())
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            format!("{} values [{sample}]", values.len())
                        }
                        _ => return None,
                    };
                    let unit = reading
                        .get("unit")
                        .and_then(Value::as_str)
                        .or_else(|| property.get("unit").and_then(Value::as_str))
                        .map(|unit| format!(" {unit}"))
                        .unwrap_or_default();
                    Some(format!("{property_name}={value}{unit}"))
                })
                .collect::<Vec<_>>();
            let enabled = feature
                .get("isEnabled")
                .and_then(Value::as_bool)
                .map(|enabled| format!("enabled={enabled}; "))
                .unwrap_or_default();
            Some(format!(
                "{name}: {enabled}{}",
                if readings.is_empty() {
                    "no scalar readings".to_owned()
                } else {
                    readings.join(", ")
                }
            ))
        })
        .collect())
}

fn summarize_room_telemetry(response: &Value) -> Result<Vec<(String, RoomTelemetry)>> {
    let features = response
        .get("data")
        .and_then(Value::as_array)
        .context("Viessmann RoomControl response did not contain a data array")?;
    let mut rooms = BTreeMap::<String, RoomTelemetry>::new();

    for feature in features {
        let Some(name) = feature.get("feature").and_then(Value::as_str) else {
            continue;
        };
        if let Some(room) = name.strip_suffix(".sensors.temperature") {
            if feature.get("isEnabled").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let telemetry = rooms.entry(room.to_owned()).or_default();
            telemetry.temperature = nested_number(feature, &["properties", "value", "value"]);
            telemetry.sensor_status = nested_string(feature, &["properties", "status", "value"]);
            continue;
        }

        let setpoint = [
            (".temperature.levels.normal.heating", Setpoint::Normal),
            (".temperature.levels.reduced.heating", Setpoint::Reduced),
            (".temperature.levels.comfort.heating", Setpoint::Comfort),
        ]
        .into_iter()
        .find_map(|(suffix, kind)| name.strip_suffix(suffix).map(|room| (room, kind)));

        if let Some((room, kind)) = setpoint {
            if feature.get("isEnabled").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let value = nested_number(feature, &["properties", "temperature", "value"]);
            let telemetry = rooms.entry(room.to_owned()).or_default();
            match kind {
                Setpoint::Normal => telemetry.normal_heating = value,
                Setpoint::Reduced => telemetry.reduced_heating = value,
                Setpoint::Comfort => telemetry.comfort_heating = value,
            }
        }
    }

    Ok(rooms.into_iter().collect())
}

fn summarize_heating_references(response: &Value) -> Result<Vec<String>> {
    let features = response
        .get("data")
        .and_then(Value::as_array)
        .context("Viessmann heating response did not contain a data array")?;
    let mut references = Vec::new();

    for feature in features {
        let Some(name) = feature.get("feature").and_then(Value::as_str) else {
            continue;
        };
        if feature.get("isEnabled").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let reference = match name {
            "heating.circuits.0.operating.programs.normal"
            | "heating.circuits.0.operating.programs.reduced"
            | "heating.circuits.0.operating.programs.comfort" => {
                let temperature = format_optional_temperature(nested_number(
                    feature,
                    &["properties", "temperature", "value"],
                ));
                Some(format!("{name}: temperature={temperature}"))
            }
            "heating.circuits.0.heating.curve" => {
                let shift = format_optional_number(nested_number(
                    feature,
                    &["properties", "shift", "value"],
                ));
                let slope = format_optional_number(nested_number(
                    feature,
                    &["properties", "slope", "value"],
                ));
                Some(format!("{name}: shift={shift}, slope={slope}"))
            }
            "heating.circuits.0.temperature.levels" => {
                let minimum = format_optional_temperature(nested_number(
                    feature,
                    &["properties", "min", "value"],
                ));
                let maximum = format_optional_temperature(nested_number(
                    feature,
                    &["properties", "max", "value"],
                ));
                Some(format!("{name}: min={minimum}, max={maximum}"))
            }
            _ => None,
        };
        if let Some(reference) = reference {
            references.push(reference);
        }
    }

    Ok(references)
}

#[derive(Clone, Copy)]
enum Setpoint {
    Normal,
    Reduced,
    Comfort,
}

fn nested_number(value: &Value, path: &[&str]) -> Option<f64> {
    path.iter()
        .try_fold(value, |current, part| current.get(*part))?
        .as_f64()
}

fn nested_string(value: &Value, path: &[&str]) -> Option<String> {
    path.iter()
        .try_fold(value, |current, part| current.get(*part))?
        .as_str()
        .map(str::to_owned)
}

fn format_optional_temperature(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.1} C"))
        .unwrap_or_else(|| "unavailable".to_owned())
}

fn format_optional_number(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.2}"))
        .unwrap_or_else(|| "unavailable".to_owned())
}

fn summarize_feature(feature: &Value) -> FeatureSummary {
    let properties = feature
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default();

    FeatureSummary {
        feature: feature
            .get("feature")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        enabled: feature.get("isEnabled").and_then(Value::as_bool),
        properties,
        commands: summarize_commands(feature.get("commands")),
    }
}

fn summarize_commands(commands: Option<&Value>) -> Vec<CommandSummary> {
    let Some(commands) = commands else {
        return Vec::new();
    };
    let entries: Vec<(&str, &Value)> = match commands {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                item.get("name")
                    .and_then(Value::as_str)
                    .map(|name| (name, item))
            })
            .collect(),
        Value::Object(items) => items
            .iter()
            .map(|(name, item)| (name.as_str(), item))
            .collect(),
        _ => Vec::new(),
    };

    entries
        .into_iter()
        .map(|(key, command)| {
            let name = command
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(key)
                .to_owned();
            let parameters = command
                .get("params")
                .map(parameter_names)
                .unwrap_or_default();
            let allowed_values = command
                .pointer("/params/mode/constraints/enum")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            CommandSummary {
                name,
                executable: command.get("isExecutable").and_then(Value::as_bool),
                parameters,
                allowed_values,
            }
        })
        .collect()
}

fn parameter_names(parameters: &Value) -> Vec<String> {
    match parameters {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect(),
        Value::Object(items) => items.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

fn random_url_token(length: usize) -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(length)
        .map(char::from)
        .collect()
}

fn unix_time() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock is before Unix epoch")
        .map(|duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identifies_exact_model_and_s_series_api_alias() {
        assert_eq!(
            heat_pump_match_reason("Vitocal 222-S", "heating"),
            Some("exact model name")
        );
        assert_eq!(
            heat_pump_match_reason("CU401B_S", "heating"),
            Some("Vitocal S-series model alias")
        );
        assert_eq!(
            heat_pump_match_reason("cu401b_s", "heating"),
            Some("Vitocal S-series model alias")
        );
        assert_eq!(heat_pump_match_reason("CU401B_S", "zigbee"), None);
        assert_eq!(heat_pump_match_reason("Vitocal 222-A", "heating"), None);
    }

    #[test]
    fn identifies_room_control_devices() {
        assert!(is_room_control("Smart_RoomControl", "roomControl"));
        assert!(is_room_control("other-model", "roomControl"));
        assert!(!is_room_control("CU401B_S", "heating"));
    }

    #[test]
    fn builds_device_feature_path_from_installation_and_gateway_ids() {
        let device = json!({
            "installationId": 12345,
            "gatewaySerial": "gateway-serial",
            "id": "RoomControl-1"
        });
        assert_eq!(
            device_features_path(&device).unwrap(),
            "/features/installations/12345/gateways/gateway-serial/devices/RoomControl-1/features/"
        );
    }

    #[test]
    fn summarizes_supported_features_without_exposing_current_values() {
        let feature = json!({
            "feature": "heating.dhw.temperature",
            "isEnabled": true,
            "properties": {
                "value": {"value": 48.0},
                "unit": {"value": "celsius"}
            },
            "commands": {
                "setTargetTemperature": {
                    "isExecutable": true,
                    "params": {
                        "targetTemperature": {"type": "number"}
                    }
                }
            }
        });

        let summary = summarize_feature(&feature);
        let serialized = serde_json::to_string(&summary).unwrap();

        assert_eq!(summary.feature, "heating.dhw.temperature");
        assert_eq!(summary.properties, vec!["unit", "value"]);
        assert_eq!(summary.commands[0].name, "setTargetTemperature");
        assert_eq!(summary.commands[0].parameters, vec!["targetTemperature"]);
        assert!(summary.commands[0].allowed_values.is_empty());
        assert!(!serialized.contains("48.0"));
        assert!(!serialized.contains("celsius"));
    }

    #[test]
    fn exposes_allowed_heating_modes_in_read_only_feature_summary() {
        let feature = json!({
            "feature": "heating.circuits.0.operating.modes.active",
            "commands": {
                "setMode": {
                    "isExecutable": true,
                    "params": {
                        "mode": {
                            "type": "string",
                            "constraints": {"enum": ["normal", "forcedNormal"]}
                        }
                    }
                }
            }
        });
        let summary = summarize_feature(&feature);
        assert_eq!(
            summary.commands[0].allowed_values,
            vec!["normal", "forcedNormal"]
        );
    }

    #[test]
    fn reports_live_power_and_energy_feature_values_with_units() {
        let response = json!({
            "data": [
                {
                    "feature": "heating.compressors.0.sensors.power",
                    "isEnabled": true,
                    "properties": {
                        "value": {"value": 73.0, "unit": "percent"},
                        "status": {"value": "connected"}
                    }
                },
                {
                    "feature": "heating.energy.consumption",
                    "isEnabled": true,
                    "properties": {
                        "value": {"value": 12.5, "unit": "kilowattHour"}
                    }
                },
                {
                    "feature": "heating.circuits.0.sensors.temperature",
                    "isEnabled": true,
                    "properties": {
                        "value": {"value": 21.0, "unit": "celsius"}
                    }
                }
            ]
        });

        let readings = summarize_power_measurements(&response).unwrap();
        assert_eq!(readings.len(), 2);
        assert!(readings[0].contains("value=73.0 percent"));
        assert!(readings[0].contains("status=connected"));
        assert!(readings[1].contains("value=12.5 kilowattHour"));
        assert!(
            readings
                .iter()
                .all(|reading| !reading.contains("temperature"))
        );
    }

    #[test]
    fn reports_energy_history_arrays_and_heat_production_candidates() {
        let response = json!({
            "data": [
                {
                    "feature": "heating.power.consumption.heating",
                    "isEnabled": true,
                    "properties": {
                        "day": {
                            "value": [1.2, 1.4, 1.1],
                            "unit": "kilowattHour"
                        }
                    }
                },
                {
                    "feature": "heating.power.production",
                    "isEnabled": true,
                    "properties": {
                        "year": {
                            "value": [1200, 1300],
                            "unit": "kilowattHour"
                        }
                    }
                }
            ]
        });
        let readings = summarize_power_measurements(&response).unwrap();
        assert_eq!(readings.len(), 2);
        assert!(readings[0].contains("day=3 values"));
        assert!(readings[0].contains("kilowattHour"));
        assert!(readings[1].contains("heating.power.production"));
        assert!(readings[1].contains("year=2 values"));
    }

    #[test]
    fn includes_heat_pump_electrical_and_heat_output_probe_paths() {
        for feature in [
            "device.power.statusReport.consumption",
            "device.power.statusReport.production",
            "heating.power.consumption.current",
            "heating.power.consumption.total",
            "heating.power.production",
            "heating.compressors.0.heat.production.current",
            "heating.compressors.0.power.consumption.dhw",
            "heating.compressors.0.power.consumption.heating",
            "heating.heat.production.dhw",
            "heating.heat.production.heating",
            "heating.heat.production.summary.dhw",
            "heating.heat.production.summary.heating",
            "heating.heat.production.currentYear.dhw",
            "heating.heat.production.currentYear.heating",
            "heating.heat.production.total",
            "heating.heat.production.current",
            "heating.power.consumption.total",
            "heating.power.consumption.current",
            "heating.heatingRod.power.consumption.total",
            "heating.heatingRod.power.consumption.summary.dhw",
            "heating.heatingRod.power.consumption.summary.heating",
            "heating.heatingRod.heat.production.current",
            "heating.solar.power.production",
            "heating.heatingRod.power.consumption.current",
            "heating.heatingRod.heat.production.current",
        ] {
            assert!(POWER_ENERGY_FEATURE_CANDIDATES.contains(&feature));
        }
    }

    #[test]
    fn summarizes_installation_wide_energy_features_with_device_ids() {
        let response = json!({
            "data": [{
                "feature": "heating.heat.production.dhw",
                "isEnabled": true,
                "gatewayId": {"gatewaySerial": "serial-1"},
                "deviceId": "0",
                "properties": {
                    "day": {"value": [1.2, 1.4], "unit": "kilowattHour"}
                }
            }]
        });
        let summary = summarize_installation_energy_response(&response).unwrap();
        assert_eq!(summary.len(), 1);
        assert!(summary[0].contains("heating.heat.production.dhw"));
        assert!(summary[0].contains("deviceId=\"0\""));
        assert!(summary[0].contains("kilowattHour"));
    }

    #[test]
    fn installation_energy_search_reports_empty_or_null_data() {
        assert!(
            summarize_installation_energy_response(&json!({"data": []})).unwrap()[0]
                .contains("0 feature records")
        );
        assert!(
            summarize_installation_energy_response(&json!({"data": null})).unwrap()[0]
                .contains("null")
        );
        assert!(summarize_installation_energy_response(&json!({})).is_err());
    }

    #[test]
    fn truncates_large_read_only_probe_responses() {
        let response = json!({"data": "x".repeat(1400)});
        let summary = summarize_probe_response(&response);
        assert!(summary.ends_with("[truncated]"));
        assert!(summary.chars().count() < 1250);
    }

    #[test]
    fn extracts_enabled_room_temperature_and_heating_setpoints() {
        let response = json!({
            "data": [
                {
                    "feature": "rooms.0.sensors.temperature",
                    "isEnabled": true,
                    "properties": {
                        "status": {"value": "connected"},
                        "value": {"value": 21.5}
                    }
                },
                {
                    "feature": "rooms.0.temperature.levels.normal.heating",
                    "isEnabled": true,
                    "properties": {"temperature": {"value": 20}}
                },
                {
                    "feature": "rooms.0.temperature.levels.reduced.heating",
                    "isEnabled": true,
                    "properties": {"temperature": {"value": 18}}
                },
                {
                    "feature": "rooms.0.temperature.levels.comfort.heating",
                    "isEnabled": true,
                    "properties": {"temperature": {"value": 22}}
                },
                {
                    "feature": "rooms.1.sensors.temperature",
                    "isEnabled": false,
                    "properties": {
                        "status": {"value": "disconnected"},
                        "value": {"value": 15.0}
                    }
                }
            ]
        });

        let rooms = summarize_room_telemetry(&response).unwrap();
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0].0, "rooms.0");
        assert_eq!(rooms[0].1.temperature, Some(21.5));
        assert_eq!(rooms[0].1.sensor_status.as_deref(), Some("connected"));
        assert_eq!(rooms[0].1.normal_heating, Some(20.0));
        assert_eq!(rooms[0].1.reduced_heating, Some(18.0));
        assert_eq!(rooms[0].1.comfort_heating, Some(22.0));
    }

    #[test]
    fn extracts_live_heating_circuit_setpoints_and_curve_baseline() {
        let response = json!({
            "data": [
                {
                    "feature": "heating.circuits.0.operating.programs.normal",
                    "isEnabled": true,
                    "properties": {"temperature": {"value": 20.0}}
                },
                {
                    "feature": "heating.circuits.0.operating.programs.reduced",
                    "isEnabled": true,
                    "properties": {"temperature": {"value": 18.0}}
                },
                {
                    "feature": "heating.circuits.0.heating.curve",
                    "isEnabled": true,
                    "properties": {
                        "shift": {"value": 2.0},
                        "slope": {"value": 0.8}
                    }
                },
                {
                    "feature": "heating.circuits.1.operating.programs.normal",
                    "isEnabled": false,
                    "properties": {"temperature": {"value": 99.0}}
                }
            ]
        });

        let references = summarize_heating_references(&response).unwrap();
        assert_eq!(references.len(), 3);
        assert!(
            references
                .iter()
                .any(|line| line.ends_with("temperature=20.0 C"))
        );
        assert!(
            references
                .iter()
                .any(|line| line.ends_with("temperature=18.0 C"))
        );
        assert!(
            references
                .iter()
                .any(|line| line.ends_with("shift=2.00, slope=0.80"))
        );
        assert!(!references.iter().any(|line| line.contains("99.0")));
    }

    #[test]
    fn validates_only_localhost_oauth_redirects() {
        assert_eq!(
            validate_redirect_uri("http://localhost:51125").unwrap(),
            "http://localhost:51125"
        );
        assert_eq!(
            validate_redirect_uri("http://localhost:51125/").unwrap(),
            "http://localhost:51125/"
        );
        assert!(validate_redirect_uri("https://localhost:51125").is_err());
        assert!(validate_redirect_uri("http://example.com:51125").is_err());
        assert!(validate_redirect_uri("http://user@localhost:51125").is_err());
    }

    #[test]
    fn fails_when_installation_response_has_no_data_array() {
        assert!(response_data_array(&json!({}), "missing data").is_err());
        assert!(
            response_data_array(&json!({"data": []}), "missing data")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn accepts_numeric_installation_ids_and_string_device_ids() {
        let installation = json!({"id": 12345});
        let gateway = json!({"serial": "gateway-serial"});
        let installation_id =
            optional_identifier(&installation, &["id", "installationId"]).unwrap();
        let gateway_serial = required_identifier(&gateway, "serial").unwrap();
        let devices = normalize_gateway_devices(
            &installation_id,
            &gateway_serial,
            &[json!({"id": "0", "modelId": "CU401B_S"})],
        )
        .unwrap();
        let device = &devices[0];

        assert_eq!(
            required_identifier(device, "installationId").unwrap(),
            installation_id
        );
        assert_eq!(
            required_identifier(device, "gatewaySerial").unwrap(),
            gateway_serial
        );
        assert_eq!(required_identifier(device, "id").unwrap(), "0");
        assert!(required_identifier(device, "missing").is_err());
        assert!(required_identifier(&json!({"id": 1.5}), "id").is_err());
    }

    #[test]
    fn encodes_path_segments_without_form_url_plus_spaces() {
        assert_eq!(path_segment("gateway / 1"), "gateway%20%2F%201");
    }
}
