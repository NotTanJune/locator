# NOTICE: material from DigUp

Parts of locator's embedding code and its reference checks follow DigUp, an open-source macOS
app. DigUp is MIT licensed (full text below and in `LICENSE`).

- Source repository: https://github.com/ARahim3/DigUp
- Source commit: ec28973268b81f836be72187ff23b3a16ef4fd97 (the `HEAD` of `main` when copied)

## What was taken and how it is used

DigUp's Swift sources were read as reference. The Rust code in `src/` is a port of the logic,
not a copy of the files. The constants and logic below come from DigUp:

- Secret file names and extensions (`isSecret` in `Sources/DigUpKit/Rules.swift`): `src/embed/skip.rs`.
- Image sizing (`targetSize` in `Sources/LlamaRuntime/ImageSizing.swift`): `src/embed/image_sizing.rs`.
- Prompt strings (`Prompts` in `Sources/DigUpKit/Model.swift`): `src/embed/reference.rs` and `query_input` / `document_input` in `src/embed/runtime.rs`.
- Mixed text and image batch (`embedMedia` in `Sources/LlamaRuntime/LlamaEmbedder.swift`): `src/embed/mixed_batch.rs`.

## Files in this directory

These files are read by the reference checks (`src/embed/reference.rs`, the `reference_vectors`
ignored test, and `lctr embed selftest --reference`) and are embedded into the binary at build time.

| File here | Upstream path | Use |
|---|---|---|
| `vectors.json` | `Tests/LlamaRuntimeTests/Reference/vectors.json` | reference vectors made with Google's mlx-vlm |
| `photo.jpg` | `Tests/LlamaRuntimeTests/Reference/photo.jpg` | reference image (photo at budget 140, video frame at 70) |
| `screen.png` | `Tests/LlamaRuntimeTests/Reference/screen.png` | reference image (screenshot at budget 280) |
| `LICENSE` | `LICENSE` | DigUp's MIT license |

The Swift files that were used as reading material have been removed from this directory; the
commit above is where to find them.

## DigUp license text

MIT License

Copyright (c) 2026 Abdur Rahim

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
