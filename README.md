# Forge Bounties

A native Rust desktop app for finding paid programming issues on GitHub, GitLab.com, and Codeberg. It opens the local cache without network access. Use **Refresh from forges** for discovery and status checks. The desktop app picks up new pages from both manual refreshes and the background service while it is open.

The default view lists recently observed open, unassigned issues with an explicit reward amount. The details pane shows the original reward text and difficulty label. “Open” reflects forge issue state and assignees at the last check. Confirm funding, eligibility, assignment, and payment terms with the maintainer before starting. Difficulty comes from issue labels. Reward ranking groups by currency and orders amounts within each group. A bare dollar sign has its own group. Ambiguous or conflicting amounts are excluded.

Discovery searches each forge’s public issue index for bounty, reward, and payout terms. Each forge has its own worker, continuation, and rate-limit cooldown. The request budget remains six search requests and eight rechecks per forge per refresh, plus up to 18 anonymous GitLab verification requests. Three requests are reserved for first-page discovery and three for historical continuations. GitHub and Codeberg recent probes use a seven-day update window. GitLab probes the head of its instance search index. GitHub rotates targeted bounty labels, paid-issue wording, provider markers, and reverse-engineering searches alongside the broader terms. Historical searches that reach the budget resume on the next eligible refresh. GitHub requests 100 results per page and splits over-limit searches into smaller creation-date ranges. Provider results, pagination progress, and cooldowns are persisted in SQLite. The first refresh imports any existing `cache.json` and leaves that file in place. Open and assigned observations become stale after 24 hours. Status reports pending continuations, incomplete responses, date ranges the provider cannot fully serve, and failures. The status includes scanned, duplicate, missing-reward, ambiguous, invalid, and accepted counts. GitHub Search and Core quotas have separate persisted cooldowns. Recognized GitLab account and anonymous quotas are tracked separately. Codeberg and responses with an unspecified throttle use a provider-wide cooldown. Server-requested backoff applies before another request is sent.

API references for [GitHub](https://docs.github.com/en/rest/search/search#search-issues-and-pull-requests), [GitLab](https://docs.gitlab.com/api/issues/), and [Codeberg's Forgejo API](https://forgejo.org/docs/latest/user/api/usage/).

## Build and validate on your laptop

Use a current stable Rust toolchain (minimum Rust 1.89), crates.io access for initial dependency resolution, and an X11 or Wayland session with OpenGL support for the GUI. On CachyOS/Arch, install missing native packages before building. Skip the package command if they are already present.

```sh
sudo pacman -S --needed base-devel pkgconf libxkbcommon wayland libx11 libxcursor libxi libxrandr mesa xdg-utils
rustc --version
cargo generate-lockfile  # First build only, if Cargo.lock is absent
cargo test --locked
cargo build --release --locked

scratch=$(mktemp -d)
BOUNTY_HOME="$scratch" target/release/forge-bounties --status
BOUNTY_HOME="$scratch" target/release/forge-bounties --sync
BOUNTY_HOME="$scratch" target/release/forge-bounties --status
BOUNTY_HOME="$scratch" target/release/forge-bounties
```

Check each forge's status for successful site-wide searches. The provider status distinguishes anonymous global search, optional authenticated global search, and partial public repository coverage. Wait for any reported cooldown before retrying. Verify that a returned bounty has reward evidence and a working source link, then reopen the GUI with networking disabled and check that the cached bounty still loads.

## Activate

Use the same user for the GUI and user timer. The cache defaults to `${XDG_DATA_HOME:-$HOME/.local/share}/forge-bounties`. Set `BOUNTY_HOME` to an absolute path for an isolated instance. If using an override with the timer, set the same variable in its environment file and the GUI's launch environment.

```sh
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/forge-bounties"
install -d -m 700 "$data_dir" "$HOME/.config/forge-bounties"
install -d "$HOME/.local/bin" "$HOME/.local/share/applications" "$HOME/.config/systemd/user"

if test -x "$HOME/.local/bin/forge-bounties"; then
  cp -p "$HOME/.local/bin/forge-bounties" "$HOME/.local/bin/forge-bounties.previous"
fi
install -m 755 target/release/forge-bounties "$HOME/.local/bin/forge-bounties.new"
mv -f "$HOME/.local/bin/forge-bounties.new" "$HOME/.local/bin/forge-bounties"
sed "s|^Exec=.*|Exec=\"$HOME/.local/bin/forge-bounties\"|" ops/forge-bounties.desktop \
  > "$HOME/.local/share/applications/forge-bounties.desktop"
"$HOME/.local/bin/forge-bounties"
```

GitLab.com works without credentials by indexing recently active public projects and paging through their public issues. Optionally set `GITLAB_TOKEN` with `read_api` for GitLab’s authenticated global index. The app verifies those issue results anonymously before caching them. GitHub optionally accepts `GITHUB_TOKEN` for public search. Codeberg tries anonymous global search by default. You may set `CODEBERG_TOKEN` in the same file or environment with a public-only token limited to reading issues. Authenticated candidates are checked anonymously before caching. Those checks share Codeberg’s existing request budget. The GUI and timer can both read the optional GitLab token from `~/.config/forge-bounties/credentials.env` with mode 600, one `GITLAB_TOKEN=value` per line. For GitHub, set `GITHUB_TOKEN` in the launching shell or in the timer’s environment file. To create or edit the file locally:

```sh
touch "$HOME/.config/forge-bounties/credentials.env"
chmod 600 "$HOME/.config/forge-bounties/credentials.env"
${EDITOR:-vi} "$HOME/.config/forge-bounties/credentials.env"
```

When a global issue endpoint is unavailable, the provider scans public repositories from its official catalog. Repositories are discovered automatically and prioritized by activity among similarly scanned repositories. SQLite stores the catalog continuation and each repository’s issue continuation separately from global issue search. Catalog results and issue scans are bounded samples. The status reports repository coverage separately and honors any active cooldown. No repository list or credentials are needed for basic discovery.

Existing fiat amounts and explicit USDC, USDT, SOL, ETH and BTC amounts are supported. Crypto amounts retain their written decimal precision. The app does not infer a blockchain network or perform transactions.

Optional background refresh runs every hour with up to two minutes of jitter. To update an existing timer, reinstall the units below, reload systemd, and run `systemctl --user restart forge-bounties.timer`.

```sh
install -m 644 ops/forge-bounties.{service,timer} "$HOME/.config/systemd/user/"
systemctl --user daemon-reload
systemctl --user start forge-bounties.service
systemctl --user enable --now forge-bounties.timer
systemctl --user list-timers forge-bounties.timer
journalctl --user -u forge-bounties.service -n 30 --no-pager
"$HOME/.local/bin/forge-bounties" --status
```

## Inspect rejected rewards offline

Run `forge-bounties --diagnostics` to export newline-delimited JSON from the local SQLite cache. The export contains original and normalized reward evidence, the rejection reason, conflicting declarations where available, and GitLab issue decoding errors. Identical observations are grouped with first-seen and last-seen timestamps and a count. Previous versions retained only aggregate rejection counts. Detailed evidence is captured starting with the first refresh after upgrade. Authenticated search candidates contribute reward evidence only after anonymous verification.

## Recovery and rollback

A failed source leaves cached issues available, with a status message and any recorded retry time. Inspect `--status` and the journal, correct the configuration or wait for the cooldown, then start the service again. The GUI and timer share a file lock. Each completed page and its continuation commit in one SQLite transaction. Back up `cache.sqlite3` after stopping refreshes. Preserve the original `cache.json` if present. If the cache cannot be decoded, stop refreshes and preserve the damaged copy before restoring a known-good backup.

Close the GUI before rolling back. The following commands disable the timer, stop refresh, and restore the previous binary if one exists. The cache and configuration remain available for recovery.

```sh
systemctl --user disable --now forge-bounties.timer
systemctl --user stop forge-bounties.service
# Export current issues for a previous binary that reads cache.json.
umask 077
data_dir="${BOUNTY_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/forge-bounties}"
if test -e "$data_dir/cache.sqlite3"; then
  if test -e "$data_dir/cache.json"; then
    legacy_backup=$(mktemp "$data_dir/cache.legacy.XXXXXX")
    cp -p "$data_dir/cache.json" "$legacy_backup" || exit 1
  fi
  legacy_export=$(mktemp "$data_dir/cache.rollback.XXXXXX") || exit 1
  "$HOME/.local/bin/forge-bounties" --export-legacy > "$legacy_export" || exit 1
  mv -f "$legacy_export" "$data_dir/cache.json" || exit 1
fi
if test -x "$HOME/.local/bin/forge-bounties.previous"; then
  mv -f "$HOME/.local/bin/forge-bounties.previous" "$HOME/.local/bin/forge-bounties"
else
  mv -f "$HOME/.local/bin/forge-bounties" "$HOME/.local/bin/forge-bounties.disabled"
  mv -f "$HOME/.local/share/applications/forge-bounties.desktop" \
    "$HOME/.local/share/applications/forge-bounties.desktop.disabled"
fi
```
