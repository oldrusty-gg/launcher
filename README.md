# oldrusty launcher

The Windows launcher of Roses (crate `roses-launcher`, `roses-launcher.exe`):
import your ISO once, then play. Roses is an unofficial, free,
non-commercial recreation of a 2003 PlayStation 2 card-strategy game that
you play from your own disc; the game itself, how to get a release and how
to play are in the [rust-yugi-dotr](https://github.com/oldrusty-gg/rust-yugi-dotr)
repository.

The launcher:

- **Import ISO**: runs the importer (`roses.exe import-game`) on an ISO you
  made from your own PAL disc, into a new folder in `roses-data\` beside the
  launcher, then the game's import check (`roses.exe verify-import`). It
  contains no game data and downloads nothing.
- **Play**: starts the game (`roses-bevy-scene.exe --game <game.json>`) with
  the newest complete import.
- **Settings**: re-import, the data, saves and logs folders, the controller
  layout.
- **Profile**: online accounts (sign up, log in, change password) on the
  server named in `online.json`.

The launcher, `roses.exe` and `roses-bevy-scene.exe` must be in one folder;
the release package from the `rust-yugi-dotr` repository puts them together.

## Building

This repository uses no game crate. It builds against the
[platform-sdk](https://github.com/oldrusty-gg/platform-sdk) repository
(`platform-sdk`: the game manifest and launcher-game contract;
`platform-net`: the Profile page's account connection), checked out beside
it (see [docs/REPOSITORIES.md](docs/REPOSITORIES.md)). Everything about Roses
is in its manifest, `games/roses.json`; the Profile page's tests against a
real server live in the `rust-yugi-dotr` repository (`roses-server`).

```powershell
cargo build --release -p roses-launcher
$env:ROSES_MUTE = '1'; cargo test --workspace
```

## Legal

Roses is an independent fan project, not affiliated with, endorsed,
sponsored or approved by Konami, Konami Digital Entertainment, Studio Dice
or Kazuki Takahashi. Yu-Gi-Oh! and Yu-Gi-Oh! The Duelists of the Roses are
trademarks of their respective owners, named only to describe
compatibility. Rights holders: open an issue in this repository, or use
[GitHub's DMCA process](https://docs.github.com/en/site-policy/content-removal-policies/dmca-takedown-policy).
The project has no e-mail contact; security problems go through
[SECURITY.md](SECURITY.md).

## Licence

Dual-licensed under either of

- the MIT licence ([LICENSE-MIT](LICENSE-MIT)), or
- the Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)),

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in the work by you, as defined in the
Apache-2.0 licence, shall be dual-licensed as above, without any additional
terms or conditions.
