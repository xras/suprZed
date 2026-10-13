# Building suprZed

suprZed is available as source code only. No prebuilt binaries or GitHub releases are planned.
Builds and tests run locally. GitHub Actions will be disabled for the public fork.

## Get the source

Clone this repository. Follow the Windows dependency setup.
The Linux and macOS links are upstream references. Those platforms are outside this fork's supported scope:

- [Windows](../docs/src/development/windows.md)
- [Linux](../docs/src/development/linux.md)
- [macOS](../docs/src/development/macos.md)

These are upstream Zed guides. Use your suprZed checkout wherever a guide asks you to clone or build Zed.
Upstream download links install Zed.

## Build and run

Run these commands from the repository root after installing the required dependencies.

Build the app and command-line launcher:

```sh
cargo build --locked -p zed -p cli
```

Run the development app:

```sh
cargo run --locked -p zed
```

Build an optimised app and launcher:

```sh
cargo build --locked --release -p zed -p cli
```

Keep the source checkout while using a Windows debug build. It loads shaders and assets from that checkout.
Back up your editor data before testing a new build.

## Versions

suprZed shows its own version alongside the upstream Zed version.
About, startup logs and system diagnostics show the fork version and source commit.
The app and command-line launcher also accept `--version`.

Direct Cargo builds do not need Python or the maintainer's private build tools.
A local build has a version such as `0.1.0+local.abcdef1234`.
Uncommitted changes can add `.dirty`. A source archive without Git uses `0.1.0+local.unknown`.

The version is embedded during compilation. Source status is sampled when the build script runs.
A documentation edit does not force a rebuild to refresh that label.

The maintainer's numbered builds use a private counter. Debug and optimised builds share it.
Building does not publish a release.

Zed's package version, release channel and update-service fields retain their upstream values.
The suprZed version does not change the data profile or select a separate update service.

## Windows icons

Windows icons follow the upstream release channel by default. A release build alone does not select the yellow icon.
Set `SUPRZED_WINDOWS_ICON` before building to choose `main` for yellow or `debug` for cyan.
This setting changes the icon. It does not change the release channel or data profile.

For example, in PowerShell:

```powershell
$env:SUPRZED_WINDOWS_ICON = 'main'
cargo build --locked --release -p zed -p cli
Remove-Item Env:SUPRZED_WINDOWS_ICON
```

Main and debug builds currently share a Windows application identity when they use the same release channel.
Windows may group their taskbar buttons under one icon.

## Platform coverage

suprZed currently supports Windows only.
Linux and macOS are not supported or validated by this fork.
The upstream build guides do not establish support for every fork feature on those platforms.
