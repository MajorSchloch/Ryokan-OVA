# Automation baseline

The goal is reliable daily use. Common releases can download automatically.
An uncertain source stays on hold for review through the existing file picker.
A hold is a supported exception, not a successful download.

Keep the configurable minimum and target requirements, per-title language
overrides, source checks, evidence budget, and saved download selections.
Further matching and resolver changes need evidence of a recurring problem
across several titles.

## Stabilisation release

Episode deletion and recycle restore run as complete owned tasks. A browser
disconnect does not interrupt the file move, catalog updates, history, or
client bookkeeping. Restore retains its import locks until the operation ends.
Existing HTTP and HTMX responses stay the same.

This covers page reloads and disconnects. Process-crash recovery is separate.

## Fixed verification

- Reuse the existing acquisition, subtitle, target-stop, identity, source
  manifest, and download ownership tests and live evidence.
- Abort delete and restore requests during forced cross-filesystem copies.
  Verify bytes, companion files, recovery entries, catalog, history, client
  cleanup, and restore locks.
- Run focused tests during development. Run formatting, all-target Clippy,
  the build, and the full suite on the final revision, plus required CI.
- Use one implementation owner, one focused reviewer, one PR, and one
  deployment of the merged immutable image. Reuse the reviewed deployment
  process and check preservation of jobs, proofs, recovery entries, and settings.
- Audit one naturally scheduled upgrade run. Report imports, target-met skips,
  holds, and errors separately. Keep one compact operational handoff with the
  deployed revision, evidence links, and known limits.

Close this milestone when those checks pass and the scheduled run has an
explained outcome. Record isolated title cases below. Data-loss risks,
incorrect imports, requirements bypasses, and broken bookkeeping block closure.

## Exceptions and follow-up work

- **Continuous-numbered packs:** some sources still need manual review.
  Live automatic acquisition from an unseen continuous-numbered source remains
  unverified. Keep ambiguous sources held and preserve existing job selections.
- **Seed accepted releases:** add a configurable torrent policy that seeds
  releases that pass the user's requirements. Distinguish rejected files from
  acceptable files that remain below the upgrade target. Apply per-title
  language overrides. Define safe handling for a shared torrent with accepted
  and rejected files, incomplete verification, and tracker seed obligations.
  Preserve library files and other jobs. No seeding policy changes ship in
  this stabilisation release.
- **Library and navigation:** plan separately after this milestone closes.
