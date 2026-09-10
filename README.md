# zmk-flasher

Downloads a ZMK firmware artifact from a GitHub Actions run and walks you
through flashing both halves of a split keyboard over its UF2 bootloader
mass-storage volume (macOS `/Volumes`).

## Build

```
cargo build --release
```

Binary ends up at `target/release/zmk-flasher`.

## Auth

GitHub's Actions artifact API always requires a token, even for public
repos. Provide one of:

- `--token <TOKEN>`
- `$GITHUB_TOKEN` environment variable
- `gh auth login` (the tool shells out to `gh auth token` as a fallback)

A classic or fine-grained PAT with `actions:read` (repo scope for private
repos) is enough.

## Usage

```
zmk-flasher \
  --url https://github.com/tdegrunt/zmk-config/actions/workflows/build.yml \
  --left-volume KEEBART --right-volume KEEBART \
  --left-firmware nice_view-corne_choc_pro_left-zmk.uf2 \
  --right-firmware nice_view-corne_choc_pro_right-zmk.uf2
```

All of the above are the defaults, so for the common case you can just run:

```
zmk-flasher
```

`--url` accepts three shapes:

- A workflow page — `.../actions/workflows/build.yml` — uses the latest run
- A specific run — `.../actions/runs/<run_id>`
- A specific artifact — `.../actions/runs/<run_id>/artifacts/<artifact_id>`

If a run has more than one artifact and none of the above pins one down,
pass `--artifact-name <substring>` to pick it.

## Flow

1. Resolves the run/artifact from `--url`, downloads and unzips it in memory.
2. Prompts you to press the bootloader button on the **left** half, waits for
   the `--left-volume` volume to appear, copies `--left-firmware` onto it,
   then waits for the volume to disappear (device rebooting into firmware).
3. Repeats the same for the **right** half with `--right-volume` /
   `--right-firmware`.

`--timeout-secs` (default 300) and `--poll-interval-ms` (default 500) control
how long/how often it polls for the volume to appear or disappear.
