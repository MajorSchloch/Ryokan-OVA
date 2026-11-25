# Calendar

Ryokan's calendar shows what's airing for the shows in your library. It lives at `/calendar` in the top nav, between Library and Wanted. There's also a subscription feed if you'd rather see new episodes inside Apple Calendar, Google Calendar, or Thunderbird alongside the rest of your week.

## Views

Choose **Day**, **Week** or **Month**. Day and week use an episode list; month uses a calendar grid.

Use **Previous**, **Today** and **Next** in every view. In Day view, Previous opens yesterday directly. Use **Go to date** for a specific date. The selected view, anchor date and filters remain in the address, so browser Back and Forward restore them.

Day buckets use UTC, as before. Episode times use your browser's local timezone. The page states this distinction. Empty windows and unavailable schedule data have separate messages.

## Upcoming anime

Select **Upcoming anime** to browse announced titles, including titles outside your library. The background job checks up to 200 upcoming AniList entries by popularity every 12 hours. A sequel has its own card. Cards show a provider rating and a franchise link when the saved metadata supplies them.

Exact dates appear in calendar order. The **Announced** section shows a month, season, year, or **Date TBA**, depending on what AniList supplies. A premiere date has no time zone.

Combine **Title**, **Format**, and **Library**, then select **Apply filters**. **Open in library** opens the tracked entry. **Add to library** opens the release setup page. Filters use saved data.

The page shows the time of the last successful refresh. If a refresh fails, Ryokan keeps that snapshot and retries after 15 minutes. The iCal feed continues to show library episodes.

## What you'll see on each episode

- The series cover thumbnail, title, and provider rating when available.
- The episode number (E01, E02, ...). Each anime "season" in Ryokan is its own series with its own E1, E2 numbering, so an episode of *Re:Zero 4th Season* shows as E01 of that series, not S04E01 of the franchise.
- A **Premiere** badge (in the list) or a small **★** (in the grid) on E01 of any series, so you can spot where a new cour starts.
- Whether the episode is **Monitored**, including individual episode overrides.
- **Available**, **Download / import pending**, **Held for review**, **Missing**, **Upcoming**, **Unmonitored**, or **Unknown**. Select the label for its explanation and the last recorded automatic check.

Available means a managed library file covers the episode. It does not claim that the file passed its language or quality checks. Missing requires an aired, monitored episode, a complete file scan, and no pending download. An unavailable library mount shows Unknown. Browsing does not start searches or media probes.

Click an episode to jump to its series page.

## Filtering

Combine **Title**, **Monitoring**, and **Availability** in any view, then select **Apply filters**.

- **Title** matches titles, translations, and saved aliases. For example, "attack on titan" finds *Shingeki no Kyojin*.
- **Monitoring** uses the setting for each episode, including individual overrides.
- **Availability** includes **Held for review**, **Missing**, and **Download / import pending**.

Date navigation and view changes keep the filters. **Reset filters** keeps the current range and date.

## Subscribing from your calendar app

Click the **iCal Link** button in the page header. The modal:

1. Asks you to pick an API key with the `calendar` scope (create one on **Settings → API Keys** if you don't have one yet).
2. Builds the full subscription URL. The key is part of the URL, so treat it like a password.
3. Has a Copy button to put the URL on your clipboard.

Then in your calendar app:

- **Apple Calendar**: File → New Calendar Subscription, paste the URL.
- **Google Calendar**: Other calendars → + → From URL, paste the URL.
- **Thunderbird**: New Calendar → On the Network → iCalendar (ICS), paste the URL.

The feed defaults to the next 30 days. The URL you copy already ends in `?apikey=...`, so add options with `&`: `&days=N` for a wider or narrower window (capped at 90), `&monitored=true` to only see monitored series.

## How fresh is the data?

Air times come from AniList. Ryokan refreshes them every 12 hours and stores its own copy, so the calendar page doesn't have to call AniList every time you load it. Two things to know:

- A newly-added series shows up within seconds. Ryokan grabs its air times right after you add it, not on the next 12-hour refresh.
- If AniList updates an episode's air time, you'll see the change within 12 hours. If you want it sooner, go to **System → Scheduled Tasks**, find **Episode air-date refresh**, and click **Run now**.

## Series Ryokan adds via MAL

A few series in your library may have come from MAL instead of AniList. These are usually older or obscure titles that AniList doesn't have a record for. Those won't show up on the calendar, because the calendar's air-time data only comes from AniList. Most shows are on AniList and won't be affected.

---

*Last updated: 2026-11-10.*
