# Semantic search with `--em`

This page explains how `lctr scan --em` works, what it stores, what it skips, and what to do when it
does not work. The short version is in the README under "Semantic search".

## How it works

`lctr scan <dir> --em` runs the normal filename scan first. Then it embeds the contents of the files that
can be read as text, plus the images, with EmbeddingGemma 2 (Google DeepMind, Apache 2.0). The model runs
locally through llama.cpp. Each piece of text becomes a vector of 768 numbers. Search compares the
vector of your query with the stored vectors and returns the closest chunks.

A file is split into chunks of about 1800 characters. Chunks overlap by 200 characters and break at
paragraph, line, or sentence boundaries where possible. A file has at most 128 chunks. Anything beyond
that is not embedded.

## Passes and depth

A scan embeds each file in passes. Each pass takes a range of chunks from every file that has them:

| Pass | Chunks (counted from 1) | Depth it completes |
|---|---|---|
| 1 | 1 to 8 | `quick` |
| 2 | 9 to 32 | `balanced` |
| 3 | 33 to 128 | `deep` |

`--depth` (or the `embed_depth` setting) chooses how many passes a run does. The default is `quick`.
Pass 1 covers every file before pass 2 starts, so a file of any size becomes searchable early. The
`lctr scan` output says which pass is running and what it covers.

Passes are resumable:

- Stopping a run (`q` in the galaxy screen, or Ctrl-C) keeps everything already written. Each file is written in one transaction per pass.
- Running the same command again continues at the first pass that is not finished.
- Running with a deeper `--depth` embeds only the missing chunks.
- Running with a shallower `--depth` embeds nothing and deletes nothing.
- A file that changes is embedded again from chunk 1. Its old chunks are replaced.

Measured on a 54-file test corpus (695 chunks, 332,919 tokens) on an M2 Mac under load:

| Run | Chunks stored | Files complete | Time |
|---|---|---|---|
| `quick` | 356 | 30 of 54 | 65 s |
| `balanced` | 575 | 50 of 54 | 103 s |
| `deep` | 695 | 54 of 54 | about 125 s |

The final `deep` state matched the single-pass result: all 695 chunks had the same vectors, with a
minimum cosine of 1.0000000 per chunk.

## Speed

The speed is set by the model. The GPU was busy for the whole pass in every run, at about 5 to 6 chunks
per second. Across the runs measured for this release the throughput ranged from about 1,900 to 3,400 tokens
per second. The same deep pass took 98 s once and about 125 s in the other runs; the cause was not identified. Treat the numbers as a range, not a promise. That is why `quick` is the default: the first useful results come after about a minute on this
corpus, and the rest can wait.

## The sidecar

Vectors are stored in `index.embeddings.sqlite`, next to the filename index. Three tables:

- `meta`: the model id, the vector width, and whether each root is complete to its requested depth.
- `embed_files`: for each file, its size and modified time, its status, how many chunks it has, and how
  many are embedded.
- `chunks`: for each chunk, its ordinal, its start byte, a 240-character snippet, and its vector stored as
  f16 (2 bytes per number).

The file is created with mode 0600, so other accounts on the machine cannot read it.

Disk use: the vectors take 1,536 bytes per chunk at 768 dimensions. That is about 154 MB per 100,000
chunks. The snippets and SQLite overhead bring the measured size to about 2,340 bytes per chunk, about
234 MB per 100,000 chunks.

`lctr delete-index` deletes the filename index and the embeddings file together. `lctr embed status`
shows the embedding counts for the index.

## What is skipped and why

Some files should never reach the model. They are skipped before any of their bytes are read:

- Files whose names suggest keys or credentials. The extension list is `.pem`, `.key`, `.p12`, `.pfx`,
  `.keystore`, `.jks`, `.ppk`, `.kdbx`, `.gpg`, `.asc`, `.crt`, `.cer`, `.der`, `.p8`, and
  `.mobileprovision`. The name prefixes are `id_rsa`, `id_dsa`, `id_ecdsa`, `id_ed25519`, `.env`, `secret`,
  `credential`, `.netrc`, `.npmrc`, `.pypirc`, and `htpasswd`. The list comes from DigUp.
- Anything under a hidden folder, or with a name that starts with a dot. `~/.ssh`, `~/.gnupg`, `~/.aws`,
  and `~/.config` hold secrets in the usual layout.
- Files in the home `Library` folder.

Some files are skipped because they add noise: lock files, minified JavaScript and CSS, source maps, SVG
images, log files, dependency and build folders (`node_modules`, `target`, `build`, `dist`, `.venv`, and
others), and vendor folders that sit next to a package manifest.

A text file is also skipped when its first 8 KiB contains a PEM private key block (`PRIVATE KEY-----`) or
an AWS access key id (`AKIA` followed by 16 characters). This catches keys saved under a harmless name.
Only the first 8 KiB is checked.

Skipped files are recorded with a reason (`secret or credential file`, `hidden file or folder`,
`private key`, `low-value file`, and so on). The `LCTR_TIMING` output counts skipped files by rule without
printing their names.

## Privacy

Everything runs on your machine. The model is downloaded once from Hugging Face and checked against a
pinned SHA-256. After that, no file content leaves the machine. The embeddings file contains snippets of
your text, so treat it like the files it came from. The file has mode 0600 on Unix. Whenever lctr
opens an index in a `.locator` folder for writing (scan, watch, live index) and the folder has no `.gitignore`, it
writes one containing `*`, so `git add .` in a scanned project does not pick up the index or the embeddings
file. Folders from older versions get it on their next scan. An existing `.gitignore` is never changed. An
index placed elsewhere with `LCTR_DB` has no such file; keep it out of version control yourself.

## Models and where they live

| File | Size | Used for |
|---|---|---|
| `embeddinggemma-2-Q8_0.gguf` | 310 MB | text and query embedding (needed for `--em`) |
| `mmproj-embeddinggemma-2-Q8_0.gguf` | 555 MB | images (downloaded when a scan finds images) |

Folders:

- macOS: `~/Library/Application Support/locator/models`
- Linux: `~/.local/share/locator/models`
- Windows: `%APPDATA%\locator\models`

`LCTR_DATA_DIR` moves the whole data folder. `lctr embed fetch [--vision]` downloads the models ahead of
time. `lctr embed remove-model [--yes]` deletes them.

The `lctr-full` release archives include the text model, so their first `--em` run needs no download.

## Troubleshooting

**Images skipped with "vision model not installed".** The vision model was not downloaded, either because
the prompt was declined or because the terminal was not interactive. Run `lctr embed fetch --vision`, then
scan again.

**Download failed.** Run `lctr embed fetch` again and read the error it prints. The model file is checked
against its SHA-256 after the download, and a mismatch is reported.

**Slow pass, GPU not used.** The line `embedding N files with Metal` names the backend. On Apple Silicon the
pass must run on Metal: if Metal is unavailable, the pass stops with "Metal backend unavailable on Apple
Silicon; refusing to fall back to CPU". Other systems run on the CPU, which is much slower.

**Intel Mac.** The prebuilt binaries do not cover Intel Macs. Build from source with cmake, libclang, and a
C++ toolchain.

**Search finds nothing.** Check `lctr embed status`. If the embedded count is zero, run the scan again with
`--em`. If the count is low, the scan may still be running at `quick` depth: run it with `--depth deep`, or
just wait for the later passes.

**Stale results after changing files.** Changed files are re-embedded on the next `--em` run. Deleted files
are removed from the embeddings file on the same run.

**Start over.** `lctr delete-index` removes the filename index and the embeddings. A scan with `--em`
rebuilds both.

**Changing the model or the vector size.** `embed_dims` changes re-embed on the next run. Changes to the
vector format are covered by `MODEL_ID` in the source, and the sidecar is rebuilt automatically when it
changes.

## Checking the vectors (for developers)

The reference test embeds DigUp's reference inputs and compares the results with DigUp's vectors. It needs
the models and is not run by default:

```sh
lctr embed fetch --vision
cargo test --test embed_reference_tests -- --ignored reference_vectors
```

Every cosine must be at least 0.999. A check of images against a screenshot must stay below 0.99, so that a
constant-vector bug cannot pass. `lctr embed selftest --reference` runs the same comparison with output.

## Credits

- EmbeddingGemma 2 by Google DeepMind, Apache 2.0 license.
- llama.cpp, MIT license.
- DigUp by ARahim3, MIT license. The reference vectors, the staged passes, and the secret-file rules come from
  DigUp.
