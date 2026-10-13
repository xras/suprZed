# suprZed

suprZed is a personal fork of [Zed](https://zed.dev/). It adds features for working with AI tools and several projects.
It is an unofficial fork. It is not an official Zed release.

Zed is a fast editor that I enjoy using for coding and working with agents. Thank you to the Zed team and contributors
for building it and making its source available. Their work is the foundation of suprZed.

## Goals

The goal of these customisations is to fit suprZed to the way I use AI tools:

- Keep several agent conversations open beside code.
- Use Agent CLI terminals and Zed's built-in agent tools in the same workspace.
- Return to the same CLI conversations after restarting the editor.
- Switch between projects and see which project each agent is working on.
- See when an agent needs attention and clear the alert when I return to it.

I use these features in my daily work. I aim to keep them compatible with future Zed updates.
I also add small quality-of-life improvements. I aim to keep changes small and simple to maintain.

## Why a fork?

A fork lets me build these features and use them in my daily work. Other people can try them too.
Working examples may help explain the ideas. They may also support proposals to add some of them to Zed.

[Zed PR #64059](https://github.com/zed-industries/zed/pull/64059) explored agent conversations as regular tabs.
It was closed before the design was settled. The maintainers asked for new features to be discussed before implementation.
This fork gives me a place to try related ideas.

## Features

See [suprZed-features.md](suprZed-features.md) for the features and how to use them.
See [suprZed-features-implementation.md](suprZed-features-implementation.md) for a map of the code changes.

## Building

suprZed is available as source code only. You must build it yourself.
No prebuilt binaries or GitHub releases are planned.

See the [build guide](suprzed-docs/building.md) for instructions.
suprZed currently supports Windows only. Linux and macOS are not supported or validated by this fork.

## Issues and contributions

Bug reports and feature suggestions are welcome through this repository's Issues tab.
Please check existing issues before opening a new one.

External pull requests are not accepted initially. Code changes and merges remain under my control.
Discussions and the wiki will be disabled for the public fork. See [CONTRIBUTING.md](CONTRIBUTING.md) for reporting guidance.

## Maintenance

This is a personal development fork. I update it when time permits.
I merge upstream Zed changes manually. I test those updates locally before merging them into main.
GitHub Actions will be disabled for the public fork.

There is no fixed release schedule or commitment to provide community support.

## Upstream project and licences

See the [original Zed README](README-zed.md) for upstream project information.
See the [original contribution guide](CONTRIBUTING-zed.md) for contributions to Zed itself.
Upstream download links install Zed.

The upstream licence files and notices remain in this repository.
