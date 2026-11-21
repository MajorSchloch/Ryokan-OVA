# Welcome

**Ryokan OVA** helps you build and maintain a self-hosted anime library. Follow the releases you want, set your audio, subtitle, and quality requirements, and connect your download client. Ryokan searches for monitored episodes, checks the downloaded files, and imports the ones that meet your requirements into your media library.

[Install with Docker](install.md){ .md-button .md-button--primary }
[Build a full stack](stack-builder.md){ .md-button }

Ryokan OVA is an independent fork of [Ryokan](https://github.com/johnthreekay/Ryokan). It uses AniList to organize seasons, films, and side stories into franchises.

## What you can do with it

- **Track anime from AniList or MAL.** Link your account and Ryokan auto-adds shows you mark as watching. Or add them manually from Ryokan's search.
- **Search multiple sources at once.** Ryokan ranks results from built-in Nyaa search and compatible indexers in one list. It can also watch RSS feeds and accept autobrr announcements through a webhook.
- **Pick the best release automatically.** A quality profile, [TRaSH-Guides](https://trash-guides.info)-compatible Custom Formats (the same scoring rules Sonarr uses), and optional [SeaDex](https://releases.moe) picks (a community-curated list of best anime releases) decide what gets grabbed.
- **Set a quality target.** Choose a minimum and a target resolution, with your audio and selectable subtitle requirements. With **Minimum + good enough** selected, Ryokan checks replacement files and stops automatic upgrades once they meet the target.
- **See what's airing this week.** A built-in [Calendar](calendar.md) shows upcoming episodes for the shows in your library, as a list or a month grid. You can also subscribe to it from your phone or laptop's calendar app.
- **Use the download client you already have.** qBittorrent, Deluge, Transmission, rTorrent, SABnzbd, or NZBGet. Run more than one at once and Ryokan routes per-grab.
- **Land files in your library automatically.** Hardlink (default; keeps the torrent seeding), copy, or move.
- **Import the anime you already have.** Point the [import wizard](manual-import.md) at a folder; it matches each series on AniList and lands the files in place.
- **Undo a deletion.** With a recycle bin configured, deleted episodes and series folders wait there until you restore or purge them.
- **Back up and restore from the UI.** Snapshot the database and encryption key from System → Backup, on a schedule if you like, and restore by uploading the archive.
- **Plug into Seerr.** Ryokan exposes a Sonarr/Radarr-compatible API so Seerr requests anime the same way it asks Sonarr for TV.

## Get started

- **Already run a download client?** Start with the Ryokan container and connect it to your client. [Docker installation](install.md)
- **Starting with a new server?** Generate a Compose file with Ryokan, your client, and optional services such as Jellyfin and Seerr. [Stack builder](stack-builder.md)
- **Have an anime library already?** Preview how Ryokan will match existing files. [Import an existing library](manual-import.md)
- **Ready to add your first release?** Follow a complete setup from installation to first import. [Quick start](quick-start.md)

## When something's wrong

- **[Troubleshooting](troubleshooting.md)**: concrete diagnostic steps for the most common "why didn't this work" cases.
- **[FAQ](faq.md)**: how Ryokan compares to Sonarr, multi-user, manga support, API access, backup.
