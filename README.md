# unloot

Sort a folder of phone photos into one directory per item, sized for Vinted,
and write a CSV of what it thinks each item is.

You point it at a folder of photos — the ones you shot standing on the bed with
six things in it — and it works out where one item stops and the next begins,
because that is the only part that needs eyes. Everything else is mechanical:
downscale, strip metadata, compress, name a folder, fill a spreadsheet.

```
input/
├── PXL_20261004_150740746.jpg      →  Dog toy Pupjoy fluffy plush ring and bone white and orange/
├── PXL_20261004_150745194.jpg          photo_01.jpg
├── PXL_20261004_150751450.jpg          photo_02.jpg
├── P_20250524_091413.jpg               photo_03.jpg
├── P_20250524_091445.jpg           →  Wallet leather with red star black/
└── …                                   photo_01.jpg … photo_05.jpg
                                    →  summary_261019_010623.csv
```

Each output photo is re-encoded: the long edge is capped at 1280 px, the
orientation is baked into the pixels, and every last byte of EXIF and XMP is
gone — no GPS, no camera serial, no author tag, no thumbnail of your driveway
for some stranger to poke at. Each file is squeezed under 300 KiB (Vinted's own
advice is to keep uploads small; 1280 px on a phone screen is not the place you
notice the difference).

## Why a model at all

Consecutive photos are *usually* the same item, and the file names sort in
capture order — but "usually" is not good enough, because the gap between the
last photo of a wallet and the first photo of a pair of jeans is often four
seconds. Time is a hint, not a verdict. So unloot asks a vision model to look
at the photos and say where the items change, and asks it a second time, per
item, what the item actually is. Your phone's timestamps are included in the
question as context. The model decides.

That means unloot needs an image-capable endpoint (self-hosted llama.cpp or any
OpenAI-compatible server). It talks to `/v1/chat/completions`, asks for JSON,
and never sends anything anywhere else.

## Build it

```sh
cargo build --release          # -> target/release/unloot
```

Rust 1.85 or newer (edition 2024). The binary links libjpeg through the `image`
crate, so it does **not** shell out to anything. `ffmpeg` is not required to run
unloot — it is in the container image because the container is a pleasant place
to do other photo and video chores, and because a development box that can
transcode something is worth having.

A small static-ish binary is a one-liner with [cross](https://github.com/cross-rs/cross)
or `cargo build --release --target x86_64-unknown-linux-musl`.

## Run it

```sh
export UNLOOT_LLM_URL=http://192.168.137.134:8010/v1
unloot --input-dir ./input --output-dir ./input     # same folder = work in place
```

Start with `--dry-run` the first time. It does everything except write and
delete, including the encodes, so the numbers it prints are the real ones:

```sh
unloot --input-dir ./input --output-dir ./input --dry-run
```

### As a container

```sh
docker build -t unloot .
docker run --rm -v "$PWD/input:/workdir" \
  -e UNLOOT_LLM_URL=http://192.168.137.134:8010/v1 \
  unloot
```

The image sets `UNLOOT_INPUT_DIR=/workdir` and `UNLOOT_OUTPUT_DIR=/workdir`, so
mounting one folder in is the whole job.

### With docker compose

```sh
UNLOOT_LLM_URL=http://192.168.137.134:8010/v1 docker compose run --rm unloot
```

`docker-compose.yml` mounts `./input` at `/workdir` and passes a handful of
`UNLOOT_*` variables through from your shell.

### With podman quadlet

See [`deploy/podman/README.md`](deploy/podman/README.md) — a one-shot unit, a
watcher unit, and a volume, including how to run it on a timer instead.

## Options

**Flags take precedence over environment variables, which take precedence over
the defaults below.** Every option has both a `--flag` and an `UNLOOT_UPPERCASE`
variable; the program carries a default for all of them except `--llm-url`,
which it will refuse to run without. Booleans accept `yes/true/1` and get a
matching `--no-<name>` form, which beats an environment variable set to yes.

`unloot --print-options-md` prints this table from the same table the parser is
built from, so it cannot drift. `--print-config` shows what the program settled
on for this invocation and where each value came from (`Flag`, `Env`, or
`Default`), with the API key redacted.

| Flag | Environment | Default | Meaning |
| --- | --- | --- | --- |
| `--input-dir` | `UNLOOT_INPUT_DIR` | `.` | Directory scanned for photos (top level only; sub-directories are never touched) |
| `--output-dir` | `UNLOOT_OUTPUT_DIR` | `.` | Directory for item sub-directories and the CSV. Same as input-dir means work in place |
| `--llm-url` | `UNLOOT_LLM_URL` | **required, no default** | OpenAI-compatible base URL, e.g. http://192.168.137.134:8010/v1 (no default, must be set) |
| `--llm-model` | `UNLOOT_LLM_MODEL` | `` | Model name. Empty asks the endpoint's /models for the one loaded model |
| `--llm-api-key` | `UNLOOT_LLM_API_KEY` | `` | Bearer token for the LLM endpoint. Empty sends no Authorization header |
| `--llm-timeout` | `UNLOOT_LLM_TIMEOUT` | `180` | Wall-clock timeout per LLM request, seconds |
| `--llm-retries` | `UNLOOT_LLM_RETRIES` | `4` | Attempts per LLM request. Retries cover network errors, HTTP 5xx, unparseable replies, and a reasoning model that used all its token budget on thinking instead of answering — the budget is doubled and then dropped on those retries, so one more attempt than you think you need is cheap insurance |
| `--llm-image-edge` | `UNLOOT_LLM_IMAGE_EDGE` | `768` | Photos are sent to the LLM downscaled to this long edge, to keep prompts small (0 sends originals) |
| `--mock-llm` / `--no-mock-llm` | `UNLOOT_MOCK_LLM` | `false` | Offline mode: answer from a built-in guess, or from --mock-llm-file when given (no network at all) |
| `--mock-llm-file` | `UNLOOT_MOCK_LLM_FILE` | `` | With --mock-llm: JSON file of the form {"calls": [ ... ]}, replayed in call order: one boundary answer ({"images": [{"id", "verdict", "subject"}]}) then one description per item |
| `--max-long-edge` | `UNLOOT_MAX_LONG_EDGE` | `1280` | Longest edge of output images, pixels (downscaled only, never upscaled) |
| `--max-bytes` | `UNLOOT_MAX_BYTES` | `307200` | Hard ceiling per output image, bytes (300 KiB) |
| `--jpeg-quality-min` | `UNLOOT_JPEG_QUALITY_MIN` | `62` | Lowest JPEG quality tried when hunting for max-bytes |
| `--jpeg-quality-max` | `UNLOOT_JPEG_QUALITY_MAX` | `92` | Highest JPEG quality tried when hunting for max-bytes |
| `--window` | `UNLOOT_WINDOW` | `6` | Photos per LLM boundary-detection call |
| `--overlap` | `UNLOOT_OVERLAP` | `2` | Photos shared between consecutive windows so item boundaries survive window edges (must be < window) |
| `--gap-hint` | `UNLOOT_GAP_HINT` | `90` | Seconds of capture gap treated as weak evidence of a new item (hint only, the LLM decides) |
| `--keep-source` / `--no-keep-source` | `UNLOOT_KEEP_SOURCE` | `false` | Keep the full-resolution source photos after processing |
| `--resume` / `--no-resume` | `UNLOOT_RESUME` | `true` | Skip photos already recorded in the state ledger |
| `--describe-dirs` / `--no-describe-dirs` | `UNLOOT_DESCRIBE_DIRS` | `true` | Describe folders already sitting in the output directory and add the missing rows to the summary; their photos are read, never changed |
| `--dry-run` / `--no-dry-run` | `UNLOOT_DRY_RUN` | `false` | Plan and print the CSV only; write and delete nothing |
| `--best-effort` / `--no-best-effort` | `UNLOOT_BEST_EFFORT` | `false` | Continue when a window's boundary call fails, cutting that window at its largest capture gap |
| `--watch` / `--no-watch` | `UNLOOT_WATCH` | `false` | Keep running and process photos as they appear |
| `--watch-interval` | `UNLOOT_WATCH_INTERVAL` | `15` | Watch mode: seconds between scans of the input directory |
| `--settle` | `UNLOOT_SETTLE` | `20` | Watch mode: a photo must be this many seconds old (and unchanged) before it is picked up |
| `--name-template` | `UNLOOT_NAME_TEMPLATE` | `{category} {brand} {details} {color} size {size}` | Item directory name. Placeholders: {category} {brand} {details} {color} {size}. Empty fields drop their label word, so a belt with nothing else to say is 'Belt', not 'Belt size' |
| `--unbranded` | `UNLOOT_UNBRANDED` | `No brand` | Value written in the brand column when no brand is legible in the photos |
| `--na-label` | `UNLOOT_NA_LABEL` | `N/A` | Value written when a column does not apply to the item |
| `--currency` | `UNLOOT_CURRENCY` | `USD` | Price currency (USD prints weight in oz, anything else in g) |
| `--csv-name` | `UNLOOT_CSV_NAME` | `summary_{ts}.csv` | Report file name, relative to output-dir. {ts} is local time as yymmdd_hhmmss |
| `--state-file` | `UNLOOT_STATE_FILE` | `.unloot/state.json` | Resume ledger (relative paths resolve against output-dir) |
| `--jobs` | `UNLOOT_JOBS` | `0` | Parallel workers; 0 uses min(cpus, 4) |
| `--log` | `UNLOOT_LOG` | `info` | Log level: error, warn, info, debug, trace |

The container image overrides two of these with its own `ENV` lines —
`UNLOOT_INPUT_DIR=/workdir` and `UNLOOT_OUTPUT_DIR=/workdir` — which is the
usual default *except* inside the container.

### Exit codes

`0` did something useful (or, with `--dry-run`, planned something useful);
`2` the options were unusable; `1` the run failed partway. Nothing is deleted
unless its compressed replacement is already on disk and has been read back.

## What you get out

One directory per item, `photo_01.jpg` onwards, and one CSV:

| Column | Notes |
| --- | --- |
| `name` | The directory name, which is also the listing title you should probably use |
| `category`, `brand`, `color`, `size` | `No brand` when no brand is legible in the pixels; `N/A` when the field does not apply |
| `price (USD)` | The model's guess at a second-hand price, rounded to whole units — nobody splits a cent over a jumper. Renamed for `--currency` |
| `weight (oz)` | A wild guess in grams, converted. Renamed to `weight (g)` unless the currency is USD |

That is the whole row: the listing and nothing else. How many photos an item
ended up with, what the originals were called, and how big the JPEGs came out are
all still there — in the folder itself and in `.unloot/state.json` — they are
just not worth a column.

The name template is
`{category} {brand} {details} {color} size {size}`: category first, because that
is how Vinted's own search and browse behaves. Placeholders that come back empty
take their label with them, so a wallet — which has no size, and no legible brand
— is `Wallet leather with red star black`, not
`Wallet   leather with red star black size`. `{details}` is the model's
two-to-six-word answer to "which one is this": the material, the pattern, the
mechanism, or what it is for, read off the item or its box. `{brand}` stays empty
unless a brand name is actually legible in a photo — a logo the model merely
thinks it recognises is not enough — and the CSV then says `No brand`. Brand
capitalisation is fixed on the way in, so "pupjoy" becomes "Pupjoy".

If you would rather have shorter folders, `--name-template "{category} {color} size {size}"`
drops the description, and `--name-template "{category} size {size}"` drops the
colour too.

Two more files appear, both hidden: `.unloot/state.json` (the ledger of what has
been processed, and where each photo ended up) and `.unloot.lock` while a run is
in progress. They are what make a second run cheap and two runs at once
impossible.

### A shelf that is already sorted

If the output directory already holds item folders — sorted by hand last
winter, or by unloot on another machine — they are described rather than
rearranged: their photos are read, their names are taken as the truth, and the
summary gains the rows it was missing.

```sh
unloot --input-dir ~/Pictures/listings --output-dir ~/Pictures/listings
#  INFO describing folders that are already sorted; their photos are read, not changed folders=7
```

- Nothing inside an existing folder is written, renamed, or re-encoded. The
  photos keep their own names (`IMG_4138.jpg` stays `IMG_4138.jpg`), because
  renaming them would be a surprise, and the name column of the CSV is the
  folder's name verbatim.
- A folder is only described if the summary does not already list it, so
  running this twice a day costs nothing. Delete the summary and the next run
  describes the whole shelf again — which is the way to get a summary back in
  the current format, or after you have renamed folders to taste.
- Loose photos in the same directory are still sorted as usual, so a half-done
  shelf converges in one run: the folders gain rows, the new photos gain
  folders.
- `--no-describe-dirs` (or `UNLOOT_DESCRIBE_DIRS=no`) confines unloot to loose
  photos, if you keep other people's directories in there.

## Safety, such as it is

- **The originals are deleted by default.** Pass `--keep-source` if that is not
  what you want. The reasoning is that these are photos of things you are about
  to post online: the full-resolution copy is the one with your location and
  your face in the background, and keeping it around only invites you to upload
  it by accident. Nothing is removed until its replacement is written and
  verified.
- **`--dry-run` first**, honestly. It is cheap and it shows you the folders and
  the sizes before anything moves.
- **A folder unloot did not create is read-only.** An item directory that was
  already there keeps its name, its file names, its bytes and its timestamps —
  the end-to-end test snapshots the tree before and after and insists they match.
  Only loose photos get moved, and only once their replacement is on disk.
- **A wrong boundary is the worst case**, and it happens: two similar jumpers,
  one folder. The photos are all still there and the CSV lists which source file
  went where, so moving `photo_04.jpg` into a new folder is a ten-second fix.
- **`--best-effort`** lets a run finish when the model is unreachable, cutting
  windows on the largest capture gap instead and saying loudly which items were
  guessed. Without it, unloot stops and tells you, rather than inventing
  boundaries.
- **Prices are guesses.** They come from the model, in the currency you asked
  for, and were not checked against anything. Use them as a starting number, not
  as advice.

## Talking to the model

The request is a normal OpenAI-compatible `POST /v1/chat/completions` with
`response_format: {"type": "json_object"}`, `stream: false`, and photos as base64
data URLs downscaled to `--llm-image-edge` (768 px) so a six-photo window is not
a 12 MB prompt. `--llm-model` empty asks the endpoint's `/models` for the one
model it has loaded, which is the usual shape for a self-hosted server.

Two things worth knowing if a run seems slow or obstinate:

- **Reasoning models spend the whole budget thinking.** llama.cpp returns HTTP
  200 with an empty answer and `finish_reason: length` when that happens. unloot
  notices and escalates: double the token budget, double it again, then stop
  sending a cap at all and let the server's own limit decide. The escalation is
  remembered for the rest of the run, so the first unlucky answer does not make
  every later call pay for it. That is why a first run can take a minute per
  window, and why `--llm-retries` is 4 rather than 2 — the last attempt is the
  one that usually comes back uncapped.
- **The photo count per boundary call is `--window` (6).** Bigger windows are
  cheaper in requests and worse at boundaries; `--overlap` (2) is what keeps a
  boundary that falls near the edge of a window from being missed.

## Development

```sh
cargo test                    # unit + end-to-end, no network
cargo run -- --help
```

The end-to-end tests drive the real binary against synthesised photos with
`--mock-llm`, so they are honest about files on disk and do not need an
endpoint. `--mock-llm-file` replays a scripted JSON fixture
(`{"calls": [ ... ]}`, one boundary answer then one description per item) when
you want a specific outcome.

## Known limits

- Item boundaries can be wrong, as above. Similar items photographed
  back-to-back are the hard case.
- Only the top level of `--input-dir` is scanned, and only files with a photo
  extension; that is also what makes it safe to point at a folder that already
  has item directories in it.
- `.heic` and `.heif` are not decoded. Android sends JPEGs; iPhone originals
  need converting first (`ffmpeg -i IMG_1.HEIC IMG_1.jpg`) or sharing them as
  JPEG from the phone.
- `--watch` notices a photo once it is `--settle` seconds old; it does not
  follow a growing file, it just waits.
