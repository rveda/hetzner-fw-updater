# hetzner-fw-updater — specification

Describes the behaviour of `src/main.rs`.

## Purpose

Command-line tool that manages the rules of Hetzner Cloud firewalls. It is
particularly useful for keeping rules that grant access from a home network up
to date: the `{{my-public-ip}}` placeholder in the config file is replaced with
the caller's current public IP addresses.

This document describes exactly what the program does: the order of the
steps, every message and exit code, and edge cases. For an introduction, the
config file format and the commands' user-facing behaviour, see the
[README](README.md); this document does not repeat it.

## Invocation

```
hetzner-fw-updater download <config file> <firewall name>
hetzner-fw-updater update <config file>
hetzner-fw-updater check <config file>
```

All commands need a Hetzner Cloud API token in the `API_TOKEN` environment
variable.

## Config file

The format is described in the README. In addition:

- the file must be a JSON array of objects; `firewall-name` is a string and
  must be unique within the file;
- `"rules": null` is the same as a missing `rules` key;
- rules are kept as generic JSON and not validated by the tool, except for the
  placeholder; the API validates them when they are sent;
- other keys in an entry are kept when `download` saves the file.

## Current behaviour

1. Check the arguments. On error, print the message and the usage to stderr and
   exit with code 1:
   - fewer than two arguments: usage only;
   - unknown command: `unknown command "<cmd>"`;
   - `download` without a firewall name:
     `download requires a firewall name`;
   - extra arguments: `too many arguments for <command>`.
2. Read and parse the config file. It must be a JSON array, and no firewall name
   may appear twice (`firewall "<name>" is configured more than once`). If this
   fails, print `could not read config file <path>: <error>` and exit with
   code 1. Exception: `download` treats a missing file as an empty array
   and creates the file.
3. Check the entries whose rules the command may upload: every entry for
   `update`, the entries with a `health-endpoint` for `check`, none for
   `download`. If this fails, print `invalid config file <path>: <error>` and
   exit with code 1. Nothing is looked up or changed. The errors are:
   - `update` with no entries: `contains no firewalls`;
   - an entry without a `rules` key (or with `null`):
     `firewall "<name>": has no "rules" key`;
   - an invalid placeholder (see below): `firewall "<name>": invalid address
     "<entry>" in <field>: {{my-public-ip}} must be the whole entry`.
4. If `API_TOKEN` is unset or empty, print
   `API_TOKEN environment variable is not set` and exit with code 1.
5. Run the command.

### download

1. Fetch the named firewall (`GET /v1/firewalls?name=<name>`). If no firewall
   has that name, fail with `firewall "<name>" not found`.
2. If the config file has an entry for the firewall, replace its `rules` with
   the rules the API returned. Otherwise append a new entry and print
   `Added firewall "<name>" to <path>`. Rules are saved exactly as the API
   returns them, in the API's key order. Other entries are not changed.
3. Write the config file back:
   - indented with 4 spaces, ending in a newline;
   - key order in each entry is `firewall-name`, `health-endpoint` (if set),
     `rules`, then any other keys in their original order;
   - written to `<path>.<pid>.tmp` (`<pid>` is the process ID, so concurrent
     runs do not share it), synced to disk and then renamed over the original,
     and the directory is synced after the rename. A failed write or a crash
     does not damage the existing file. If the write fails, the temporary file
     is removed.
4. Print `Downloaded <n> rules from firewall "<name>" to <path>` and exit with
   code 0.

If the API call or the write fails, the error is printed to stderr, the config
file is left unchanged, and the exit code is 2.

### update

The firewalls are processed in the order they appear in the file.

The entries have already been checked (step 3 above), so no firewall is changed
if any entry is invalid.

1. If any rule uses the `{{my-public-ip}}` placeholder, look up the public
   addresses once and replace the placeholder in all entries (see below). If
   the lookup fails, print the error and exit with code 2; no firewall is
   changed.
2. For each firewall:
   1. Fetch it by name to get its ID and current rules.
   2. If the current rules are the same as the configured ones (see *Comparing
      rules*), print `Firewall "<name>" already has the configured <n> rules`
      and go to the next firewall without changing it.
   3. Send its rules as the firewall's complete rule set
      (`POST /v1/firewalls/<id>/actions/set_rules`). All existing rules are
      replaced, so rules not in the config are removed. Hetzner validates the
      rules and reports invalid ones as an API error.
   4. Wait until Hetzner has applied the rules (see *Waiting for the rules to
      be applied*).
   5. Print `Uploaded <n> rules to firewall "<name>"`.

   If any of these steps fails, print `firewall "<name>": <error>` and continue
   with the next firewall.
3. If any firewall failed, print `failed to update <n> of <total> firewalls` and
   exit with code 2. Otherwise exit with code 0.

#### Comparing rules

Two rule sets are the same if they contain the same rules, ignoring the order
of the rules, of the keys in a rule, and of the addresses in `source_ips` and
`destination_ips`. A key whose value is `null` or an empty list counts as
missing, because the API returns e.g. `"description": null` and
`"destination_ips": []` for keys the config may leave out. Any other
difference, such as an address written differently from how the API returns
it, counts as a change, so the rules are at worst uploaded needlessly.
Placeholders are replaced before comparing.

#### Waiting for the rules to be applied

`set_rules` returns actions that Hetzner completes in the background. Each
action is polled (`GET /v1/firewalls/actions/<id>`) every second until its
status is `success`. If its status is `error`, the firewall fails with
`applying the rules failed (<code>): <message>`; if it has not finished after
60 s, with `rules were not applied within 60 s`.

#### `{{my-public-ip}}` placeholder

The README describes what the placeholder is replaced with. In detail:

- the addresses are looked up via `https://ip.hetzner.com`, once over IPv4 and
  once over IPv6;
- if one lookup fails, `warning: could not determine IPv4 address: <error>`
  (or IPv6) is printed and only the other address is used; if both fail, the
  command fails with `could not determine any public IP address`;
- the lookup only happens when at least one rule uses the placeholder, and at
  most once per run, even when `check` updates several firewalls (a failed
  lookup is not retried);
- after the lookup, `Replacing {{my-public-ip}} with <ips>` is printed, the
  addresses separated by `, `;
- the replacement addresses take the placeholder's position in the list, IPv4
  first;
- the placeholder combined with other text (for example
  `{{my-public-ip}}/24`) is rejected when the config is checked, before any
  lookup;
- the placeholder is not replaced in other fields such as `description`.

**Warning:** an explicit empty list (`"rules": []`) is uploaded and removes all
rules from that firewall.

### check

Checks whether each firewall with a `health-endpoint` lets the caller through,
and updates the firewall's rules when it does not. The firewalls are processed
in the order they appear in the file. Entries without a `health-endpoint` are
skipped.

For each firewall with a `health-endpoint`:

1. Probe the endpoint (see below).
   - Reachable: print `firewall "<name>": <url> is reachable` and go to the
     next firewall.
   - Failed: print `firewall "<name>": <url>: <error>` to stderr, count the
     firewall as failed, and go to the next firewall without changing it.
   - Blocked: print `firewall "<name>": <url>: <reason>; updating rules`, where
     `<reason>` is `timed out` or `connection refused`, and continue.
2. Update this firewall's rules as `update` does, including replacing the
   `{{my-public-ip}}` placeholder (printing `Replacing {{my-public-ip}} with
   <ips>` the first time the addresses are looked up) and printing `Uploaded <n> rules to firewall "<name>"`. Only this
   firewall is changed, and the command waits until Hetzner has applied the
   rules. If the firewall already has the configured rules, it is not what
   blocks the endpoint: print `Firewall "<name>" already has the configured
   <n> rules` and `error: firewall "<name>": <url>: <reason>, but the firewall
   already has the configured rules` to stderr, count the firewall as failed,
   and go to the next firewall without probing again. If the address lookup fails, an API call fails or the rules are not
   applied, print `firewall "<name>": <error>` to stderr, count the firewall as
   failed, and go to the next firewall.
3. Wait 3 seconds after the rules are applied and probe the endpoint again.
   - Reachable: print `firewall "<name>": <url> is reachable after the update`.
   - Blocked: print `error: firewall "<name>": <url> after updating the rules:
     <reason>` to stderr and count the firewall as failed.
   - Failed: print `error: firewall "<name>": <url> after updating the rules:
     <error>` to stderr and count the firewall as failed.

After all firewalls:

- if no entry has a `health-endpoint`, print `no firewall in the config file
  has a health-endpoint` (this is not an error);
- if any firewall failed, print `<failed> of <checked> health checks failed`
  and exit with code 2, where `<checked>` is the number of entries with a
  `health-endpoint`. Otherwise exit with code 0.

Unlike `update`, an empty config file is not an error. Entries without a
`health-endpoint` are not checked for config errors.

#### Probing an endpoint

The endpoint is sent a `GET` request over IPv4 and over IPv6 at the same time,
each with a 15 s timeout for the whole request.

| Result for one address family | Outcome |
|---|---|
| Status 200 | Reachable |
| Request timed out | Blocked (`timed out`) |
| Connection refused | Blocked (`connection refused`) |
| Any other status, DNS failure, TLS error, unreachable network, … | Failed (`unexpected status <code>` or the error message) |

The results are combined:

1. If either family is reachable, the endpoint is reachable.
2. Otherwise, if either family is blocked, the endpoint is blocked, with that
   family's reason (IPv4's if both are blocked).
3. Otherwise the endpoint failed, with the error message
   `IPv4: <error>; IPv6: <error>`.

The families are probed separately because the HTTP client reports only the
error for the last address it tried. If both were tried in one request, an
IPv4 timeout followed by an immediate IPv6 "network unreachable" would be
reported as a failure, not as blocked.

A refused connection is treated as blocked because a firewall that rejects
packets (instead of dropping them) refuses the connection. It is also what
happens when the service behind the endpoint is not running; in that case the
firewall already has the configured rules, so it is not changed and the
health check fails.

## Available building blocks

Helpers in `main.rs` used by the commands:

- `public_ip(family)` looks up the caller's public IPv4 or IPv6 address via
  `https://ip.hetzner.com`. It sends `User-Agent: curl/8.0`, because the service
  returns HTML to other user agents.
- `same_rules(a, b)` compares two rule sets as described in *Comparing rules*.
- `Client::get_firewall(name)` returns the firewall's ID and its current rules
  (`GET /v1/firewalls?name=<name>`).
- `Client::set_rules(id, rules)` replaces the firewall's entire rule set
  (`POST /v1/firewalls/<id>/actions/set_rules`) and waits until it is applied.
- API errors are reported as `API error (<code>): <message>` or
  `API request failed: <status>: <body>`. The timeout is 30 s per call.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Command succeeded |
| 1 | Usage error, unreadable or invalid config file (including duplicate firewall names, entries without `rules` and invalid placeholders), or missing `API_TOKEN` |
| 2 | An API, network or file-write failure, rules that were not applied, or a failed health check |
