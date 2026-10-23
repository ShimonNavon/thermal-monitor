# Thermal Monitor

Live heat and load for my whole setup: the MacBook Pro and both Kubernetes nodes, in one dark dashboard.

**Dashboard:** http://ubuntu03:30100/ (tailnet) · **Gallery:** https://projects.navonsimon.com/

![Dashboard](docs/dashboard.png)

## What it shows

Each machine reports CPU/GPU temperature, load averages, memory and disk usage, uptime, core count, clock speed, fan RPMs and individual thermal zones. The Mac adds GPU and fan sensors from `macmon`; the servers read Linux sysfs, hwmon and `/proc`.

```
MacBook Pro (launchd, every 15 s) ──┐
                                    ├──►  thermal-monitor (axum)  ──►  dashboard + JSON API
ubuntu03  (DaemonSet, every 15 s) ──┤     stores latest + 3 min history
ubuntu04  (DaemonSet, every 15 s) ──┘     serves :30100 on the tailnet
```

- `serve` — the collector/API/dashboard. In-memory only: latest reading per host plus a rolling history (180 samples).
- `push` — samples the local machine and posts a reading. Runs from launchd on macOS and as a Kubernetes DaemonSet on the cluster.
- `read` — prints one local sample as JSON.
- `/agent/binary` — the running binary serves itself; the DaemonSet pulls it at startup, so agents don't need the private registry.
- Only one server replica by design: history lives in memory and resets on rollout.

## Endpoints

| Method | Path            | Purpose                                     |
|--------|-----------------|---------------------------------------------|
| GET    | `/`             | Dashboard                                   |
| GET    | `/api/temps`    | Latest reading per host + history           |
| POST   | `/api/report`   | Agent push (`x-thermal-token` required)     |
| GET    | `/agent/binary` | Current agent binary                        |
| GET    | `/healthz`      | Health probe                                |

## Run locally

```sh
cargo run -- serve                 # http://localhost:8080
cargo run -- read                  # one local sample
cargo run -- push --url http://localhost:8080/api/report --once
```

Server configuration: `BIND` (default `0.0.0.0:8080`), `THERMAL_TOKEN`. Without a token the report endpoint is open. The Mac collector expects [`macmon`](https://github.com/vladkens/macmon) on `PATH`.

## Deploy

The image is built on `ubuntu02` (Docker), pushed to the cluster registry on `ubuntu04` through an SSH tunnel, then rolled out with `kubectl`.

```sh
# once: create the push token (kept out of git)
kubectl -n thermal-monitor create secret generic thermal-token \
  --from-literal=token="$(openssl rand -hex 24)" --dry-run=client -o yaml | kubectl apply -f -

./deploy/deploy.sh            # build + push + rollout (server and DaemonSet)
```

- `Deployment` on **ubuntu04** (the image registry only listens on its loopback), NodePort **30100**.
- `DaemonSet` on every node. It downloads the binary from the server and mounts the host root read-only for accurate disk usage.

## Mac collector

```sh
brew install macmon
cargo build --release
cp target/release/thermal-monitor bin/thermal
mkdir -p ~/.config/thermal-monitor
printf '%s' '<token>' > ~/.config/thermal-monitor/token
chmod 600 ~/.config/thermal-monitor/token
```

`~/Library/LaunchAgents/com.simon.thermal-monitor.plist` runs:

```
bin/thermal push --url http://ubuntu03:30100/api/report \
  --token-file ~/.config/thermal-monitor/token --interval 15
```

```sh
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.simon.thermal-monitor.plist
```
