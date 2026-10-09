# Rust Viessmann price controller

This project contains a ViCare discovery command and an experimental local
energy/price dashboard with optional automatic heating-curve control. Control
is off by default and starts only after an explicit request from the dashboard.

ViCare may report the heat pump by an internal model ID rather than the
consumer-facing model name. For example, `CU401B_S` is treated as a Vitocal
S-series heat-pump alias for feature discovery; this ID alone does not confirm
that the exact appliance is a 222-S.

## Prerequisites

- Rust and Cargo
- A Viessmann Developer Portal client ID
- The client ID configured to allow the redirect URI
  `http://localhost:51125` (without a trailing slash)

## Run discovery

```sh
VIESSMANN_CLIENT_ID='your-client-id' cargo run
```

The command opens the Viessmann sign-in page in the default browser. After
authorization, the OAuth token is stored in the operating system's per-user
configuration directory with owner-only file permissions on Unix systems.
The token is never printed. Sign out/revoke the client from the Viessmann
account if you want to invalidate it.

At startup, the command prints the exact redirect URI it sends to Viessmann.
The default is `http://localhost:51125`. An existing `VIESSMANN_REDIRECT_URI`
environment variable overrides the default; unset it or update it if it shows
a URI different from the one registered for this Client ID.

The redirect URI must match the URI registered in the Developer Portal
exactly. If you use a different localhost callback port, set
`VIESSMANN_REDIRECT_URI` to that exact URI, for example
`http://localhost:51126`, and register that URI in the portal as well.

The output includes the device model, enabled `Smart_RoomControl` room
temperature readings and normal/reduced/comfort heating setpoints, and a
compact list of heating features and available commands. Current room readings
are printed locally; do not share them if you consider them private. OAuth
tokens are never printed and must not be added to source control.
For device features whose names mention power, energy, consumption,
production, electrical measurements, or compressors, discovery prints scalar
readings and samples from array-valued history properties, including units.
This scan includes the heat pump, room controller, and Vitoconnect device. Use
`cargo run` (without `--monitor`) to inspect these candidates. In particular,
a compressor sensor reported in percent is not a kW measurement and cannot be
integrated into kWh; identify an actual electrical power or energy feature
before using it for the consumption graph.
Discovery also directly probes documented compressor electrical-consumption
features, including `heating.compressors.0.power.consumption.dhw` and
`heating.compressors.0.power.consumption.heating`, and heat-production
features, including `heating.heat.production.dhw` and
`heating.heat.production.heating`, their `summary.*` variants, and
`currentYear.*` variants. Discovery also probes documented total/current
heating energy points, heating-rod consumption and production, and solar
production where those features are listed. Devices whose roles contain
`capability:productionReport;electric` are also probed for
`device.power.statusReport.production` and
`device.power.statusReport.consumption` to check the electrical report values.
These probes are read-only; unsupported paths are reported as unavailable.
The API catalog labels some of these as Advanced features and availability
depends on the device and controller. A `kilowatt` value alone does not prove
that a feature is instantaneous electrical consumption (it may be a nominal
capacity), so check its meaning and whether it changes over time before using
it as a power sample. Heat production in kWh is thermal output, not
electricity consumed; keep the two quantities separate in any graph.
It also uses the read-only installation-wide feature search
`POST /features/installations/{installationId}/features/filter`, requesting relevant
heating/compressor power and energy feature names and printing each result's
gateway and device identifiers. This can discover features that are exposed
for another device in the same installation.

## Run the local dashboard

```sh
cargo run --release -- --monitor
```

The monitor can run headlessly on a Raspberry Pi or another always-on Linux
server. It does not open a browser on the server. To open the dashboard from a
phone, connect the phone to the same trusted home network and visit
`http://<raspberry-pi-ip>:8787/`; on the Pi, `hostname -I` lists its LAN
addresses. `DASHBOARD_PORT` changes the port. The dashboard listens on all
network interfaces and currently has **no password or HTTPS**. Do not forward
the dashboard port from your router or expose it to the public internet: anyone
who can reach it can submit heating-control requests.

### Fresh Raspberry Pi installation

These steps assume a Raspberry Pi 3 or newer, Raspberry Pi OS Lite 64-bit, an
internet connection, and SSH access. Raspberry Pi 4 with 2 GB RAM or more is a
good fit. Use Raspberry Pi Imager to write the OS, configure a username and
password, enable SSH, and join the Pi to your home network. Ethernet and a DHCP
reservation in the router are recommended so the Pi's address stays stable.

Connect to the Pi from another computer:

```sh
ssh <user>@<raspberry-pi-ip>
```

Update Raspberry Pi OS and install the build tools:

```sh
sudo apt update
sudo apt full-upgrade -y
sudo apt install -y build-essential pkg-config curl git
```

Install Rust for the logged-in user, then load Cargo into this shell:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
rustc --version
cargo --version
```

Get the project source. Replace the placeholder with the repository URL if it
is hosted in Git; alternatively copy the project directory to the Pi:

```sh
git clone <repository-url> rust-viessman-control
cd rust-viessman-control
cargo build --release
```

Before starting the service for the first time, configure the OAuth client ID
in the Viessmann Developer Portal with the exact redirect URI
`http://localhost:51125`. Open an SSH tunnel from the computer that will open
the sign-in page (in a separate terminal and keep it running):

```sh
ssh -N -L 51125:127.0.0.1:51125 <user>@<raspberry-pi-ip>
```

Back in the Pi's SSH session, start the monitor once interactively:

```sh
export VIESSMANN_CLIENT_ID='<your-client-id>'
./target/release/rust-viessman-control --monitor
```

If the Pi has no browser, copy the Viessmann sign-in URL printed in that SSH
session into a browser on your computer. Complete the sign-in while the tunnel
is active; the callback is forwarded to the Pi. Once the monitor reports that
it is running, open `http://<raspberry-pi-ip>:8787/` on your phone, verify the
readings, and stop the interactive monitor with Ctrl+C. The OAuth token is
stored for the Pi user, so run the system service below as that same user.

To start the monitor automatically after reboot, create a root-owned
environment file for the client ID:

```sh
sudo install -m 600 /dev/null /etc/vicare-controller.env
sudo nano /etc/vicare-controller.env
```

Put this single line in the file and save it:

```text
VIESSMANN_CLIENT_ID=<your-client-id>
```

Create `/etc/systemd/system/vicare-controller.service`, replacing `<user>` in
the paths and `User` value with the Pi login name:

```sh
sudo nano /etc/systemd/system/vicare-controller.service
```

Put this in the file:

```ini
[Unit]
Description=Viessmann heat-pump price controller
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=<user>
WorkingDirectory=/home/<user>/rust-viessman-control
EnvironmentFile=/etc/vicare-controller.env
ExecStart=/home/<user>/rust-viessman-control/target/release/rust-viessman-control --monitor
Restart=on-failure
RestartSec=10

[Install]
WantedBy=multi-user.target
```

Enable and start the service, then inspect its logs:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now vicare-controller
sudo systemctl status vicare-controller
journalctl -u vicare-controller -f
```

The dashboard remains available at `http://<raspberry-pi-ip>:8787/` while the
service is running. Use `sudo systemctl stop vicare-controller` to stop it and
`sudo systemctl restart vicare-controller` after building an updated release.
Keep TCP port 8787 inside a trusted home network; do not configure router port
forwarding for it. The web dashboard itself has no authentication.

For the first sign-in on a headless Pi, start an SSH tunnel from your computer
before launching the monitor:

```sh
ssh -N -L 51125:127.0.0.1:51125 <user>@<raspberry-pi-ip>
```

In another terminal, launch the monitor on the Pi. If it cannot open a browser,
it prints a Viessmann sign-in URL. Open that URL in a browser on your computer;
the SSH tunnel forwards the OAuth callback back to the Pi. Keep the tunnel open
until sign-in completes. After authorization, the token is stored on the Pi in
its user configuration directory, so subsequent starts do not require the
browser login while the token remains valid.

The monitor requests the Viessmann `heating.compressors.0.sensors.power`
feature every five minutes for a separate diagnostic reading; a percent reading
is labeled as compressor load, not electrical power, and is not plotted as
consumption. Historical compressor sensor readings, accepted power samples,
room-temperature sensor readings, the
automatic controller's applied heating-curve shift, and prices are retained in
`power-price-history.json` in the project directory from which the monitor is
launched. Room temperature is sampled every five minutes and stored with the
timestamp reported by ViCare; repeated polls of the same Zigbee reading are not
duplicated. While automatic control is active, the current heating-curve shift
is recorded every five minutes and immediately when the controller changes it.
The monitor prints the full path at startup. If the
old Application Support history exists and
there is no project-local history yet, it is copied on startup; an existing
project-local history is never overwritten. Old samples are no longer trimmed,
so chart history survives restarts and
remains available for longer-term views. It fetches Finnish, tax-inclusive prices from
Spot-hinta.fi's `TodayAndDayForward` endpoint. The dashboard is served locally
at port 8787 on the Pi's network interfaces (override the port with
`DASHBOARD_PORT`). `DASHBOARD_HTML` changes where the generated HTML is saved.
The dashboard refreshes its chart and controller status from the local service
every 30 seconds without reloading the page.

The chart defaults to the current day in the Europe/Helsinki time zone. Choose
any previous date for a quarter-hour view, or select the previous complete
calendar week, month, or year for a summary (daily buckets for week/month,
monthly buckets for year). The chart shows the automatic controller's
heating-curve shift as a blue line, the spot price, and saved room temperature
as a green line. The curve shift is the ViCare `shift` value sent to the heating
curve command, not a measured electrical quantity; no unit is supplied by the
API. Only periods when automatic control is active have curve-shift samples.
Chart buckets average saved curve-shift and temperature samples; buckets without
samples remain blank. Price lines average available quarter-hour prices.
Daylight-saving changes are accounted for in day lengths.

The graph shows Spot-hinta.fi's `PriceWithTax` as a cents/kWh line, the saved
controller shift as a blue line, and room temperature as a °C line. Compressor
power or load is not used as a graph series. The
controller shift and room temperature use independent axes; the price has its
own scale. Sampling gaps remain blank. The shift line can help compare how the
controller responds to cheap and expensive periods, but it is not a measurement
of energy use or proof of monetary savings.
The separately documented cumulative electrical-consumption and thermal
production features are currently probed by `cargo run` discovery but are not
yet rendered in the dashboard; first confirm which of those features your
device exposes and the response units/periods.
The dashboard's **Kevyt**, **Normaali**, and **Jyrkkä** modes change the
heating-curve shift by up to ±0.5, ±1, and ±2 respectively from the value read
when control starts. Prices are ranked across the next 24 hours; the cheapest
quarter is treated as cheap and the most expensive quarter as expensive. Curve
slope is preserved. Adjustments are limited by the live ViCare shift bounds
and the 20–22 °C comfort range: at or above 22 °C cheap-price heating increases
are blocked, and at or below 20 °C expensive-price heating reductions are
blocked. The active room reading may be up to 60 minutes old to account for
infrequent Zigbee updates; a missing, disconnected, or older reading pauses
price adjustments and restores the starting curve shift.

Use **Pysäytä ja palauta asetukset** to restore the starting curve shift and
operating mode. Ctrl+C also attempts this restoration. Do not terminate the
process abruptly while control is active: recovery after a crash or power loss
is not yet persisted. The controller depends on the ViCare API and an active
room sensor; verify the appliance behavior and comfort settings before
enabling it. All values and mode constraints are checked from live ViCare
feature data before control starts. Dashboard control requests are handled
immediately; ordinary sensor and price sampling remains every five minutes
(price refresh is no more frequent than every 15 minutes).
