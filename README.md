# htldev

Offline-first search CLI for htl.dev — scrape, index, and search your school's documentation from the terminal.

## Install

**Linux (x64)**
```sh
curl -LO https://github.com/DenizCiger/better-htldev/releases/latest/download/htldev-x86_64-unknown-linux-gnu.tar.xz
tar -xf htldev-x86_64-unknown-linux-gnu.tar.xz
sudo mv htldev /usr/local/bin/
```

**macOS (Apple Silicon)**
```sh
curl -LO https://github.com/DenizCiger/better-htldev/releases/latest/download/htldev-aarch64-apple-darwin.tar.xz
tar -xf htldev-aarch64-apple-darwin.tar.xz
sudo mv htldev /usr/local/bin/
```

**macOS (Intel)**
```sh
curl -LO https://github.com/DenizCiger/better-htldev/releases/latest/download/htldev-x86_64-apple-darwin.tar.xz
tar -xf htldev-x86_64-apple-darwin.tar.xz
sudo mv htldev /usr/local/bin/
```

**Windows (PowerShell)**
```powershell
Invoke-WebRequest -Uri "https://github.com/DenizCiger/better-htldev/releases/latest/download/htldev-x86_64-pc-windows-msvc.zip" -OutFile "htldev.zip"
Expand-Archive htldev.zip -DestinationPath "$env:USERPROFILE\.htldev"
$env:PATH += ";$env:USERPROFILE\.htldev"
[Environment]::SetEnvironmentVariable("PATH", $env:PATH + ";$env:USERPROFILE\.htldev", "User")
```

Restart your terminal, then verify with `htldev --version`.

## Setup

First, scrape and index the htl.dev content (requires your HTL credentials):

```sh
htldev scrape
htldev index
```

You'll be prompted for your username and password. Credentials are stored securely in your system keyring after the first run.

## Usage

**Interactive TUI** (recommended)
```sh
htldev tui
```

**Search from the command line**
```sh
htldev search "docker compose"
htldev search "git rebase" --limit 5
htldev search "\.env" --regex
htldev search "kubernetes" --path "technologies/*"
```

**Read a document**
```sh
htldev show technologies/docker.md
htldev open technologies/docker.md   # opens in browser
```

**Keep content up to date**
```sh
htldev scrape --sync   # re-check all files for updates
htldev index           # re-index after scraping
```

**Diagnostics**
```sh
htldev doctor
```

## Options

All commands accept these global flags:

| Flag | Description |
|------|-------------|
| `--source <PATH>` | Custom path to the markdown mirror |
| `--index-db <PATH>` | Custom path to the SQLite index |
