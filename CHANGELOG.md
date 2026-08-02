# Changelog

All notable changes to Furumi are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.5] - 2026-08-02

### Added

- Command history navigation with the Up and Down arrow keys.
- A copy-friendly plain terminal view for connection tickets, device and Jam
  invites, and track-sharing links.
- A configurable permanent music directory with write validation and an
  optional safe migration of existing Furumi-managed files.
- Queue reordering for one track or a `Shift+V` selection with `Alt+K` and
  `Alt+J`.
- Reproducible Nix/devenv tooling with Rust 1.97 and ALSA development files.

### Changed

- Sequential `a` actions now build one ordered play-next block instead of
  reversing independently inserted tracks.
- Federated artist images and release covers are stored beside permanent music
  in an `Artist/Release` directory tree.

### Fixed

- Running a development build next to another Furumi instance no longer lets
  an MPRIS name collision disable terminal raw mode and freeze keyboard input.
- `nix develop` now keeps devenv state outside the read-only Nix store and
  isolates Rust 1.97 build artifacts from other toolchains.
- Music-directory validation and migration now reject overlapping changes,
  resolve canonical paths, and produce Windows-portable managed filenames.

[Unreleased]: https://gt.hexor.cy/ab/furumi_tui/compare/v0.2.5...HEAD
[0.2.5]: https://gt.hexor.cy/ab/furumi_tui/compare/v0.2.4...v0.2.5
