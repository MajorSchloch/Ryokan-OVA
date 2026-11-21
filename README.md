# Ryokan OVA

[![CI](https://github.com/MajorSchloch/Ryokan-OVA/actions/workflows/rust.yml/badge.svg?branch=main)](https://github.com/MajorSchloch/Ryokan-OVA/actions/workflows/rust.yml?query=branch%3Amain)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)

Ryokan OVA is a fork of [Ryokan](https://github.com/johnthreekay/Ryokan). It uses the AniList entry and episode number as the library identity. A release can use a different number for the same episode.

Use `ghcr.io/majorschloch/ryokan-ova:ova` to follow the builds from `main`. The same builds are available as `:main`. For an existing library, review the [migration procedure](docs/migration-preview.md) before you change the image. An image update alone cannot move files or repair old episode assignments.

## Upstream project

A self-hosted anime PVR written in Rust. Searches indexers for releases, scores them by quality, and sends them to your download client from a single web UI. Supports qBittorrent, Deluge, Transmission, rTorrent/ruTorrent, and SABnzbd.

I built this because Sonarr doesn't always work well for anime. The RSS sync for currently airing shows works just fine, but downloading season batches of shows that've finished airing almost always hangs the interactive search. Sonarr's anime mode searches each episode individually by absolute number, so a finished season means dozens of separate searches per indexer instead of one batch grab.

## Documentation

- [Install with Docker](docs/install.md)
- [Build from source](docs/from-source.md)

## Status

Actively developed. Expect occasional bugs. See [Releases](https://github.com/johnthreekay/Ryokan/releases) for version-to-version changes.

## Contributing

Bug reports, feature requests, and PRs are welcome. Open PRs against `main`. The checks run `cargo fmt`, Clippy with warnings treated as errors, and the test suite. The build prerequisites and code conventions are in [`CLAUDE.md`](CLAUDE.md).

```bash
git clone https://github.com/MajorSchloch/Ryokan-OVA.git
cd Ryokan-OVA
cargo run            # serves on 0.0.0.0:8978, creates data/ryokan.db
```

## Security

Please report security issues privately. See [SECURITY.md](SECURITY.md).

## License

Ryokan is licensed under [GPL-3.0-or-later](LICENSE).

Third-party crate notices (MIT / Apache-2.0 / BSD / ISC) are bundled in [`licenses/THIRD_PARTY_LICENSES.html`](licenses/THIRD_PARTY_LICENSES.html).
