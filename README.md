# hetzner-fw-updater

A command-line tool that manages the rules of
[Hetzner Cloud firewalls](https://docs.hetzner.com/cloud/firewalls/) from a
JSON config file.

You describe each firewall's rules in one file. The tool can:

- **download** a firewall's current rules from Hetzner into the file,
- **update** every firewall in the file with the rules written there,
- **check** that a service behind a firewall can be reached, and update the
  firewall if it cannot.

Rules can use the `{{my-public-ip}}` placeholder, which is replaced with the
public IP addresses of the machine running the tool. This is useful when your
home or office address changes: run `check` regularly (for example from a systemd timer) and access
is restored when the address changes.

## Building

You need a Rust toolchain (edition 2024, Rust 1.85 or later).

```sh
cargo build --release
```

The binary is written to `target/release/hetzner-fw-updater`.

To build and install it into `~/.cargo/bin`, which `rustup` puts on your
`PATH`, run:

```sh
cargo install --path .
```

The examples in [Running check regularly](#running-check-regularly) expect the
binary in `/usr/local/bin`, where every user and service can find it.

## API token

Every command needs a Hetzner Cloud API token with **read & write** access,
passed in the `API_TOKEN` environment variable. Create one in the Hetzner Cloud
Console under *Security → API tokens* in the project that holds the firewalls.

```sh
export API_TOKEN=your-token-here
```

## Config file

The config file is a JSON array with one entry per firewall:

```json
[
    {
        "firewall-name": "web-server",
        "health-endpoint": "https://web.example.com/health",
        "rules": [
            {
                "description": "ssh from my current address",
                "direction": "in",
                "protocol": "tcp",
                "port": "22",
                "source_ips": ["{{my-public-ip}}"],
                "destination_ips": []
            },
            {
                "description": "https from anywhere",
                "direction": "in",
                "protocol": "tcp",
                "port": "443",
                "source_ips": ["0.0.0.0/0", "::/0"],
                "destination_ips": []
            }
        ]
    }
]
```

| Key | Required | Description |
|---|---|---|
| `firewall-name` | Yes | Name of the firewall in Hetzner Cloud. Each name may appear only once in the file. |
| `rules` | For `update` and `check` | The firewall's complete rule set, in the format of the Hetzner Cloud API [`set_rules`](https://docs.hetzner.cloud/) action. `download` fills this in. |
| `health-endpoint` | No | URL that `check` requests to test whether the firewall lets you through. |

Rules are passed to the API as written; Hetzner validates them when they are
uploaded. Other keys in an entry are kept when `download` rewrites the file.

The easiest way to start is to create the firewall in the Hetzner Cloud
Console, then run `download` to write its rules into a new config file.

### The `{{my-public-ip}}` placeholder

An entry in `source_ips` or `destination_ips` that is exactly
`{{my-public-ip}}` is replaced, when rules are uploaded, with the public
addresses of the machine running the tool: `<ipv4>/32` and `<ipv6>/128`. The
addresses are looked up via `https://ip.hetzner.com`. If the machine has only
one of IPv4 or IPv6, only that address is used.

```json
"source_ips": ["{{my-public-ip}}", "10.0.0.0/8"]
```

becomes, for example:

```json
"source_ips": ["203.0.113.7/32", "2001:db8::1/128", "10.0.0.0/8"]
```

The placeholder must be the whole entry; `{{my-public-ip}}/24` is rejected. The
config file itself keeps the placeholder. Note that running `download`
afterwards replaces it with the real addresses, because it writes the rules as
Hetzner returns them.

## Usage

```
hetzner-fw-updater download [<config file>] <firewall name>
hetzner-fw-updater update [<config file>]
hetzner-fw-updater check [<config file>]
```

If no config file is given, `/etc/hetzner-fw/config.json` is used.

### download

```sh
hetzner-fw-updater download config.json web-server
```

Fetches the current rules of the named firewall and writes them into the config
file, replacing that entry's `rules`. If the firewall is not in the file yet, it
is added; if the file does not exist, it is created. Other entries are left
unchanged.

### update

```sh
hetzner-fw-updater update config.json
```

Uploads the configured rules to every firewall in the file.

- Each firewall's rules are **replaced completely**: rules that are not in the
  config file are removed from the firewall. An empty list (`"rules": []`)
  removes all rules.
- All entries are checked before anything is changed. If any entry has no
  `rules` key or an invalid placeholder, no firewall is updated.
- A firewall that already has the configured rules is left unchanged. The
  order of rules and addresses does not matter.
- The command waits until Hetzner has applied each firewall's new rules, so
  when it reports success the rules are in effect.
- If updating one firewall fails, the others are still updated, and the command
  exits with an error at the end.

### check

```sh
hetzner-fw-updater check config.json
```

For each firewall that has a `health-endpoint`:

1. Sends a GET request to the endpoint, waiting up to 15 seconds.
2. If it answers with status 200, the firewall is left alone.
3. If the request **times out** or the **connection is refused**, which is
   what happens when the firewall blocks you, the firewall's rules are updated
   as with `update`. Once Hetzner has applied them, and 3 seconds later, the
   endpoint is requested again. If it still does not answer with 200, an error
   is printed.

   If the firewall already has the configured rules, the firewall is not what
   blocks you (for example, the service is not running). It is left unchanged
   and an error is printed.

Before anything is probed, the entries with a `health-endpoint` are checked
like in `update`: if one has no `rules` key or an invalid placeholder, nothing
is probed or changed.

Other failures, such as an unknown host or a status other than 200, are
reported as errors and do not change the firewall. Firewalls without a
`health-endpoint` are skipped.

The endpoint is tried over IPv4 and IPv6 at the same time. It counts as
reachable if either answers with 200.

To keep access working when your public address changes, run `check`
regularly; see [Running check regularly](#running-check-regularly).

## Running check regularly

Run `check` every few minutes so access is restored soon after your public
address changes. Keep the API token in a file that only root can read, never
in a crontab or unit file: a crontab line is readable by anyone who can list
it, and the token would show up in `ps` output while the job runs.

### systemd timer

The setup below runs `check` every 10 minutes as an unprivileged, temporary
user. It assumes the binary is in `/usr/local/bin`:

```sh
sudo install -m 755 target/release/hetzner-fw-updater /usr/local/bin/
```

1. Put the config file and the token in `/etc/hetzner-fw`. The token file is
   created empty with mode 600 first, so the token is never readable by others:

   ```sh
   sudo install -d -m 755 /etc/hetzner-fw
   sudo install -m 644 config.json /etc/hetzner-fw/config.json
   sudo install -m 600 /dev/null /etc/hetzner-fw/token.env
   sudoedit /etc/hetzner-fw/token.env
   ```

   `token.env` holds one line:

   ```sh
   API_TOKEN=your-token-here
   ```

2. Create the service, `/etc/systemd/system/hetzner-fw-check.service`:

   ```ini
   [Unit]
   Description=Check Hetzner firewall access and update the rules if blocked
   Wants=network-online.target
   After=network-online.target

   [Service]
   Type=oneshot
   # Read by systemd as root, before it switches to the temporary user.
   EnvironmentFile=/etc/hetzner-fw/token.env
   ExecStart=/usr/local/bin/hetzner-fw-updater check
   # Run as a temporary user with a read-only file system.
   DynamicUser=yes
   ProtectSystem=strict
   ProtectHome=yes
   PrivateTmp=yes
   ```

3. Create the timer, `/etc/systemd/system/hetzner-fw-check.timer`:

   ```ini
   [Unit]
   Description=Run hetzner-fw-check every 10 minutes

   [Timer]
   OnBootSec=1min
   OnUnitActiveSec=10min

   [Install]
   WantedBy=timers.target
   ```

4. Start the timer, and run the check once to see that it works:

   ```sh
   sudo systemctl daemon-reload
   sudo systemctl enable --now hetzner-fw-check.timer
   sudo systemctl start hetzner-fw-check.service
   ```

Useful commands:

| Command | Shows |
|---|---|
| `systemctl list-timers hetzner-fw-check.timer` | When the check last ran and runs next |
| `journalctl -u hetzner-fw-check.service` | The output of each run |
| `systemctl status hetzner-fw-check.service` | Whether the last run succeeded |

Notes:

- A run that exits with an error (for example a failed health check) marks
  the service as *failed*, so it shows up in `systemctl --failed` and in any
  monitoring that watches failed units.
- systemd does not start the service again while a run is still going, so slow
  runs never overlap.
- `check` only reads the config file, which is why the service can use a
  read-only file system. Run `download` by hand, not from this service.
- To use Docker instead, you need no timer: the image can run `check` on a
  schedule itself, see [Running check regularly in Docker](#running-check-regularly-in-docker).

### cron

Without systemd, source the token file in the crontab line so the token itself
is not in the crontab:

```cron
*/10 * * * * set -a; . /etc/hetzner-fw/token.env; /usr/local/bin/hetzner-fw-updater check
```

The token file must be readable by the user whose crontab this is.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Usage error, unreadable or invalid config file (including an entry without `rules` or with an invalid placeholder), or `API_TOKEN` not set |
| 2 | A firewall could not be downloaded or updated, its new rules were not applied, a health check failed, or the config file could not be written |

## Docker

The [Dockerfile](Dockerfile) builds the tool in an Alpine Rust image and copies
the binary into a small Alpine runtime image (about 6 MB).

```sh
docker build -t hetzner-fw-updater .
```

Pass the command and its arguments to `docker run` to run the tool once;
without a command, the container runs `check` every 15 minutes (see below).
Mount the directory that holds the config file at `/config`, the container's
working directory, and pass the token from your environment:

```sh
docker run --rm -e API_TOKEN -v "$PWD:/config" hetzner-fw-updater check config.json
docker run --rm -e API_TOKEN -v "$PWD:/config" hetzner-fw-updater update config.json
```

Things to know:

- **Mount the directory, not the file.** `download` saves the config file by
  writing a temporary file next to it and renaming it over the original, which
  fails when only the file is mounted.
- **`download` needs your user ID.** The container runs as `nobody`, which
  cannot write to your directory. Run `download` as yourself:

  ```sh
  docker run --rm -e API_TOKEN -v "$PWD:/config" --user "$(id -u):$(id -g)" \
      hetzner-fw-updater download config.json web-server
  ```

  `update` and `check` only read the config file and work without this.
- **IPv6.** Docker's default bridge network usually has no IPv6. Then the
  IPv6 lookup for `{{my-public-ip}}` fails, a warning is printed, and only your
  IPv4 address is put in the rules; `check` also probes over IPv4 only. On
  Linux, add `--network host` to use the host's addresses, IPv6 included.

### Running check regularly in Docker

Started without a command, the container runs `check` on
`/config/config.json` every 15 minutes until it is stopped. Keep the token in
a file that only you can read and point Docker at it:

```sh
docker run -d --name hetzner-fw-check --restart unless-stopped --network host \
    --env-file /home/me/.hetzner-fw.env -v /home/me/hetzner-fw:/config \
    hetzner-fw-updater
```

where `/home/me/.hetzner-fw.env` contains `API_TOKEN=your-token-here`.

- The first check runs at startup, and each later one 15 minutes after the
  previous one finished. Set `CHECK_INTERVAL` to another number of seconds to
  change this, e.g. `-e CHECK_INTERVAL=300`.
- A failed check is logged and the next one still runs, so the container keeps
  running. See the output with `docker logs -t hetzner-fw-check`.

## Development

```sh
cargo test
```

To try a command against your Hetzner project, run it from source with
[dev-run.sh](dev-run.sh). It asks for the API token without echoing it and
passes its arguments to the program:

```sh
./dev-run.sh check config.json
```

[specs.md](specs.md) complements this README with the details it leaves out:
the exact messages, the order of the steps, and edge cases.

### Releasing

[release.sh](release.sh) tests the project, builds and publishes the Docker
image to a registry, and tags the release in git. It needs `cargo`, `docker`,
`git` and `python3`. Commit your changes and log in to the registry first, then
pass the registry host and an optional path:

```sh
docker login registry.example.com:5000
./release.sh registry.example.com:5000/tools
```

The script stops at the first step that fails:

1. Reads the package name and version from `Cargo.toml` (via
   `cargo metadata`). The image is named
   `<registry>/hetzner-fw-updater`.
2. Stops if the working tree has uncommitted changes or untracked files, so
   the image is always built from a commit.
3. Stops if the git tag `v<version>` or the image tagged with this version in
   the registry already exists. Bump `version` in `Cargo.toml` for a new
   release, or run with `FORCE=1` to overwrite the existing tags.
4. Runs the tests (`cargo test --release --locked`).
5. Builds the Docker image, which compiles the binary in its build stage, and
   tags it `:<version>` and `:latest`.
6. Pushes both image tags.
7. Tags the current commit `v<version>` in git (an annotated tag). The tag is
   not pushed; push it with `git push origin v<version>`.

For example, version `0.1.0` is published as
`registry.example.com:5000/tools/hetzner-fw-updater:0.1.0` and `:latest`, and
the commit is tagged `v0.1.0`.

If you are not logged in, step 3 cannot see the existing image tags, so it does
not stop the release, and the push in step 6 fails.
