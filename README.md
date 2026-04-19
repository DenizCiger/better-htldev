# htldev

Offline-first search CLI for htl.dev — scrape, index, and search your school's documentation from the terminal.

## Install

**Linux / macOS**
```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/DenizCiger/better-htldev/releases/latest/download/htldev-installer.sh | sh
```

**Windows (PowerShell)**
```powershell
irm https://github.com/DenizCiger/better-htldev/releases/latest/download/htldev-installer.ps1 | iex
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
