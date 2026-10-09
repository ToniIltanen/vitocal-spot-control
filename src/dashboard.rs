use crate::{
    StoredToken, api_get, api_post, device_features_path, refresh_access_token, write_token,
};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Months, NaiveDate, TimeZone, Utc};
use chrono_tz::Europe::Helsinki;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex, Notify},
    time::{MissedTickBehavior, interval},
};

const SPOT_HINTA_URL: &str = "https://api.spot-hinta.fi/TodayAndDayForward";
const POWER_FEATURE: &str = "heating.compressors.0.sensors.power";
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5 * 60);
const MAX_SAMPLE_GAP_SECONDS: i64 = 10 * 60;
const QUARTER_SECONDS: i64 = 15 * 60;
const HEATING_CURVE_FEATURE: &str = "heating.circuits.0.heating.curve";
const HEATING_MODE_FEATURE: &str = "heating.circuits.0.operating.modes.active";
const ROOM_TEMPERATURE_FEATURE: &str = "rooms.0.sensors.temperature";
const MAX_ROOM_TEMPERATURE_AGE_SECONDS: i64 = 60 * 60;
const COMFORT_MIN_C: f64 = 20.0;
const COMFORT_MAX_C: f64 = 22.0;
const FORCED_HEATING_MIN_C: f64 = 19.0;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ControlMode {
    Light,
    #[default]
    Normal,
    Strong,
}

impl ControlMode {
    fn curve_delta(self) -> f64 {
        match self {
            Self::Light => 0.5,
            Self::Normal => 1.0,
            Self::Strong => 2.0,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Light => "Kevyt",
            Self::Normal => "Normaali",
            Self::Strong => "Jyrkkä",
        }
    }
}

#[derive(Debug, Default, Clone, Serialize)]
struct UiControlState {
    requested_active: bool,
    active: bool,
    mode: ControlMode,
    status: String,
    emergency_heating: bool,
}

#[derive(Debug, Default, Serialize)]
struct DashboardState {
    points: Vec<ChartPoint>,
    updated_at: i64,
    control: UiControlState,
    compressor_reading: Option<SensorReading>,
    compressor_reading_error: Option<String>,
    room_temperature: Option<TemperatureSample>,
    room_temperature_error: Option<String>,
    #[serde(skip)]
    history: History,
}

#[derive(Debug, Clone)]
struct ActiveControl {
    baseline_shift: f64,
    slope: f64,
    minimum_shift: f64,
    maximum_shift: f64,
    baseline_mode: String,
    forced_normal_mode: Option<String>,
    emergency_heating: bool,
    applied_shift: f64,
    applied_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PowerSample {
    timestamp: i64,
    power_kw: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SensorReading {
    timestamp: i64,
    value: f64,
    unit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct TemperatureSample {
    timestamp: i64,
    temperature_c: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ControlValueSample {
    timestamp: i64,
    curve_shift: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PricePoint {
    timestamp: i64,
    price_eur_per_kwh: f64,
}

#[derive(Clone, Copy)]
struct PriceWindow<'a> {
    points: &'a [PricePoint],
    now: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PricePosition {
    Cheap,
    Middle,
    Expensive,
}

fn price_position(prices: &[PricePoint], now: i64) -> Option<PricePosition> {
    let current_slot = now.div_euclid(QUARTER_SECONDS) * QUARTER_SECONDS;
    let current = prices
        .iter()
        .find(|price| price.timestamp == current_slot)?;
    let mut day_ahead: Vec<f64> = prices
        .iter()
        .filter(|price| {
            price.timestamp >= current_slot
                && price.timestamp < current_slot + 24 * 60 * 60
                && price.price_eur_per_kwh.is_finite()
        })
        .map(|price| price.price_eur_per_kwh)
        .collect();
    if day_ahead.len() < 8 {
        return None;
    }
    day_ahead.sort_by(f64::total_cmp);
    let low_cut = day_ahead[day_ahead.len() / 4];
    let high_cut = day_ahead[day_ahead.len() * 3 / 4];
    if low_cut >= high_cut {
        return Some(PricePosition::Middle);
    }
    if current.price_eur_per_kwh <= low_cut {
        Some(PricePosition::Cheap)
    } else if current.price_eur_per_kwh >= high_cut {
        Some(PricePosition::Expensive)
    } else {
        Some(PricePosition::Middle)
    }
}

fn target_curve_shift(
    mode: ControlMode,
    position: PricePosition,
    temperature: f64,
    baseline: f64,
    minimum: f64,
    maximum: f64,
) -> f64 {
    let delta = match position {
        PricePosition::Cheap => mode.curve_delta(),
        PricePosition::Expensive => -mode.curve_delta(),
        PricePosition::Middle => 0.0,
    };
    let bounded_delta = if (temperature >= COMFORT_MAX_C && delta > 0.0)
        || (temperature <= COMFORT_MIN_C && delta < 0.0)
    {
        0.0
    } else {
        delta
    };
    (baseline + bounded_delta).clamp(minimum, maximum)
}

fn emergency_heating_active(current: bool, temperature: Option<f64>) -> bool {
    match temperature {
        Some(value) if value < FORCED_HEATING_MIN_C => true,
        Some(value) if current && value >= COMFORT_MIN_C => false,
        _ => current,
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct History {
    power_samples: Vec<PowerSample>,
    prices: Vec<PricePoint>,
    #[serde(default)]
    compressor_readings: Vec<SensorReading>,
    #[serde(default)]
    room_temperatures: Vec<TemperatureSample>,
    #[serde(default)]
    control_values: Vec<ControlValueSample>,
}

#[derive(Debug, Serialize)]
struct ChartPoint {
    timestamp: i64,
    energy_kwh: Option<f64>,
    price_cents_per_kwh: Option<f64>,
    room_temperature_c: Option<f64>,
    curve_shift: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct SpotPrice {
    #[serde(rename = "DateTime")]
    date_time: String,
    #[serde(rename = "PriceWithTax")]
    price_with_tax: f64,
}

pub(crate) async fn run_monitor(
    http: &Client,
    token: &mut StoredToken,
    client_id: &str,
    token_file: &Path,
    heat_pump: &Value,
    room_control: &Value,
) -> Result<()> {
    let dirs =
        directories::ProjectDirs::from("fi", "rust-viessman-control", "rust-viessman-control")
            .context("Could not determine a per-user data directory")?;
    let project_dir = env::current_dir().context("Could not determine project directory")?;
    let history_path = project_dir.join("power-price-history.json");
    let previous_history_path = dirs.data_dir().join("power-price-history.json");
    migrate_history_if_missing(&previous_history_path, &history_path)?;
    println!("Measurement history: {}", history_path.display());
    let dashboard_path = env::var_os("DASHBOARD_HTML")
        .map(PathBuf::from)
        .unwrap_or(env::current_dir()?.join("dashboard.html"));
    let mut history = read_history(&history_path)?;
    let feature_prefix = device_features_path(heat_pump)?;
    let feature_path = format!("{}/{POWER_FEATURE}", feature_prefix.trim_end_matches('/'));
    let mut sample_interval = interval(SAMPLE_INTERVAL);
    sample_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_price_fetch = 0_i64;
    let shared = Arc::new(Mutex::new(DashboardState {
        history: history.clone(),
        room_temperature: history.room_temperatures.last().cloned(),
        control: UiControlState {
            status: "Ohjaus pysäytetty".to_owned(),
            ..UiControlState::default()
        },
        ..DashboardState::default()
    }));
    let control_notify = Arc::new(Notify::new());
    let port = env::var("DASHBOARD_PORT")
        .ok()
        .map(|value| value.parse::<u16>())
        .transpose()
        .context("DASHBOARD_PORT must be a valid port number")?
        .unwrap_or(8787);
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("Could not bind dashboard on all interfaces at port {port}"))?;
    let dashboard_url = format!("http://<raspberry-pi-ip>:{port}/");
    let server_shared = Arc::clone(&shared);
    let server_notify = Arc::clone(&control_notify);
    let server_path = dashboard_path.clone();
    tokio::spawn(async move {
        if let Err(error) =
            serve_dashboard(listener, server_path, server_shared, server_notify).await
        {
            eprintln!("Dashboard server stopped: {error:#}");
        }
    });
    let mut controller: Option<ActiveControl> = None;

    println!("Monitoring and local control service listening on 0.0.0.0:{port}");
    println!(
        "Open {dashboard_url} from a phone on your home network (replace the address with the Raspberry Pi's IP)."
    );
    println!("Warning: the dashboard has no password; do not expose its port to the internet.");
    println!("Dashboard: {}", dashboard_path.display());
    println!(
        "Consumption graph uses the compressor sensor only when ViCare reports watts or kilowatts; percentage readings are not converted to kWh."
    );
    println!("Price control remains off until started from the dashboard.");
    println!("Stop the service with Ctrl+C.");

    loop {
        let sample_due = tokio::select! {
            _ = sample_interval.tick() => true,
            _ = control_notify.notified() => false,
            signal = tokio::signal::ctrl_c() => {
                signal.context("Could not listen for Ctrl+C")?;
                if let Some(active) = controller.as_ref() {
                    refresh_token_if_needed(http, token, client_id, token_file).await?;
                    stop_control(http, &token.access_token, heat_pump, active).await
                        .context("Could not restore the original heating settings on shutdown")?;
                }
                println!("Monitor stopped; original heating settings restored.");
                break Ok(());
            }
        };
        refresh_token_if_needed(http, token, client_id, token_file).await?;
        let now = unix_time()?;
        let mut sampled_room_temperature = None;

        if sample_due {
            match fetch_compressor_reading(http, &token.access_token, &feature_path, now).await {
                Ok(reading) => {
                    if history
                        .compressor_readings
                        .last()
                        .is_none_or(|previous| previous.timestamp < reading.timestamp)
                    {
                        if let Some(sample) = power_sample_from_reading(&reading) {
                            println!(
                                "Sampled compressor electrical power: {:.2} kW",
                                sample.power_kw
                            );
                            history.power_samples.push(sample);
                        } else {
                            println!(
                                "Sampled compressor load: {:.1} {} (not electrical power; kWh graph unavailable)",
                                reading.value, reading.unit
                            );
                        }
                        history.compressor_readings.push(reading.clone());
                        let mut state = shared.lock().await;
                        state.compressor_reading = Some(reading);
                        state.compressor_reading_error = None;
                    } else {
                        eprintln!(
                            "Skipped duplicate or out-of-order Viessmann compressor reading."
                        );
                    }
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    let mut state = shared.lock().await;
                    state.compressor_reading = None;
                    state.compressor_reading_error = Some(message.clone());
                    eprintln!("Viessmann compressor reading failed: {message}");
                }
            }

            let room_path = format!(
                "{}/{}",
                device_features_path(room_control)?.trim_end_matches('/'),
                ROOM_TEMPERATURE_FEATURE
            );
            match fetch_room_temperature(http, &token.access_token, &room_path).await {
                Ok(Some(sample)) => {
                    if history
                        .room_temperatures
                        .last()
                        .is_none_or(|previous| previous.timestamp < sample.timestamp)
                    {
                        history.room_temperatures.push(sample.clone());
                    }
                    sampled_room_temperature = Some(sample.clone());
                    let mut state = shared.lock().await;
                    state.room_temperature = Some(sample);
                    state.room_temperature_error = None;
                }
                Ok(None) => {
                    let mut state = shared.lock().await;
                    state.room_temperature = None;
                    state.room_temperature_error =
                        Some("Huoneanturin lukema ei ole käytettävissä.".to_owned());
                    eprintln!("Room-temperature sample unavailable; no history sample recorded.");
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    let mut state = shared.lock().await;
                    state.room_temperature = None;
                    state.room_temperature_error = Some(message.clone());
                    eprintln!("Room-temperature sample failed: {message}");
                }
            }

            if last_price_fetch == 0 || now - last_price_fetch >= QUARTER_SECONDS {
                match fetch_spot_prices(http).await {
                    Ok(prices) => {
                        merge_prices(&mut history.prices, prices);
                        last_price_fetch = now;
                    }
                    Err(error) => eprintln!("Spot-hinta.fi price refresh failed: {error:#}"),
                }
            }
        }

        #[derive(Debug)]
        struct ControlUpdate {
            emergency_heating: bool,
            status: String,
        }

        async fn begin_control(
            http: &Client,
            access_token: &str,
            heat_pump: &Value,
            room_control: &Value,
            mode: ControlMode,
        ) -> Result<ActiveControl> {
            let heat_path = device_features_path(heat_pump)?;
            let features = api_get(http, access_token, &heat_path).await?;
            let curve = find_feature(&features, HEATING_CURVE_FEATURE)?;
            let active_mode = find_feature(&features, HEATING_MODE_FEATURE)?;
            let curve_shift = property_number(curve, "shift")?;
            let slope = property_number(curve, "slope")?;
            let baseline_mode = active_mode
                .pointer("/properties/value/value")
                .and_then(Value::as_str)
                .context(
                    "Could not read current heating operating mode; refusing to start control",
                )?
                .to_owned();
            let constraints = curve
                .pointer("/commands/setCurve/params/shift/constraints")
                .context(
                    "ViCare did not expose setCurve shift limits; refusing to start control",
                )?;
            let minimum_shift = constraints
                .get("min")
                .and_then(Value::as_f64)
                .context("ViCare did not expose minimum curve shift")?;
            let maximum_shift = constraints
                .get("max")
                .and_then(Value::as_f64)
                .context("ViCare did not expose maximum curve shift")?;
            if minimum_shift >= maximum_shift
                || !(minimum_shift..=maximum_shift).contains(&curve_shift)
            {
                bail!("ViCare returned invalid heating-curve shift limits");
            }
            let forced_normal_mode = active_mode
                .pointer("/commands/setMode/params/mode/constraints/enum")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .find(|mode| mode.eq_ignore_ascii_case("forcedNormal"))
                .map(str::to_owned);
            let room_path = format!(
                "{}/{}",
                device_features_path(room_control)?.trim_end_matches('/'),
                ROOM_TEMPERATURE_FEATURE
            );
            let room_temperature = fetch_room_temperature(http, access_token, &room_path).await?;
            if room_temperature.is_none() {
                bail!(
                    "The active room sensor is unavailable; refusing to start automatic heating control"
                );
            }

            println!(
                "Enabling {} price control from curve shift {curve_shift:.2}, slope {slope:.2}, mode {baseline_mode}",
                mode.label()
            );
            Ok(ActiveControl {
                baseline_shift: curve_shift,
                slope,
                minimum_shift,
                maximum_shift,
                baseline_mode: baseline_mode.clone(),
                forced_normal_mode,
                emergency_heating: false,
                applied_shift: curve_shift,
                applied_mode: Some(baseline_mode),
            })
        }

        fn find_feature<'a>(response: &'a Value, name: &str) -> Result<&'a Value> {
            response
                .get("data")
                .and_then(Value::as_array)
                .context("Viessmann features response did not contain a data array")?
                .iter()
                .find(|feature| feature.get("feature").and_then(Value::as_str) == Some(name))
                .with_context(|| format!("ViCare did not return required feature {name}"))
        }

        fn property_number(feature: &Value, property: &str) -> Result<f64> {
            feature
                .pointer(&format!("/properties/{property}/value"))
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite())
                .with_context(|| format!("ViCare feature has no numeric {property} property"))
        }

        async fn apply_price_control(
            http: &Client,
            access_token: &str,
            device_path: &str,
            active: &mut ActiveControl,
            mode: ControlMode,
            room_temperature: Option<f64>,
            price_window: PriceWindow<'_>,
        ) -> Result<ControlUpdate> {
            active.emergency_heating = active.forced_normal_mode.is_some()
                && emergency_heating_active(active.emergency_heating, room_temperature);

            if active.emergency_heating {
                let forced_normal_mode = active
                    .forced_normal_mode
                    .as_deref()
                    .context("Forced-heating override state has no allowed ViCare mode")?;
                set_operating_mode(
                    http,
                    access_token,
                    device_path,
                    forced_normal_mode,
                    &mut active.applied_mode,
                )
                .await?;
                set_curve(
                    http,
                    access_token,
                    device_path,
                    active,
                    active.baseline_shift,
                )
                .await?;
                let status = room_temperature
                    .map(|temperature| {
                        format!(
                            "PAKOTETTU LÄMMITYS: huone {temperature:.1} °C, jatkuu vähintään {:.0} °C:een",
                            COMFORT_MIN_C
                        )
                    })
                    .unwrap_or_else(|| "PAKOTETTU LÄMMITYS: huoneanturin lukema puuttuu".to_owned());
                return Ok(ControlUpdate {
                    emergency_heating: true,
                    status,
                });
            }

            if active
                .forced_normal_mode
                .as_deref()
                .is_some_and(|mode| active.applied_mode.as_deref() == Some(mode))
            {
                set_operating_mode(
                    http,
                    access_token,
                    device_path,
                    &active.baseline_mode,
                    &mut active.applied_mode,
                )
                .await?;
            }

            let Some(temperature) = room_temperature else {
                set_curve(
                    http,
                    access_token,
                    device_path,
                    active,
                    active.baseline_shift,
                )
                .await?;
                return Ok(ControlUpdate {
                    emergency_heating: false,
                    status: "Huoneanturin lukema puuttuu: hinnan mukainen säätö tauolla, perussiirto palautettu"
                        .to_owned(),
                });
            };

            let Some(price_position) = price_position(price_window.points, price_window.now) else {
                set_curve(
                    http,
                    access_token,
                    device_path,
                    active,
                    active.baseline_shift,
                )
                .await?;
                return Ok(ControlUpdate {
                    emergency_heating: false,
                    status:
                        "Nykyistä Spot-hinta.fi-hintaa ei ole saatavilla; säätö odottaa hintatietoa"
                            .to_owned(),
                });
            };
            let target_shift = target_curve_shift(
                mode,
                price_position,
                temperature,
                active.baseline_shift,
                active.minimum_shift,
                active.maximum_shift,
            );
            let bounded_delta = target_shift - active.baseline_shift;
            set_curve(http, access_token, device_path, active, target_shift).await?;

            let price = price_window
                .points
                .iter()
                .find(|price| {
                    price.timestamp
                        == price_window.now.div_euclid(QUARTER_SECONDS) * QUARTER_SECONDS
                })
                .map(|price| price.price_eur_per_kwh * 100.0)
                .context("Current Spot-hinta.fi quarter price disappeared")?;
            let action = match price_position {
                PricePosition::Cheap if bounded_delta > 0.0 => {
                    format!("halpa vartti: käyrän siirto +{bounded_delta:.1}")
                }
                PricePosition::Expensive if bounded_delta < 0.0 => {
                    format!("kallis vartti: käyrän siirto {bounded_delta:.1}")
                }
                PricePosition::Cheap | PricePosition::Expensive => {
                    "huonelämpötilaraja estää hinnan mukaisen muutoksen".to_owned()
                }
                PricePosition::Middle => "hintaluokka keskitaso: perussiirto".to_owned(),
            };
            Ok(ControlUpdate {
                emergency_heating: false,
                status: format!(
                    "{} ohjaus; huone {temperature:.1} °C; hinta {price:.3} c/kWh; {action}; käyrän siirto {target_shift:.2}",
                    mode.label(),
                ),
            })
        }

        async fn set_curve(
            http: &Client,
            access_token: &str,
            device_path: &str,
            active: &mut ActiveControl,
            target_shift: f64,
        ) -> Result<()> {
            if (active.applied_shift - target_shift).abs() < 0.01 {
                return Ok(());
            }
            let path = format!("{}{HEATING_CURVE_FEATURE}/commands/setCurve", device_path);
            api_post(
                http,
                access_token,
                &path,
                &json!({"shift": target_shift, "slope": active.slope}),
            )
            .await?;
            active.applied_shift = target_shift;
            Ok(())
        }

        async fn set_operating_mode(
            http: &Client,
            access_token: &str,
            device_path: &str,
            mode: &str,
            applied_mode: &mut Option<String>,
        ) -> Result<()> {
            if applied_mode.as_deref() == Some(mode) {
                return Ok(());
            }
            let path = format!("{device_path}{HEATING_MODE_FEATURE}/commands/setMode");
            api_post(http, access_token, &path, &json!({"mode": mode})).await?;
            *applied_mode = Some(mode.to_owned());
            Ok(())
        }

        async fn stop_control(
            http: &Client,
            access_token: &str,
            heat_pump: &Value,
            active: &ActiveControl,
        ) -> Result<()> {
            let device_path = device_features_path(heat_pump)?;
            let mut restore = active.clone();
            set_curve(
                http,
                access_token,
                &device_path,
                &mut restore,
                active.baseline_shift,
            )
            .await?;
            set_operating_mode(
                http,
                access_token,
                &device_path,
                &active.baseline_mode,
                &mut restore.applied_mode,
            )
            .await?;
            Ok(())
        }

        let requested_control = shared.lock().await.control.clone();
        let mut control_started = false;
        if requested_control.requested_active && controller.is_none() {
            match begin_control(
                http,
                &token.access_token,
                heat_pump,
                room_control,
                requested_control.mode,
            )
            .await
            {
                Ok(active) => {
                    controller = Some(active);
                    control_started = true;
                    let mut state = shared.lock().await;
                    state.control.active = true;
                    state.control.status = format!(
                        "Ohjaus käynnissä: {} ({:.0}–{:.0} °C mukavuusalue)",
                        state.control.mode.label(),
                        COMFORT_MIN_C,
                        COMFORT_MAX_C
                    );
                }
                Err(error) => {
                    let mut state = shared.lock().await;
                    state.control.requested_active = false;
                    state.control.active = false;
                    state.control.status = format!("Ohjausta ei voitu käynnistää: {error:#}");
                    eprintln!("{}", state.control.status);
                }
            }
        } else if !requested_control.requested_active
            && let Some(active) = controller.as_ref()
        {
            match stop_control(http, &token.access_token, heat_pump, active).await {
                Ok(()) => {
                    controller = None;
                    let mut state = shared.lock().await;
                    state.control.active = false;
                    state.control.emergency_heating = false;
                    state.control.status =
                        "Ohjaus pysäytetty; tallennetut lämmityskäyrä- ja käyttötilat palautettu"
                            .to_owned();
                }
                Err(error) => {
                    let mut state = shared.lock().await;
                    state.control.status =
                        format!("Palautus epäonnistui; ohjaus pysyy aktiivisena: {error:#}");
                    eprintln!("{}", state.control.status);
                }
            }
        }

        let room_temperature = if controller.is_some() && sample_due {
            sampled_room_temperature
                .as_ref()
                .map(|sample| sample.temperature_c)
        } else if controller.is_some() {
            let room_path = format!(
                "{}/{}",
                device_features_path(room_control)?.trim_end_matches('/'),
                ROOM_TEMPERATURE_FEATURE
            );
            match fetch_room_temperature(http, &token.access_token, &room_path).await {
                Ok(temperature) => temperature.map(|sample| sample.temperature_c),
                Err(error) => {
                    eprintln!("Room-temperature sample failed; price adjustment paused: {error:#}");
                    None
                }
            }
        } else {
            None
        };

        let curve_shift_before_update = controller.as_ref().map(|active| active.applied_shift);
        let mut control_value_changed = false;
        if let Some(active) = controller.as_mut() {
            match apply_price_control(
                http,
                &token.access_token,
                &feature_prefix,
                active,
                requested_control.mode,
                room_temperature,
                PriceWindow {
                    points: &history.prices,
                    now,
                },
            )
            .await
            {
                Ok(control_update) => {
                    let curve_shift_changed = curve_shift_before_update
                        .is_some_and(|previous| (previous - active.applied_shift).abs() >= 0.01);
                    if sample_due || control_started || curve_shift_changed {
                        control_value_changed =
                            record_control_value(&mut history, now, active.applied_shift);
                    }
                    let mut state = shared.lock().await;
                    state.control.emergency_heating = control_update.emergency_heating;
                    state.control.status = control_update.status;
                }
                Err(error) => {
                    let mut state = shared.lock().await;
                    state.control.status = format!("Ohjausvirhe; tarkista pumppu: {error:#}");
                    eprintln!("{}", state.control.status);
                }
            }
        }

        if sample_due || control_value_changed {
            write_json_atomic(&history_path, &history).with_context(|| {
                format!(
                    "Could not save monitor history to {}",
                    history_path.display()
                )
            })?;
            let today = current_helsinki_date(now)?.to_string();
            let points = chart_points_for_period(&history, "day", Some(&today), now)?;
            {
                let mut state = shared.lock().await;
                state.points = points;
                state.updated_at = now;
                state.history = history.clone();
                write_dashboard_atomic(&dashboard_path, &state.points, now, &state.control)
            }
            .with_context(|| format!("Could not update {}", dashboard_path.display()))?;

            println!("Dashboard state updated.");
        }
    }
}

async fn refresh_token_if_needed(
    http: &Client,
    token: &mut StoredToken,
    client_id: &str,
    token_file: &Path,
) -> Result<()> {
    if token.expires_at > (unix_time()?.max(0) as u64).saturating_add(120) {
        return Ok(());
    }
    let refresh_token = token
        .refresh_token
        .as_deref()
        .context("ViCare access token is expiring and no refresh token is available")?;
    *token = refresh_access_token(http, client_id, refresh_token).await?;
    write_token(token_file, token)?;
    Ok(())
}

fn parse_room_temperature(response: &Value, now: i64) -> Result<Option<TemperatureSample>> {
    let data = response
        .get("data")
        .context("Viessmann room temperature response did not contain data")?;
    let feature = if let Some(features) = data.as_array() {
        features
            .iter()
            .find(|feature| {
                feature.get("feature").and_then(Value::as_str) == Some(ROOM_TEMPERATURE_FEATURE)
            })
            .context("Viessmann room temperature response did not contain rooms.0 sensor")?
    } else {
        data
    };
    if feature.get("isEnabled").and_then(Value::as_bool) != Some(true) {
        bail!(
            "ViCare room sensor {} is disabled or has no enabled state",
            ROOM_TEMPERATURE_FEATURE
        );
    }
    let status = feature
        .pointer("/properties/status/value")
        .and_then(Value::as_str)
        .context("ViCare room sensor response has no status")?;
    if status != "connected" {
        bail!("ViCare room sensor status is {status}");
    }
    let timestamp = feature
        .get("timestamp")
        .and_then(Value::as_str)
        .map(DateTime::parse_from_rfc3339)
        .transpose()
        .context("Room temperature feature contains invalid timestamp")?
        .map(|time| time.timestamp());
    if let Some(timestamp) = timestamp {
        if timestamp > now + 60 {
            bail!("ViCare room sensor timestamp is in the future");
        }
        if now - timestamp > MAX_ROOM_TEMPERATURE_AGE_SECONDS {
            bail!(
                "ViCare room sensor reading is stale ({} seconds old; maximum is {} seconds)",
                now - timestamp,
                MAX_ROOM_TEMPERATURE_AGE_SECONDS
            );
        }
    }
    let temperature = feature
        .pointer("/properties/value/value")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && (0.0..=40.0).contains(value))
        .context("ViCare room sensor response has no valid temperature (expected 0-40 C)")?;
    Ok(Some(TemperatureSample {
        timestamp: timestamp.unwrap_or(now),
        temperature_c: temperature,
    }))
}

async fn fetch_room_temperature(
    http: &Client,
    access_token: &str,
    feature_path: &str,
) -> Result<Option<TemperatureSample>> {
    let response = api_get(http, access_token, feature_path).await?;
    parse_room_temperature(&response, unix_time()?)
}

async fn fetch_compressor_reading(
    http: &Client,
    access_token: &str,
    feature_path: &str,
    received_at: i64,
) -> Result<SensorReading> {
    let response = api_get(http, access_token, feature_path).await?;
    parse_compressor_reading(&response, received_at)
}

fn parse_compressor_reading(response: &Value, received_at: i64) -> Result<SensorReading> {
    let data = response
        .get("data")
        .context("Viessmann power response did not contain data")?;
    let feature = if let Some(features) = data.as_array() {
        features
            .iter()
            .find(|item| item.get("feature").and_then(Value::as_str) == Some(POWER_FEATURE))
            .context("Viessmann power response did not contain the compressor sensor")?
    } else {
        data
    };
    if feature.get("isEnabled").and_then(Value::as_bool) == Some(false) {
        bail!("Viessmann compressor power sensor is disabled");
    }
    if let Some(status) = feature
        .pointer("/properties/status/value")
        .and_then(Value::as_str)
        && status != "connected"
    {
        bail!("Viessmann compressor power sensor status is {status}");
    }

    let reading = feature
        .pointer("/properties/value")
        .context("Viessmann compressor power response has no value property")?;
    let value = reading
        .get("value")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0)
        .context("Viessmann compressor power value is invalid")?;
    let unit = reading
        .get("unit")
        .and_then(Value::as_str)
        .filter(|unit| !unit.trim().is_empty())
        .context("Viessmann compressor sensor response has no unit")?;
    let timestamp = feature
        .get("timestamp")
        .and_then(Value::as_str)
        .map(DateTime::parse_from_rfc3339)
        .transpose()
        .context("Viessmann power feature contains an invalid timestamp")?
        .map(|time| time.timestamp())
        .unwrap_or(received_at);
    if timestamp > received_at + 60 {
        bail!("Viessmann power sample timestamp is in the future");
    }
    if received_at - timestamp > MAX_SAMPLE_GAP_SECONDS {
        bail!("Viessmann compressor power sample is more than 10 minutes old");
    }
    Ok(SensorReading {
        timestamp,
        value,
        unit: unit.to_owned(),
    })
}

fn power_sample_from_reading(reading: &SensorReading) -> Option<PowerSample> {
    let power_kw = match reading.unit.to_ascii_lowercase().as_str() {
        "kilowatt" | "kilowatts" | "kw" => reading.value,
        "watt" | "watts" | "w" => reading.value / 1000.0,
        _ => return None,
    };
    Some(PowerSample {
        timestamp: reading.timestamp,
        power_kw,
    })
}

async fn fetch_spot_prices(http: &Client) -> Result<Vec<PricePoint>> {
    let response = http
        .get(SPOT_HINTA_URL)
        .send()
        .await
        .context("Spot-hinta.fi request failed")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read Spot-hinta.fi response")?;
    if !status.is_success() {
        bail!("Spot-hinta.fi returned {status}: {body}");
    }
    let entries: Vec<SpotPrice> =
        serde_json::from_str(&body).context("Spot-hinta.fi returned invalid price JSON")?;
    let mut prices = Vec::with_capacity(entries.len());
    for entry in entries {
        if !entry.price_with_tax.is_finite() {
            bail!("Spot-hinta.fi returned a non-finite price");
        }
        let timestamp = DateTime::parse_from_rfc3339(&entry.date_time)
            .with_context(|| format!("Invalid Spot-hinta.fi timestamp: {}", entry.date_time))?
            .timestamp();
        prices.push(PricePoint {
            timestamp: timestamp.div_euclid(QUARTER_SECONDS) * QUARTER_SECONDS,
            price_eur_per_kwh: entry.price_with_tax,
        });
    }
    if prices.is_empty() {
        bail!("Spot-hinta.fi returned no prices");
    }
    Ok(prices)
}

fn merge_prices(current: &mut Vec<PricePoint>, incoming: Vec<PricePoint>) {
    let mut merged: BTreeMap<i64, f64> = current
        .drain(..)
        .map(|point| (point.timestamp, point.price_eur_per_kwh))
        .collect();
    merged.extend(
        incoming
            .into_iter()
            .map(|point| (point.timestamp, point.price_eur_per_kwh)),
    );
    *current = merged
        .into_iter()
        .map(|(timestamp, price_eur_per_kwh)| PricePoint {
            timestamp,
            price_eur_per_kwh,
        })
        .collect();
}

fn current_helsinki_date(now: i64) -> Result<NaiveDate> {
    DateTime::<Utc>::from_timestamp(now, 0)
        .map(|instant| instant.with_timezone(&Helsinki).date_naive())
        .context("Current timestamp is outside the supported date range")
}

fn local_midnight(date: NaiveDate) -> Result<i64> {
    Helsinki
        .from_local_datetime(
            &date
                .and_hms_opt(0, 0, 0)
                .context("Could not construct local midnight")?,
        )
        .single()
        .map(|instant| instant.timestamp())
        .context("Local midnight is ambiguous or does not exist in Europe/Helsinki")
}

fn chart_points_for_period(
    history: &History,
    period: &str,
    selected_date: Option<&str>,
    now: i64,
) -> Result<Vec<ChartPoint>> {
    let today = current_helsinki_date(now)?;
    let (start_date, end_date, resolution) = match period {
        "day" => {
            let date = selected_date
                .map(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d"))
                .transpose()
                .context("Selected date must use YYYY-MM-DD format")?
                .unwrap_or(today);
            if date > today {
                bail!("Selected date cannot be in the future");
            }
            (
                date,
                date.succ_opt().context("Selected date is out of range")?,
                Resolution::Quarter,
            )
        }
        "previous_week" => {
            let current_week_start =
                today - chrono::Duration::days(i64::from(today.weekday().num_days_from_monday()));
            let start = current_week_start - chrono::Duration::days(7);
            (start, current_week_start, Resolution::Day)
        }
        "previous_month" => {
            let current_month_start = today
                .with_day(1)
                .context("Could not determine the first day of the current month")?;
            let start = current_month_start
                .checked_sub_months(Months::new(1))
                .context("Could not determine the previous month")?;
            (start, current_month_start, Resolution::Day)
        }
        "previous_year" => {
            let current_year_start = NaiveDate::from_ymd_opt(today.year(), 1, 1)
                .context("Could not determine the first day of the current year")?;
            let start = NaiveDate::from_ymd_opt(today.year() - 1, 1, 1)
                .context("Could not determine the previous year")?;
            (start, current_year_start, Resolution::Month)
        }
        _ => bail!("Unsupported chart period: {period}"),
    };

    let start = local_midnight(start_date)?;
    let end = local_midnight(end_date)?;
    let bins = match resolution {
        Resolution::Quarter => (start..end)
            .step_by(QUARTER_SECONDS as usize)
            .map(|bin_start| (bin_start, bin_start + QUARTER_SECONDS))
            .collect(),
        Resolution::Day => {
            let mut date = start_date;
            let mut bins = Vec::new();
            while date < end_date {
                let next_date = date.succ_opt().context("Chart date is out of range")?;
                bins.push((local_midnight(date)?, local_midnight(next_date)?));
                date = next_date;
            }
            bins
        }
        Resolution::Month => {
            let mut month = start_date;
            let mut bins = Vec::new();
            while month < end_date {
                let next_month = month
                    .checked_add_months(Months::new(1))
                    .context("Chart month is out of range")?;
                bins.push((local_midnight(month)?, local_midnight(next_month)?));
                month = next_month;
            }
            bins
        }
    };

    let prices: BTreeMap<i64, f64> = history
        .prices
        .iter()
        .map(|point| (point.timestamp, point.price_eur_per_kwh * 100.0))
        .collect();
    let mut samples = history.power_samples.clone();
    samples.sort_by_key(|sample| sample.timestamp);
    let mut temperatures = history.room_temperatures.clone();
    temperatures.sort_by_key(|sample| sample.timestamp);
    let mut control_values = history.control_values.clone();
    control_values.sort_by_key(|sample| sample.timestamp);
    Ok(bins
        .into_iter()
        .map(|(bin_start, bin_end)| {
            if resolution == Resolution::Quarter {
                ChartPoint {
                    timestamp: bin_start,
                    energy_kwh: energy_for_quarter(&samples, bin_start, MAX_SAMPLE_GAP_SECONDS),
                    price_cents_per_kwh: prices.get(&bin_start).copied(),
                    room_temperature_c: average_temperature(&temperatures, bin_start, bin_end),
                    curve_shift: average_control_value(&control_values, bin_start, bin_end),
                }
            } else {
                aggregate_chart_bin(
                    &samples,
                    &temperatures,
                    &control_values,
                    &prices,
                    bin_start,
                    bin_end,
                )
            }
        })
        .collect())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Resolution {
    Quarter,
    Day,
    Month,
}

fn aggregate_chart_bin(
    samples: &[PowerSample],
    temperatures: &[TemperatureSample],
    control_values: &[ControlValueSample],
    prices: &BTreeMap<i64, f64>,
    start: i64,
    end: i64,
) -> ChartPoint {
    let quarter_starts: Vec<i64> = (start..end).step_by(QUARTER_SECONDS as usize).collect();
    let energy_values: Vec<f64> = quarter_starts
        .iter()
        .filter_map(|quarter| energy_for_quarter(samples, *quarter, MAX_SAMPLE_GAP_SECONDS))
        .collect();
    let energy_kwh = (energy_values.len() == quarter_starts.len() && !energy_values.is_empty())
        .then(|| energy_values.iter().sum());
    let available_prices: Vec<f64> = quarter_starts
        .iter()
        .filter_map(|quarter| prices.get(quarter))
        .copied()
        .collect();
    let price_cents_per_kwh = (!available_prices.is_empty())
        .then(|| available_prices.iter().sum::<f64>() / available_prices.len() as f64);
    ChartPoint {
        timestamp: start,
        energy_kwh,
        price_cents_per_kwh,
        room_temperature_c: average_temperature(temperatures, start, end),
        curve_shift: average_control_value(control_values, start, end),
    }
}

fn average_control_value(samples: &[ControlValueSample], start: i64, end: i64) -> Option<f64> {
    let values: Vec<f64> = samples
        .iter()
        .filter(|sample| sample.timestamp >= start && sample.timestamp < end)
        .map(|sample| sample.curve_shift)
        .filter(|shift| shift.is_finite())
        .collect();
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

fn record_control_value(history: &mut History, timestamp: i64, curve_shift: f64) -> bool {
    if !curve_shift.is_finite() {
        return false;
    }
    let index = history
        .control_values
        .partition_point(|sample| sample.timestamp < timestamp);
    let sample = ControlValueSample {
        timestamp,
        curve_shift,
    };
    if let Some(existing) = history.control_values.get_mut(index)
        && existing.timestamp == timestamp
    {
        if (existing.curve_shift - curve_shift).abs() < f64::EPSILON {
            return false;
        }
        *existing = sample;
    } else {
        history.control_values.insert(index, sample);
    }
    true
}

fn average_temperature(samples: &[TemperatureSample], start: i64, end: i64) -> Option<f64> {
    let values: Vec<f64> = samples
        .iter()
        .filter(|sample| sample.timestamp >= start && sample.timestamp < end)
        .map(|sample| sample.temperature_c)
        .filter(|temperature| temperature.is_finite() && (0.0..=40.0).contains(temperature))
        .collect();
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

fn energy_for_quarter(samples: &[PowerSample], start: i64, max_gap_seconds: i64) -> Option<f64> {
    let end = start + QUARTER_SECONDS;
    let mut points = vec![(start, interpolate_power(samples, start, max_gap_seconds)?)];
    points.extend(
        samples
            .iter()
            .filter(|sample| sample.timestamp > start && sample.timestamp < end)
            .map(|sample| (sample.timestamp, sample.power_kw)),
    );
    points.push((end, interpolate_power(samples, end, max_gap_seconds)?));

    let mut energy = 0.0;
    for pair in points.windows(2) {
        let seconds = pair[1].0 - pair[0].0;
        if seconds <= 0 || seconds > max_gap_seconds {
            return None;
        }
        energy += (pair[0].1 + pair[1].1) / 2.0 * seconds as f64 / 3600.0;
    }
    Some(energy)
}

fn interpolate_power(samples: &[PowerSample], at: i64, max_gap_seconds: i64) -> Option<f64> {
    if let Some(sample) = samples.iter().find(|sample| sample.timestamp == at) {
        return Some(sample.power_kw);
    }
    for pair in samples.windows(2) {
        let seconds = pair[1].timestamp - pair[0].timestamp;
        if pair[0].timestamp < at
            && at < pair[1].timestamp
            && seconds > 0
            && seconds <= max_gap_seconds
        {
            let fraction = (at - pair[0].timestamp) as f64 / seconds as f64;
            return Some(pair[0].power_kw + fraction * (pair[1].power_kw - pair[0].power_kw));
        }
    }
    None
}

fn read_history(path: &Path) -> Result<History> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("Could not parse monitor history at {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(History::default()),
        Err(error) => Err(error).with_context(|| format!("Could not read {}", path.display())),
    }
}

fn migrate_history_if_missing(source: &Path, destination: &Path) -> Result<()> {
    if destination.exists() || !source.exists() {
        return Ok(());
    }
    let history = read_history(source)?;
    write_json_atomic(destination, &history).with_context(|| {
        format!(
            "Could not migrate monitor history to {}",
            destination.display()
        )
    })
}

fn write_json_atomic(path: &Path, history: &History) -> Result<()> {
    let bytes = serde_json::to_vec(history).context("Could not serialize monitor history")?;
    write_atomic(path, &bytes)
}

fn write_dashboard_atomic(
    path: &Path,
    points: &[ChartPoint],
    updated_at: i64,
    control: &UiControlState,
) -> Result<()> {
    let json = serde_json::to_string(points).context("Could not serialize dashboard points")?;
    let control_mode = match control.mode {
        ControlMode::Light => "light",
        ControlMode::Normal => "normal",
        ControlMode::Strong => "strong",
    };
    let initial_date = current_helsinki_date(updated_at)?.to_string();
    let initial_status = escape_html(&control.status);
    let html = format!(
        r#"<!doctype html>
<html lang="fi">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Vitocal — kulutus ja pörssihinta</title>
  <style>
    :root {{ color-scheme: light; font-family: system-ui, -apple-system, sans-serif; color: #18212b; background: #f3f6fa; }}
    body {{ margin: 0; padding: 24px; }}
    main {{ max-width: 1400px; margin: auto; }}
    h1 {{ margin: 0 0 6px; font-size: clamp(1.4rem, 3vw, 2rem); }}
    .subtitle, .note {{ color: #536273; }}
    .card {{ margin-top: 20px; padding: 18px; background: white; border: 1px solid #dce3eb; border-radius: 12px; box-shadow: 0 3px 12px #2030400c; }}
    .legend {{ display: flex; flex-wrap: wrap; gap: 18px; margin: 2px 0 14px; font-size: .95rem; }}
    .key {{ display: inline-block; width: 14px; height: 14px; margin-right: 7px; vertical-align: -2px; border-radius: 3px; }}
    .control-key {{ width: 20px; height: 3px; background: #4488d4; vertical-align: 4px; }}
    .line-key {{ width: 20px; height: 3px; background: #e66d4b; vertical-align: 4px; }}
    .temperature-key {{ width: 20px; height: 3px; background: #27845b; vertical-align: 4px; }}
    .chart-wrap {{ position: relative; width: 100%; }}
    canvas {{ display: block; width: 100%; height: min(62vh, 520px); min-height: 340px; }}
    #tip {{ position: absolute; display: none; pointer-events: none; z-index: 1; padding: 9px 11px; border-radius: 7px; background: #18212b; color: white; font-size: 13px; line-height: 1.5; white-space: nowrap; box-shadow: 0 2px 8px #0003; }}
    .note {{ margin: 14px 0 0; font-size: .9rem; line-height: 1.45; }}
    .controls {{ display: flex; flex-wrap: wrap; align-items: center; gap: 10px; }}
    .controls button, .controls select {{ min-height: 40px; padding: 8px 14px; border: 1px solid #b9c7d6; border-radius: 7px; background: white; font: inherit; }}
    .controls button {{ cursor: pointer; font-weight: 650; }}
    #sensor-reading {{ padding: 12px 14px; border-radius: 8px; background: #edf4fb; color: #18344f; font-weight: 650; }}
    #room-temperature-reading {{ padding: 12px 14px; border-radius: 8px; background: #edf7f1; color: #205c3c; font-weight: 650; }}
    .history-controls {{ display: flex; flex-wrap: wrap; align-items: center; gap: 10px; margin: 0 0 16px; }}
    .history-controls select, .history-controls input {{ min-height: 40px; padding: 8px 12px; border: 1px solid #b9c7d6; border-radius: 7px; background: white; font: inherit; }}
    #start {{ background: #176b43; color: white; border-color: #176b43; }}
    #stop {{ background: #a33131; color: white; border-color: #a33131; }}
    #control-status {{ margin: 12px 0 0; line-height: 1.45; }}
    @media (max-width: 600px) {{ body {{ padding: 12px; }} .card {{ padding: 10px; }} }}
  </style>
</head>
<body>
<main>
  <h1>Vitocal — lämmitysohjaus ja pörssihinta</h1>
  <div class="subtitle">Lämmityskäyrän siirto ja huonelämpötila suhteessa Spot-hinta.fi:n verolliseen varttihintaan</div>
  <section class="card" aria-label="Automaattiohjaus">
    <h2>Automaattiohjaus</h2>
    <div class="controls">
      <label for="control-mode">Ohjauksen voimakkuus</label>
      <select id="control-mode">
        <option value="light" {light_selected}>Kevyt</option>
        <option value="normal" {normal_selected}>Normaali</option>
        <option value="strong" {strong_selected}>Jyrkkä</option>
      </select>
      <button id="start" type="button">Käynnistä ohjaus</button>
      <button id="stop" type="button">Pysäytä ja palauta asetukset</button>
    </div>
    <p id="control-status" role="status" aria-live="polite">{initial_status}</p>
    <p class="note">Ohjaus muuttaa lämmityskäyrän siirtoa hintaluokan mukaan. Mukavuusalue on 20–22 °C: vähintään 22 °C:ssa halvan hinnan mukainen lisälämmitys estetään, ja enintään 20 °C:ssa kalliin hinnan mukainen lämmityksen vähennys estetään. Pysäytys ja Ctrl+C palauttavat käynnistyksessä tallennetut asetukset. Käyttö vaatii toimivan huoneanturin ja Viessmannin sallimat asetukset.</p>
  </section>
  <section class="card">
    <div class="history-controls" aria-label="Graafin aikaväli">
      <label for="history-period">Näytä</label>
      <select id="history-period">
        <option value="day">Päivä</option>
        <option value="previous_week">Edellinen viikko</option>
        <option value="previous_month">Edellinen kuukausi</option>
        <option value="previous_year">Edellinen vuosi</option>
      </select>
      <label id="history-date-label" for="history-date">Päivä</label>
      <input id="history-date" type="date" value="{initial_date}" max="{initial_date}">
    </div>
    <div class="legend">
      <span><i class="key control-key"></i>Lämmityskäyrän siirto (ViCare-arvo, vasen akseli)</span>
      <span><i class="key line-key"></i>Sähkön hinta (c/kWh, oikea akseli)</span>
      <span><i class="key temperature-key"></i>Huonelämpötila (°C, oikea akseli)</span>
    </div>
    <p id="sensor-reading" role="status" aria-live="polite">Luetaan kompressorin anturia...</p>
    <p id="room-temperature-reading" role="status" aria-live="polite">Luetaan huonelämpötila-anturia...</p>
    <div class="chart-wrap"><canvas id="chart" aria-label="Varttikohtainen lämmityskäyrän siirto, sähkön hinta ja huonelämpötila"></canvas><div id="tip"></div></div>
    <p class="note">Sininen viiva näyttää automaattiohjauksen käyttämän lämmityskäyrän siirtoarvon; arvo tallennetaan ohjauksen ollessa aktiivinen viiden minuutin välein ja aina siirron muuttuessa. Se on ViCare-arvo, ei mitattu sähköteho eikä huoneen lämpötila. Vihreä viiva näyttää tallennettujen huonelämpötilanäytteiden keskiarvon. Näytteettömät jaksot jäävät tyhjiksi. Hintaviiva ja lämpötilaviiva käyttävät oikean reunan eri asteikkoja; ohjausarvo käyttää vasenta asteikkoa. Näkymä päivittyy 30 sekunnin välein. Viimeisin päivitys: <time id="updated"></time>.</p>
  </section>
</main>
<script>
let points = {json};
const updatedAt = {updated_at};
const initialMode = "{control_mode}";
const canvas = document.getElementById('chart');
const context = canvas.getContext('2d');
const tooltip = document.getElementById('tip');
const formatter = new Intl.DateTimeFormat('fi-FI', {{ timeZone: 'Europe/Helsinki', day: '2-digit', month: '2-digit', hour: '2-digit', minute: '2-digit' }});
document.getElementById('updated').textContent = formatter.format(new Date(updatedAt * 1000));
const modeSelect = document.getElementById('control-mode');
const statusText = document.getElementById('control-status');
const sensorReadingText = document.getElementById('sensor-reading');
const roomTemperatureReadingText = document.getElementById('room-temperature-reading');
const historyPeriod = document.getElementById('history-period');
const historyDate = document.getElementById('history-date');
const historyDateLabel = document.getElementById('history-date-label');
modeSelect.value = initialMode;

async function sendControl(active) {{
  const response = await fetch('/api/control', {{
    method: 'POST',
    headers: {{ 'Content-Type': 'application/json' }},
    body: JSON.stringify({{ active, mode: modeSelect.value }})
  }});
  if (!response.ok) throw new Error(`Ohjauspyyntö epäonnistui (${{response.status}})`);
  const state = await response.json();
  statusText.textContent = state.control.status;
  renderCompressorReading(state);
  renderRoomTemperature(state);
}}
function renderCompressorReading(state) {{
  if (state.compressor_reading) {{
    const reading = state.compressor_reading;
    const time = formatter.format(new Date(reading.timestamp * 1000));
    const unit = reading.unit.toLowerCase();
    if (unit === 'percent' || unit === '%') {{
      sensorReadingText.textContent = `Kompressorin kuormitus: ${{reading.value.toFixed(1)}} % (päivitetty ${{time}}). Tämä ei ole sähköteho eikä näy kulutuksena graafissa.`;
    }} else if (['kilowatt', 'kilowatts', 'kw', 'watt', 'watts', 'w'].includes(unit)) {{
      sensorReadingText.textContent = `Kompressorin sähköteho: ${{reading.value}} ${{reading.unit}} (päivitetty ${{time}}).`;
    }} else {{
      sensorReadingText.textContent = `Kompressorin anturin lukema: ${{reading.value}} ${{reading.unit}} (päivitetty ${{time}}).`;
    }}
  }} else if (state.compressor_reading_error) {{
    sensorReadingText.textContent = `Kompressorin lukemaa ei saatu: ${{state.compressor_reading_error}}`;
  }} else {{
    sensorReadingText.textContent = 'Kompressorin lukemaa ei ole vielä saatavilla.';
  }}
}}
function renderRoomTemperature(state) {{
  if (state.room_temperature) {{
    const reading = state.room_temperature;
    const time = formatter.format(new Date(reading.timestamp * 1000));
    roomTemperatureReadingText.textContent = `Huonelämpötila: ${{reading.temperature_c.toFixed(1)}} °C (päivitetty ${{time}}).`;
  }} else if (state.room_temperature_error) {{
    roomTemperatureReadingText.textContent = `Huonelämpötilaa ei saatu: ${{state.room_temperature_error}}`;
  }} else {{
    roomTemperatureReadingText.textContent = 'Huonelämpötilaa ei ole vielä saatavilla.';
  }}
}}
async function refreshControlState() {{
  try {{
    const query = new URLSearchParams({{ period: historyPeriod.value }});
    if (historyPeriod.value === 'day') query.set('date', historyDate.value);
    const response = await fetch(`/api/state?${{query}}`, {{ cache: 'no-store' }});
    if (!response.ok) throw new Error(`Tilatietoa ei saatu (${{response.status}})`);
    const state = await response.json();
    statusText.textContent = state.control.status;
    renderCompressorReading(state);
    renderRoomTemperature(state);
    const wasShowingToday = historyDate.value === historyDate.max;
    historyDate.max = state.today;
    if (wasShowingToday && historyPeriod.value === 'day') historyDate.value = state.today;
    points = state.points;
    document.getElementById('updated').textContent = formatter.format(new Date(state.updated_at * 1000));
    draw();
  }} catch (error) {{
    statusText.textContent = `Yhteys ohjauspalveluun epäonnistui: ${{error.message}}`;
  }}
}}
document.getElementById('start').addEventListener('click', () => sendControl(true).catch(error => statusText.textContent = error.message));
document.getElementById('stop').addEventListener('click', () => sendControl(false).catch(error => statusText.textContent = error.message));
function updateHistoryControls() {{
  const isDay = historyPeriod.value === 'day';
  historyDate.hidden = !isDay;
  historyDateLabel.hidden = !isDay;
}}
historyPeriod.addEventListener('change', () => {{ updateHistoryControls(); refreshControlState(); }});
historyDate.addEventListener('change', refreshControlState);
updateHistoryControls();
setInterval(refreshControlState, 30000);
refreshControlState();

function draw() {{
  const ratio = window.devicePixelRatio || 1;
  const rect = canvas.getBoundingClientRect();
  canvas.width = Math.round(rect.width * ratio);
  canvas.height = Math.round(rect.height * ratio);
  context.setTransform(ratio, 0, 0, ratio, 0, 0);
  const width = rect.width, height = rect.height;
  const left = 66, right = 110, top = 24, bottom = 52;
  const plotW = width - left - right, plotH = height - top - bottom;
  const validPrices = points.map(p => p.price_cents_per_kwh).filter(Number.isFinite);
  const validTemperatures = points.map(p => p.room_temperature_c).filter(Number.isFinite);
  const validControlValues = points.map(p => p.curve_shift).filter(Number.isFinite);
  let minControl = validControlValues.length ? Math.min(...validControlValues) : -1;
  let maxControl = validControlValues.length ? Math.max(...validControlValues) : 1;
  if (minControl === maxControl) {{ minControl -= 0.5; maxControl += 0.5; }}
  const controlPad = Math.max(0.1, (maxControl - minControl) * 0.08);
  minControl -= controlPad; maxControl += controlPad;
  let minPrice = validPrices.length ? Math.min(...validPrices) : 0;
  let maxPrice = validPrices.length ? Math.max(...validPrices) : 1;
  if (minPrice === maxPrice) {{ minPrice -= 0.5; maxPrice += 0.5; }}
  const pricePad = (maxPrice - minPrice) * 0.08;
  minPrice -= pricePad; maxPrice += pricePad;
  let minTemperature = validTemperatures.length ? Math.min(...validTemperatures) : 18;
  let maxTemperature = validTemperatures.length ? Math.max(...validTemperatures) : 24;
  if (minTemperature === maxTemperature) {{ minTemperature -= 0.5; maxTemperature += 0.5; }}
  const temperaturePad = Math.max(0.2, (maxTemperature - minTemperature) * 0.08);
  minTemperature -= temperaturePad; maxTemperature += temperaturePad;
  const x = index => left + (index + 0.5) * plotW / points.length;
  const yControl = value => top + plotH - (value - minControl) / (maxControl - minControl) * plotH;
  const yPrice = value => top + plotH - (value - minPrice) / (maxPrice - minPrice) * plotH;
  const yTemperature = value => top + plotH - (value - minTemperature) / (maxTemperature - minTemperature) * plotH;

  context.clearRect(0, 0, width, height);
  context.font = '12px system-ui, sans-serif';
  context.textBaseline = 'middle';
  context.strokeStyle = '#e7edf3';
  context.fillStyle = '#536273';
  for (let tick = 0; tick <= 4; tick++) {{
    const y = top + tick * plotH / 4;
    context.beginPath(); context.moveTo(left, y); context.lineTo(width - right, y); context.stroke();
    const controlTick = maxControl - (maxControl - minControl) * tick / 4;
    const priceTick = maxPrice - (maxPrice - minPrice) * tick / 4;
    context.textAlign = 'right';
    context.fillStyle = '#4488d4';
    context.fillText(controlTick.toFixed(2), left - 9, y);
    context.textAlign = 'left';
    context.fillStyle = '#e66d4b';
    context.fillText(priceTick.toFixed(1), width - right + 9, y);
    const temperatureTick = maxTemperature - (maxTemperature - minTemperature) * tick / 4;
    context.fillStyle = '#27845b';
    context.fillText(temperatureTick.toFixed(1), width - right + 52, y);
    context.fillStyle = '#536273';
  }}
  context.strokeStyle = '#e66d4b'; context.fillStyle = '#e66d4b';
  context.lineWidth = 2.3; context.beginPath();
  let connected = false;
  points.forEach((point, index) => {{
    if (!Number.isFinite(point.price_cents_per_kwh)) {{ connected = false; return; }}
    const px = x(index), py = yPrice(point.price_cents_per_kwh);
    if (connected) context.lineTo(px, py); else context.moveTo(px, py);
    connected = true;
  }});
  context.stroke();
  points.forEach((point, index) => {{
    if (Number.isFinite(point.price_cents_per_kwh)) {{
      context.beginPath(); context.arc(x(index), yPrice(point.price_cents_per_kwh), 2.3, 0, Math.PI * 2); context.fill();
    }}
  }});
  context.strokeStyle = '#27845b'; context.fillStyle = '#27845b';
  context.lineWidth = 2.3; context.beginPath();
  connected = false;
  points.forEach((point, index) => {{
    if (!Number.isFinite(point.room_temperature_c)) {{ connected = false; return; }}
    const px = x(index), py = yTemperature(point.room_temperature_c);
    if (connected) context.lineTo(px, py); else context.moveTo(px, py);
    connected = true;
  }});
  context.stroke();
  points.forEach((point, index) => {{
    if (Number.isFinite(point.room_temperature_c)) {{
      context.beginPath(); context.arc(x(index), yTemperature(point.room_temperature_c), 2.3, 0, Math.PI * 2); context.fill();
    }}
  }});
  context.strokeStyle = '#ffffff';
  context.lineWidth = 7;
  context.lineJoin = 'round';
  context.lineCap = 'round';
  context.beginPath();
  connected = false;
  points.forEach((point, index) => {{
    if (!Number.isFinite(point.curve_shift)) {{ connected = false; return; }}
    const px = x(index), py = yControl(point.curve_shift);
    if (connected) context.lineTo(px, py); else context.moveTo(px, py);
    connected = true;
  }});
  context.stroke();
  context.strokeStyle = '#1769aa'; context.fillStyle = '#1769aa';
  context.lineWidth = 3.5;
  context.beginPath();
  connected = false;
  points.forEach((point, index) => {{
    if (!Number.isFinite(point.curve_shift)) {{ connected = false; return; }}
    const px = x(index), py = yControl(point.curve_shift);
    if (connected) context.lineTo(px, py); else context.moveTo(px, py);
    connected = true;
  }});
  context.stroke();
  points.forEach((point, index) => {{
    if (Number.isFinite(point.curve_shift)) {{
      context.beginPath(); context.arc(x(index), yControl(point.curve_shift), 4, 0, Math.PI * 2); context.fill();
      context.strokeStyle = '#ffffff'; context.lineWidth = 1.5; context.stroke();
      context.fillStyle = '#1769aa';
    }}
  }});
  context.fillStyle = '#536273'; context.textAlign = 'center';
  const labelEvery = Math.max(1, Math.ceil(points.length / 12));
  points.forEach((point, index) => {{
    if (index % labelEvery === 0) context.fillText(formatter.format(new Date(point.timestamp * 1000)), x(index), height - 24);
  }});
  context.save(); context.translate(16, top + plotH / 2); context.rotate(-Math.PI / 2);
  context.textAlign = 'center'; context.fillText('Lämmityskäyrän siirto (ViCare-arvo)', 0, 0); context.restore();
  context.save(); context.translate(width - 12, top + plotH / 2); context.rotate(Math.PI / 2);
  context.textAlign = 'center'; context.fillText('Hinta (c/kWh) / lämpötila (°C)', 0, 0); context.restore();
}}

canvas.addEventListener('mousemove', event => {{
  const rect = canvas.getBoundingClientRect();
  const left = 66, right = 110;
  const fraction = (event.clientX - rect.left - left) / (rect.width - left - right);
  const index = Math.max(0, Math.min(points.length - 1, Math.floor(fraction * points.length)));
  const point = points[index];
  if (!point) {{ tooltip.style.display = 'none'; return; }}
  const price = Number.isFinite(point.price_cents_per_kwh) ? `${{point.price_cents_per_kwh.toFixed(3)}} c/kWh` : 'hinta ei saatavilla';
  const temperature = Number.isFinite(point.room_temperature_c) ? `${{point.room_temperature_c.toFixed(1)}} °C` : 'lämpötila ei saatavilla';
  const controlValue = Number.isFinite(point.curve_shift) ? point.curve_shift.toFixed(2) : 'ohjaus ei aktiivinen';
  tooltip.innerHTML = `${{formatter.format(new Date(point.timestamp * 1000))}}<br>Lämmityskäyrän siirto: ${{controlValue}}<br>Hinta: ${{price}}<br>Huonelämpötila: ${{temperature}}`;
  tooltip.style.display = 'block';
  tooltip.style.left = `${{Math.min(rect.width - 250, Math.max(0, event.clientX - rect.left + 12))}}px`;
  tooltip.style.top = `${{Math.max(0, event.clientY - rect.top - 50)}}px`;
}});
canvas.addEventListener('mouseleave', () => tooltip.style.display = 'none');
window.addEventListener('resize', draw);
draw();
</script>
</body>
</html>
"#,
        light_selected = if control.mode == ControlMode::Light {
            "selected"
        } else {
            ""
        },
        normal_selected = if control.mode == ControlMode::Normal {
            "selected"
        } else {
            ""
        },
        strong_selected = if control.mode == ControlMode::Strong {
            "selected"
        } else {
            ""
        },
        initial_status = initial_status,
    );
    write_atomic(path, html.as_bytes())
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("Could not create {}", parent.display()))?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes)
        .with_context(|| format!("Could not write {}", temporary.display()))?;
    fs::rename(&temporary, path)
        .with_context(|| format!("Could not move dashboard into {}", path.display()))
}

fn unix_time() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock is before Unix epoch")?
        .as_secs() as i64)
}

async fn serve_dashboard(
    listener: TcpListener,
    html_path: PathBuf,
    state: Arc<Mutex<DashboardState>>,
    control_notify: Arc<Notify>,
) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await.context("Dashboard accept failed")?;
        let html_path = html_path.clone();
        let state = Arc::clone(&state);
        let control_notify = Arc::clone(&control_notify);
        tokio::spawn(async move {
            if let Err(error) = handle_http(stream, &html_path, state, control_notify).await {
                eprintln!("Dashboard request failed: {error:#}");
            }
        });
    }
}

async fn handle_http(
    mut stream: TcpStream,
    html_path: &Path,
    state: Arc<Mutex<DashboardState>>,
    control_notify: Arc<Notify>,
) -> Result<()> {
    const MAX_REQUEST_BYTES: usize = 16 * 1024;
    let mut request_bytes = Vec::with_capacity(4096);
    let mut chunk = [0_u8; 4096];
    let mut expected_len = None;
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .context("Could not read dashboard request")?;
        if read == 0 {
            break;
        }
        request_bytes.extend_from_slice(&chunk[..read]);
        if request_bytes.len() > MAX_REQUEST_BYTES {
            bail!("Dashboard request exceeded the size limit");
        }
        if expected_len.is_none()
            && let Some(header_end) = request_bytes
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
        {
            let header = String::from_utf8_lossy(&request_bytes[..header_end]);
            let content_length = header
                .lines()
                .skip(1)
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>())
                })
                .transpose()
                .context("Invalid dashboard Content-Length")?
                .unwrap_or(0);
            expected_len = Some(header_end + 4 + content_length);
        }
        if expected_len.is_some_and(|expected| request_bytes.len() >= expected) {
            break;
        }
    }
    let header_end = request_bytes
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .context("Dashboard request has incomplete headers")?;
    let expected_len =
        expected_len.context("Dashboard request is missing its header terminator")?;
    if request_bytes.len() < expected_len {
        bail!("Dashboard request body ended before Content-Length bytes were received");
    }
    let header = String::from_utf8_lossy(&request_bytes[..header_end]);
    let body = String::from_utf8_lossy(&request_bytes[header_end + 4..expected_len]);
    let request_line = header.lines().next().context("Empty dashboard request")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().context("Missing HTTP method")?;
    let request_target = parts.next().context("Missing HTTP path")?;
    let (path, query) = request_target
        .split_once('?')
        .unwrap_or((request_target, ""));

    let (status, content_type, response_body) = match (method, path) {
        ("GET", "/") => match fs::read(html_path) {
            Ok(bytes) => ("200 OK", "text/html; charset=utf-8", bytes),
            Err(error) => (
                "500 Internal Server Error",
                "text/plain; charset=utf-8",
                format!("Dashboard file unavailable: {error}").into_bytes(),
            ),
        },
        ("GET", "/api/state") => {
            let mut period = "day";
            let mut date = None;
            for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
                match key.as_ref() {
                    "period" => {
                        period = match value.as_ref() {
                            "day" => "day",
                            "previous_week" => "previous_week",
                            "previous_month" => "previous_month",
                            "previous_year" => "previous_year",
                            _ => "",
                        }
                    }
                    "date" => date = Some(value.into_owned()),
                    _ => {}
                }
            }
            if period.is_empty() {
                (
                    "400 Bad Request",
                    "application/json",
                    br#"{"error":"Unsupported period"}"#.to_vec(),
                )
            } else {
                let state = state.lock().await;
                match chart_points_for_period(&state.history, period, date.as_deref(), unix_time()?)
                {
                    Ok(points) => (
                        "200 OK",
                        "application/json",
                        serde_json::to_vec(&json!({
                            "points": points,
                            "updated_at": state.updated_at,
                            "today": current_helsinki_date(unix_time()?)?.to_string(),
                            "control": state.control,
                            "compressor_reading": state.compressor_reading,
                            "compressor_reading_error": state.compressor_reading_error,
                            "room_temperature": state.room_temperature,
                            "room_temperature_error": state.room_temperature_error,
                        }))
                        .context("Could not serialize dashboard state")?,
                    ),
                    Err(error) => (
                        "400 Bad Request",
                        "application/json",
                        serde_json::to_vec(&json!({"error": format!("{error:#}")}))
                            .context("Could not serialize chart request error")?,
                    ),
                }
            }
        }
        ("POST", "/api/control") => {
            let request: UiControlRequest =
                serde_json::from_str(&body).context("Invalid control request JSON")?;
            let mut state = state.lock().await;
            state.control.mode = request.mode;
            state.control.requested_active = request.active;
            state.control.status = if request.active {
                format!(
                    "{}-ohjauspyyntö vastaanotettu; käsitellään nyt",
                    request.mode.label()
                )
            } else {
                "Ohjauksen pysäytyspyyntö vastaanotettu; palautetaan asetukset nyt".to_owned()
            };
            control_notify.notify_one();
            (
                "202 Accepted",
                "application/json",
                serde_json::to_vec(&*state).context("Could not serialize control state")?,
            )
        }
        _ => (
            "404 Not Found",
            "text/plain; charset=utf-8",
            b"Not found".to_vec(),
        ),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        response_body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.write_all(&response_body).await?;
    Ok(())
}

#[derive(Deserialize)]
struct UiControlRequest {
    active: bool,
    #[serde(default)]
    mode: ControlMode,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_compressor_power_in_supported_units() {
        let kilowatts = json!({
            "data": [{
                "feature": POWER_FEATURE,
                "isEnabled": true,
                "properties": {
                    "status": {"value": "connected"},
                    "value": {"value": 1.25, "unit": "kilowatt"}
                }
            }]
        });
        let watts = json!({
            "data": {
                "isEnabled": true,
                "properties": {"value": {"value": 1250.0, "unit": "watt"}}
            }
        });
        let kilowatt_reading = parse_compressor_reading(&kilowatts, 1_800).unwrap();
        let watt_reading = parse_compressor_reading(&watts, 1_800).unwrap();
        assert_eq!(
            power_sample_from_reading(&kilowatt_reading)
                .unwrap()
                .power_kw,
            1.25
        );
        assert_eq!(
            power_sample_from_reading(&watt_reading).unwrap().power_kw,
            1.25
        );
    }

    #[test]
    fn keeps_percentage_reading_separate_from_electrical_power() {
        let unknown_unit = json!({
            "data": {
                "properties": {"value": {"value": 10.0, "unit": "percent"}}
            }
        });
        let disconnected = json!({
            "data": {
                "properties": {
                    "status": {"value": "notConnected"},
                    "value": {"value": 10.0, "unit": "kilowatt"}
                }
            }
        });
        let reading = parse_compressor_reading(&unknown_unit, 1_800).unwrap();
        assert_eq!(reading.value, 10.0);
        assert_eq!(reading.unit, "percent");
        assert!(power_sample_from_reading(&reading).is_none());
        assert!(parse_compressor_reading(&disconnected, 1_800).is_err());
    }

    #[test]
    fn parses_room_temperature_from_single_feature_and_feature_list_responses() {
        let feature = json!({
            "feature": ROOM_TEMPERATURE_FEATURE,
            "isEnabled": true,
            "timestamp": "2026-10-09T10:00:00Z",
            "properties": {
                "status": {"value": "connected"},
                "value": {"value": 20.5, "unit": "celsius"}
            }
        });
        let single = json!({"data": feature});
        let list = json!({"data": [feature]});
        let now = DateTime::parse_from_rfc3339("2026-10-09T10:01:00Z")
            .unwrap()
            .timestamp();

        let expected = Some(TemperatureSample {
            timestamp: DateTime::parse_from_rfc3339("2026-10-09T10:00:00Z")
                .unwrap()
                .timestamp(),
            temperature_c: 20.5,
        });
        assert_eq!(parse_room_temperature(&single, now).unwrap(), expected);
        assert_eq!(parse_room_temperature(&list, now).unwrap(), expected);
    }

    #[test]
    fn rejects_disconnected_disabled_stale_and_invalid_room_readings() {
        let connected = json!({
            "feature": ROOM_TEMPERATURE_FEATURE,
            "isEnabled": true,
            "timestamp": "2026-10-09T10:00:00Z",
            "properties": {
                "status": {"value": "connected"},
                "value": {"value": 20.5}
            }
        });
        let now = DateTime::parse_from_rfc3339("2026-10-09T10:01:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(
            parse_room_temperature(
                &json!({"data": [{"feature": "rooms.others.0.sensors.temperature"}, connected.clone()]}),
                now,
            )
            .unwrap(),
            Some(TemperatureSample {
                timestamp: now - 60,
                temperature_c: 20.5,
            })
        );

        let mut disconnected = connected.clone();
        disconnected["properties"]["status"]["value"] = json!("notConnected");
        assert!(
            parse_room_temperature(&json!({"data": disconnected}), now)
                .unwrap_err()
                .to_string()
                .contains("notConnected")
        );

        let mut disabled = connected.clone();
        disabled["isEnabled"] = json!(false);
        assert!(
            parse_room_temperature(&json!({"data": disabled}), now)
                .unwrap_err()
                .to_string()
                .contains("disabled")
        );

        let mut slow_update = connected.clone();
        slow_update["timestamp"] = json!(
            DateTime::<Utc>::from_timestamp(now - 1_101, 0)
                .unwrap()
                .to_rfc3339()
        );
        assert_eq!(
            parse_room_temperature(&json!({"data": slow_update}), now).unwrap(),
            Some(TemperatureSample {
                timestamp: now - 1_101,
                temperature_c: 20.5,
            })
        );

        let mut stale = connected;
        stale["timestamp"] = json!(
            DateTime::<Utc>::from_timestamp(now - MAX_ROOM_TEMPERATURE_AGE_SECONDS - 1, 0)
                .unwrap()
                .to_rfc3339()
        );
        assert!(
            parse_room_temperature(&json!({"data": stale}), now)
                .unwrap_err()
                .to_string()
                .contains("stale")
        );
        assert!(parse_room_temperature(&json!({}), now).is_err());
    }

    #[test]
    fn computes_energy_for_quarter_only_with_full_sample_coverage() {
        let samples = vec![
            PowerSample {
                timestamp: 0,
                power_kw: 2.0,
            },
            PowerSample {
                timestamp: 300,
                power_kw: 2.0,
            },
            PowerSample {
                timestamp: 600,
                power_kw: 2.0,
            },
            PowerSample {
                timestamp: 900,
                power_kw: 2.0,
            },
        ];
        assert_eq!(energy_for_quarter(&samples, 0, 600), Some(0.5));
        assert_eq!(energy_for_quarter(&samples[1..], 0, 600), None);
    }

    #[test]
    fn chart_periods_use_helsinki_calendar_days_and_previous_calendar_periods() {
        let history = History::default();
        let now =
            local_midnight(NaiveDate::from_ymd_opt(2026, 10, 9).unwrap()).unwrap() + 12 * 3600;
        let today = chart_points_for_period(&history, "day", None, now).unwrap();
        assert_eq!(today.len(), 96);
        assert_eq!(
            today[0].timestamp,
            local_midnight(NaiveDate::from_ymd_opt(2026, 10, 9).unwrap()).unwrap()
        );

        let previous_week = chart_points_for_period(&history, "previous_week", None, now).unwrap();
        assert_eq!(previous_week.len(), 7);
        let previous_month =
            chart_points_for_period(&history, "previous_month", None, now).unwrap();
        assert_eq!(previous_month.len(), 30);
        let previous_year = chart_points_for_period(&history, "previous_year", None, now).unwrap();
        assert_eq!(previous_year.len(), 12);
    }

    #[test]
    fn historical_measurements_remain_in_the_json_archive() {
        let path = std::env::temp_dir().join(format!(
            "rust-viessman-history-test-{}.json",
            std::process::id()
        ));
        let history = History {
            power_samples: vec![PowerSample {
                timestamp: 1,
                power_kw: 1.25,
            }],
            prices: vec![PricePoint {
                timestamp: 1,
                price_eur_per_kwh: 0.1,
            }],
            compressor_readings: vec![SensorReading {
                timestamp: 1,
                value: 0.0,
                unit: "percent".to_owned(),
            }],
            room_temperatures: vec![TemperatureSample {
                timestamp: 2,
                temperature_c: 20.5,
            }],
            control_values: vec![ControlValueSample {
                timestamp: 3,
                curve_shift: -3.0,
            }],
        };

        write_json_atomic(&path, &history).unwrap();
        let restored = read_history(&path).unwrap();
        fs::remove_file(path).unwrap();

        assert_eq!(restored.power_samples.len(), 1);
        assert_eq!(restored.power_samples[0].timestamp, 1);
        assert_eq!(restored.power_samples[0].power_kw, 1.25);
        assert_eq!(restored.prices.len(), 1);
        assert_eq!(restored.compressor_readings.len(), 1);
        assert_eq!(restored.compressor_readings[0].unit, "percent");
        assert_eq!(
            restored.room_temperatures,
            vec![TemperatureSample {
                timestamp: 2,
                temperature_c: 20.5,
            }]
        );
        assert_eq!(
            restored.control_values,
            vec![ControlValueSample {
                timestamp: 3,
                curve_shift: -3.0,
            }]
        );
    }

    #[test]
    fn migrates_existing_history_without_overwriting_project_history() {
        let root = std::env::temp_dir().join(format!(
            "rust-viessman-history-migration-test-{}",
            std::process::id()
        ));
        let source = root.join("old/power-price-history.json");
        let destination = root.join("project/power-price-history.json");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        let old_history = History {
            compressor_readings: vec![SensorReading {
                timestamp: 1,
                value: 52.0,
                unit: "percent".to_owned(),
            }],
            ..History::default()
        };
        write_json_atomic(&source, &old_history).unwrap();

        migrate_history_if_missing(&source, &destination).unwrap();
        assert_eq!(
            read_history(&destination)
                .unwrap()
                .compressor_readings
                .len(),
            1
        );

        let project_history = History {
            prices: vec![PricePoint {
                timestamp: 2,
                price_eur_per_kwh: 0.2,
            }],
            ..History::default()
        };
        write_json_atomic(&destination, &project_history).unwrap();
        migrate_history_if_missing(&source, &destination).unwrap();
        let retained = read_history(&destination).unwrap();
        assert_eq!(retained.prices.len(), 1);
        assert!(retained.compressor_readings.is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selected_day_range_handles_daylight_saving_time_and_rejects_future() {
        let history = History::default();
        let now = local_midnight(NaiveDate::from_ymd_opt(2026, 10, 26).unwrap()).unwrap() + 3600;
        let spring_day = chart_points_for_period(&history, "day", Some("2026-03-29"), now).unwrap();
        let autumn_day = chart_points_for_period(&history, "day", Some("2026-10-25"), now).unwrap();
        assert_eq!(spring_day.len(), 92);
        assert_eq!(autumn_day.len(), 100);
        assert!(chart_points_for_period(&history, "day", Some("2026-10-27"), now).is_err());
    }

    #[test]
    fn summary_energy_requires_complete_quarter_coverage() {
        let samples: Vec<_> = (0..=12)
            .map(|step| PowerSample {
                timestamp: step * 300,
                power_kw: 2.0,
            })
            .collect();
        let prices: BTreeMap<_, _> = (0..4)
            .map(|step| (step * QUARTER_SECONDS, 10.0 + step as f64))
            .collect();
        let temperatures: Vec<_> = (0..4)
            .map(|step| TemperatureSample {
                timestamp: step * QUARTER_SECONDS + 60,
                temperature_c: 20.0 + step as f64,
            })
            .collect();
        let control_values: Vec<_> = (0..4)
            .map(|step| ControlValueSample {
                timestamp: step * QUARTER_SECONDS + 60,
                curve_shift: -4.0 + step as f64,
            })
            .collect();
        let complete =
            aggregate_chart_bin(&samples, &temperatures, &control_values, &prices, 0, 3600);
        assert_eq!(complete.energy_kwh, Some(2.0));
        assert_eq!(complete.price_cents_per_kwh, Some(11.5));
        assert_eq!(complete.room_temperature_c, Some(21.5));
        assert_eq!(complete.curve_shift, Some(-2.5));
        let incomplete = aggregate_chart_bin(
            &samples[1..],
            &temperatures,
            &control_values,
            &prices,
            0,
            3600,
        );
        assert_eq!(incomplete.energy_kwh, None);
    }

    #[test]
    fn records_control_values_in_timestamp_order_and_replaces_same_timestamp() {
        let mut history = History::default();
        assert!(record_control_value(&mut history, 20, -3.0));
        assert!(record_control_value(&mut history, 10, -4.0));
        assert!(record_control_value(&mut history, 20, -2.5));
        assert!(!record_control_value(&mut history, 20, -2.5));
        assert_eq!(
            history.control_values,
            vec![
                ControlValueSample {
                    timestamp: 10,
                    curve_shift: -4.0,
                },
                ControlValueSample {
                    timestamp: 20,
                    curve_shift: -2.5,
                },
            ]
        );
    }

    #[test]
    fn quarter_chart_bins_average_temperature_readings_and_keep_empty_bins_blank() {
        let date = NaiveDate::from_ymd_opt(2026, 10, 9).unwrap();
        let start = local_midnight(date).unwrap();
        let history = History {
            room_temperatures: vec![
                TemperatureSample {
                    timestamp: start + 60,
                    temperature_c: 20.0,
                },
                TemperatureSample {
                    timestamp: start + QUARTER_SECONDS + 60,
                    temperature_c: 21.0,
                },
            ],
            control_values: vec![
                ControlValueSample {
                    timestamp: start + 60,
                    curve_shift: -3.5,
                },
                ControlValueSample {
                    timestamp: start + QUARTER_SECONDS + 60,
                    curve_shift: -4.5,
                },
            ],
            ..History::default()
        };
        let points =
            chart_points_for_period(&history, "day", Some("2026-10-09"), start + 24 * 60 * 60)
                .unwrap();
        assert_eq!(points[0].room_temperature_c, Some(20.0));
        assert_eq!(points[1].room_temperature_c, Some(21.0));
        assert_eq!(points[2].room_temperature_c, None);
        assert_eq!(points[0].curve_shift, Some(-3.5));
        assert_eq!(points[1].curve_shift, Some(-4.5));
        assert_eq!(points[2].curve_shift, None);
    }

    #[test]
    fn merges_price_updates_by_quarter_timestamp() {
        let mut prices = vec![PricePoint {
            timestamp: 900,
            price_eur_per_kwh: 0.1,
        }];
        merge_prices(
            &mut prices,
            vec![
                PricePoint {
                    timestamp: 900,
                    price_eur_per_kwh: 0.2,
                },
                PricePoint {
                    timestamp: 1800,
                    price_eur_per_kwh: 0.3,
                },
            ],
        );
        assert_eq!(prices.len(), 2);
        assert_eq!(prices[0].price_eur_per_kwh, 0.2);
        assert_eq!(prices[1].timestamp, 1800);
    }

    #[test]
    fn classifies_prices_into_lowest_and_highest_quarters() {
        let prices: Vec<_> = (0..96)
            .map(|index| PricePoint {
                timestamp: index * QUARTER_SECONDS,
                price_eur_per_kwh: index as f64 / 100.0,
            })
            .collect();
        assert_eq!(price_position(&prices, 0), Some(PricePosition::Cheap));
        let mut middle_prices = prices.clone();
        middle_prices[0].price_eur_per_kwh = 0.48;
        assert_eq!(
            price_position(&middle_prices, 0),
            Some(PricePosition::Middle)
        );
        let mut expensive_prices = prices.clone();
        expensive_prices[0].price_eur_per_kwh = 0.95;
        assert_eq!(
            price_position(&expensive_prices, 0),
            Some(PricePosition::Expensive)
        );
        assert_eq!(price_position(&prices[..7], 0), None);
    }

    #[test]
    fn applies_selected_control_strength_with_comfort_and_curve_limits() {
        assert_eq!(
            target_curve_shift(
                ControlMode::Light,
                PricePosition::Cheap,
                21.0,
                -4.0,
                -8.0,
                8.0
            ),
            -3.5
        );
        assert_eq!(
            target_curve_shift(
                ControlMode::Normal,
                PricePosition::Expensive,
                21.0,
                -4.0,
                -8.0,
                8.0
            ),
            -5.0
        );
        assert_eq!(
            target_curve_shift(
                ControlMode::Strong,
                PricePosition::Cheap,
                21.0,
                -4.0,
                -5.0,
                8.0
            ),
            -2.0
        );
        assert_eq!(
            target_curve_shift(
                ControlMode::Strong,
                PricePosition::Cheap,
                22.0,
                -4.0,
                -8.0,
                8.0
            ),
            -4.0
        );
        assert_eq!(
            target_curve_shift(
                ControlMode::Strong,
                PricePosition::Expensive,
                20.0,
                -4.0,
                -8.0,
                8.0
            ),
            -4.0
        );
    }

    #[test]
    fn emergency_heating_uses_hysteresis_and_ignores_missing_readings() {
        assert!(emergency_heating_active(false, Some(18.9)));
        assert!(emergency_heating_active(true, Some(19.9)));
        assert!(emergency_heating_active(true, None));
        assert!(!emergency_heating_active(true, Some(20.0)));
        assert!(!emergency_heating_active(false, Some(20.0)));
    }

    #[test]
    fn parses_control_modes_and_escapes_dashboard_status() {
        let request: UiControlRequest =
            serde_json::from_str(r#"{"active":true,"mode":"strong"}"#).unwrap();
        assert!(request.active);
        assert_eq!(request.mode, ControlMode::Strong);
        assert_eq!(
            escape_html("<danger & \"quotes\">"),
            "&lt;danger &amp; &quot;quotes&quot;&gt;"
        );
    }

    #[test]
    fn writes_dashboard_with_chart_and_control_ui() {
        let path = std::env::temp_dir().join(format!(
            "rust-viessman-dashboard-test-{}.html",
            std::process::id()
        ));
        let points = [ChartPoint {
            timestamp: 0,
            energy_kwh: Some(0.25),
            price_cents_per_kwh: Some(3.765),
            room_temperature_c: Some(20.5),
            curve_shift: Some(-3.0),
        }];
        let control = UiControlState {
            status: "Ohjaus pysäytetty".to_owned(),
            ..UiControlState::default()
        };

        write_dashboard_atomic(&path, &points, 1_800, &control).unwrap();
        let html = fs::read_to_string(&path).unwrap();
        fs::remove_file(&path).unwrap();

        assert!(html.contains("curve_shift"));
        assert!(html.contains("price_cents_per_kwh"));
        assert!(html.contains("room_temperature_c"));
        assert!(html.contains("Lämmityskäyrän siirto"));
        assert!(
            html.find("context.strokeStyle = '#27845b'")
                < html.rfind("context.strokeStyle = '#1769aa'")
        );
        assert!(html.contains("context.arc(x(index), yControl(point.curve_shift), 4"));
        assert!(html.contains("Huonelämpötila"));
        assert!(html.contains("Spot-hinta"));
        assert!(html.contains("Käynnistä ohjaus"));
        assert!(html.contains("Pysäytä ja palauta asetukset"));
        assert!(html.contains("api/control"));
        assert!(html.contains("sensor-reading"));
        assert!(html.contains("Tämä ei ole sähköteho"));
        assert!(!html.contains("forcedNormal"));
        assert!(html.contains("id=\"history-period\""));
        assert!(html.contains("value=\"previous_week\""));
        assert!(html.contains("value=\"previous_month\""));
        assert!(html.contains("value=\"previous_year\""));
        assert!(html.contains("type=\"date\""));
    }
}
