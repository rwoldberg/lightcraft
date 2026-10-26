# Canon CRW: where the record is

The Canon CRW (CIFF) decoder and its complete clean-room derivation record (starting assumptions,
plan, source log, container and payload notes, derivation log, the frozen format, verification,
levels and white-balance notes, scripts, logs and the frozen table JSON) live in craft-libs:
[`crates/crw/clean-room/`](https://github.com/storytold/craft-libs/tree/main/crates/crw/clean-room)
(`../craft-libs/crates/crw/clean-room/` on a dev machine). LightCraft uses the crate `craft-crw`
through the adapter `crates/raw/src/vendor/crw.rs`.

What stays here is LightCraft's own colour work on these cameras:

- [`10-chart-calibration.md`](10-chart-calibration.md): the EOS D60 colour matrices fitted to
  ColorChecker captures, implemented in `crates/raw/src/chart_table.rs`.
- [`logs/chart-captures.tsv`](logs/chart-captures.tsv): names, sizes and SHA-256 of the chart
  frames (no image content).

The capture statistics those documents cite (`08-color.md`, `09-private-corpus.md`, `sources.md`)
are in the craft-libs record.
