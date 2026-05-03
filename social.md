# locator Social Posting Kit

Use this as the source file for Reddit, Hacker News, Lobsters, X/Twitter, Mastodon, LinkedIn, and longer-form posts.

Core positioning:

> `locator` is a terminal-first local file metadata indexer. The command is `lctr`. It scans directories you control, stores a local SQLite index, and gives you fast filename/path search through a keyboard-first TUI or scriptable CLI.

Repository:

```text
https://github.com/NotTanJune/locator
```

Safer no-link wording for stricter communities:

```text
GitHub repo: NotTanJune/locator
```

Install snippet for places where direct install commands are useful:

```bash
brew tap NotTanJune/locator https://github.com/NotTanJune/locator
brew install lctr
```

Avoid in every post:

- Do not ask for stars.
- Do not cross-post identical text everywhere on the same day.
- Do not make the post read like an ad.
- Do not say it is a Spotlight replacement. Say it is a terminal-first indexed search tool for directories you control.
- Do not overclaim content search. `lctr` indexes metadata only in this version.

## Link And Self-Promotion Handling

| Place | Link strategy | Risk | Notes |
|---|---|---:|---|
| `r/rust` | Direct GitHub link is acceptable if the post is Rust-specific and technical. | Medium | Open source helps. Lead with implementation details, not product pitch. |
| `r/commandline` | Direct GitHub link is acceptable. | Low | Keep it about CLI/TUI workflow and keyboard-first search. |
| `r/DataHoarder` | Direct GitHub link is probably acceptable if framed as a tool for large disks and archives. | Medium | Keep the tone practical. Ask for large-folder feedback. |
| `r/macapps` | Prefer no raw URL in main-feed posts. Use the app promo megathread if applicable. | High | Use `GitHub repo: NotTanJune/locator` instead of a link unless rules clearly allow the post. |
| `r/selfhosted` | Skip as a top-level post. | High | `lctr` is local-first, not self-hosted. Only mention in comments if directly relevant. |
| `r/opensource` | Direct GitHub link is acceptable if the post focuses on the open-source project and technical tradeoffs. | Low-medium | Ask for feedback, not promotion. |
| Hacker News | Direct GitHub link is expected for Show HN. | Low | Use a concise title and concrete body. |
| Lobsters | Direct GitHub link is acceptable if submitted as technical content. | Medium | Best with tags like `rust`, `cli`, `search`, `release`. |
| X/Twitter, Mastodon, LinkedIn | Direct link is fine. | Low | Use demos, short claims, and clear install path. |

## Reddit

### `r/rust`

Title:

```text
I built lctr, a Rust terminal file indexer with a SQLite metadata index and ratatui search UI
```

Body:

````markdown
I have been building `locator`, a local file metadata search tool written in Rust. The command is `lctr`.

The goal is narrow: fast filename/path search for directories you control, especially large project folders and external drives. It does not index file contents. It scans metadata into a local SQLite database and then gives you either:

- `lctr search`, a keyboard-first TUI
- `lctr find <query>`, a scriptable one-shot CLI

The Rust parts that may be interesting:

- high-throughput directory traversal for large trees
- SQLite + FTS-backed metadata search
- indexed, live, and hybrid search modes
- `ratatui` search interface with filters, sort order, and file actions
- local-only index storage, no telemetry or background service

I built it after getting frustrated with repeat searches over external SSDs and large local folders. Apple does try to index external drives, but in my experience it can be slow, inconsistent, or lose usefulness when the drive is disconnected. I wanted an explicit index tied to the folder or drive I scan.

Repo:

https://github.com/NotTanJune/locator

Current install:

```bash
brew tap NotTanJune/locator https://github.com/NotTanJune/locator
brew install lctr
```

I would especially appreciate feedback on the Rust architecture, SQLite query design, traversal performance, and whether the CLI/TUI split feels right.
````

### `r/commandline`

Title:

```text
lctr: a keyboard-first terminal search UI backed by a local file metadata index
```

Body:

````markdown
I built `lctr`, a terminal-first local file search tool.

It is for repeated filename/path searches over directories you control:

- scan a directory once with `lctr scan`
- search it interactively with `lctr search`
- script it with `lctr find <query>`

It stores a local SQLite metadata index for paths, names, extensions, kinds, sizes, and dates. No file contents, no telemetry, no daemon.

Why I built it:

I wanted something between `fd` and Spotlight/Finder. `fd` is excellent for live searching, but it has to walk the tree every time. Finder and Spotlight can be awkward or unreliable for external drives and large folders. `lctr` trades an initial scan for instant repeated searches and a dense keyboard-first TUI.

Repo:

https://github.com/NotTanJune/locator

Install:

```bash
brew tap NotTanJune/locator https://github.com/NotTanJune/locator
brew install lctr
```

I would like feedback from people who live in terminal workflows: keybindings, filter design, scriptable output, and whether the search UI feels too dense or just right.
````

### `r/DataHoarder`

Title:

```text
I built a local metadata indexer for fast terminal search across large folders and external drives
```

Body:

````markdown
I built `lctr`, a local file metadata indexer for people with large folders, external drives, archives, and messy directory trees.

The basic flow:

```bash
lctr scan /Volumes/MyDrive
lctr search /Volumes/MyDrive
```

It stores a local SQLite index for filenames, paths, file kinds, extensions, sizes, and dates. It does not index file contents. The index is local and deletable with:

```bash
lctr delete-index /Volumes/MyDrive
```

Why I built it:

Finder search on external drives has been slow and inconsistent for me. Apple does index external drives in some cases, but it can be slow, fail silently, or become less useful after disconnecting and reconnecting drives. I wanted a tool where the index is explicit: scan this drive, search this drive, delete this drive index when done.

Repo:

https://github.com/NotTanJune/locator

Install:

```bash
brew tap NotTanJune/locator https://github.com/NotTanJune/locator
brew install lctr
```

If you have very large folders or external drives, I would be interested in scan speed, search latency, and any weird filesystem edge cases you hit.
````

### `r/macapps`

Use the app promo megathread or whichever app-promotion format the subreddit currently requires. This version avoids a raw link in the body.

Title:

```text
I built lctr, a terminal-first local search tool for large folders and external drives
```

Body:

````markdown
I built `locator`, a macOS-friendly terminal file search tool. The command is `lctr`.

It is not a Spotlight replacement. It is for people who want explicit, local, terminal-first indexing for folders and drives they control:

```bash
lctr scan /Volumes/MyDrive
lctr search /Volumes/MyDrive
```

What it does:

- indexes filenames, paths, file kinds, sizes, extensions, and dates
- stores the index locally as SQLite
- gives you a keyboard-first TUI with filters and sorting
- also has a scriptable `lctr find <query>` command
- does not upload data or index file contents

Why I built it:

Finder search can be fine for normal use, but on external drives and large folders it has often been too slow or inconsistent for my workflow. Apple does try to index external drives, but the experience can break down when drives are disconnected, moved, or not fully indexed. I wanted explicit indexing that I control from the terminal.

GitHub repo: NotTanJune/locator

I am happy to move this to the app promo megathread if that is the better place.
````

### `r/opensource`

Title:

```text
I built an open-source terminal file indexer for local metadata search
```

Body:

```markdown
I built `locator`, an open-source local file metadata search tool. The command is `lctr`.

It is a small tool with a focused goal: scan directories you control, store a local SQLite metadata index, then search filenames and paths quickly from the terminal.

What it supports:

- interactive TUI: `lctr search`
- scriptable CLI: `lctr find <query>`
- metadata filters: kind, extension, size, created date, modified date
- sort fields and ascending/descending order
- local index deletion
- no telemetry, no account, no file content indexing

Repo:

https://github.com/NotTanJune/locator

The design tradeoff is intentional: it does not try to be a full document search engine. Tools like `sist2` are better if you need content extraction, OCR, archive parsing, or web search. `lctr` is for fast local metadata search in terminal workflows.

I would appreciate feedback on the README, install flow, defaults, and whether the project scope is clear enough.
```

### `r/selfhosted`

Recommendation: skip top-level posting. This tool is local-first and not self-hosted.

If someone asks about local file indexing or searching disks from a homelab machine, use a comment like this instead:

```markdown
If you want a local-first terminal option rather than a web service, I built `lctr` for this kind of metadata search. It scans a directory into a local SQLite index and gives you `lctr search` plus `lctr find`.

It is not self-hosted in the usual server-app sense, so it may or may not fit what you want.

GitHub repo: NotTanJune/locator
```

## Hacker News

### Show HN

Title:

```text
Show HN: lctr, a terminal-first local file indexer for large folders and drives
```

Body:

````markdown
Hi HN,

I built `lctr`, a terminal-first local file metadata indexer.

The use case is repeated filename/path search over directories you control, especially large project folders and external drives. It does not index file contents. It scans metadata into a local SQLite index, then gives you:

- `lctr search`, a keyboard-first TUI
- `lctr find <query>`, a scriptable CLI

Repo: https://github.com/NotTanJune/locator

The reason I built it: Finder/Spotlight can be slow or inconsistent for external drives, and live tools like `fd` are great but still walk the tree every time. `lctr` trades one explicit scan for fast repeated searches, visible filters, sorting, and local-only index deletion.

Current install:

```bash
brew tap NotTanJune/locator https://github.com/NotTanJune/locator
brew install lctr
```

I would appreciate feedback on the UX, performance numbers, install flow, and whether the boundary between this and tools like `fd`, `sist2`, and Spotlight is clear.
````

## Lobsters

Title:

```text
lctr: terminal-first local file metadata indexing in Rust
```

Suggested tags:

```text
rust, cli, release, search
```

Body:

````markdown
I built `lctr`, a Rust CLI/TUI for local file metadata search.

It scans filenames, paths, kinds, extensions, sizes, and dates into a local SQLite index. It does not index file contents. The main commands are:

```bash
lctr scan /path/to/root
lctr search /path/to/root
lctr find archive --sort size --reverse
```

Repo:

https://github.com/NotTanJune/locator

The main design goal is explicit local indexing for terminal users. It is lighter than a content indexer, more persistent than live-only search, and more scriptable than GUI search.

Technical feedback I am looking for:

- SQLite/FTS schema and query design
- scan throughput on large directory trees
- TUI keymap and filter ergonomics
- packaging and install clarity
````

## X/Twitter

### Single Post

```text
I built lctr: a terminal-first local file indexer for large folders and external drives.

Scan once:
  lctr scan /Volumes/MyDrive

Then search instantly:
  lctr search /Volumes/MyDrive

Local SQLite index. No telemetry. Metadata only. Keyboard-first TUI.

https://github.com/NotTanJune/locator
```

### Thread

```text
1/ I built lctr, a terminal-first local file indexer.

It is for repeated filename/path search over large folders, project trees, and external drives.

https://github.com/NotTanJune/locator
```

```text
2/ The workflow is explicit:

lctr scan /Volumes/MyDrive
lctr search /Volumes/MyDrive

The index is a local SQLite file. No telemetry, no account, no cloud sync, no content indexing.
```

```text
3/ Why not just Finder or Spotlight?

For normal use, they are fine. For external drives, Apple indexing can be slow, inconsistent, or lose usefulness when drives are disconnected.

lctr makes the index explicit and tied to the directory or drive you scan.
```

```text
4/ Why not just fd?

fd is excellent for live filesystem search. lctr is different: it trades an initial scan for fast repeated searches, metadata filters, sorting, and a keyboard-first TUI.
```

```text
5/ Current install:

brew tap NotTanJune/locator https://github.com/NotTanJune/locator
brew install lctr

I am looking for feedback from terminal users, Rust devs, and people with large external drives.
```

## Mastodon

```text
I built `lctr`, a terminal-first local file metadata indexer.

It scans directories you control into a local SQLite index, then gives you fast filename/path search through a keyboard-first TUI or scriptable CLI.

Useful for large project folders, external drives, and repeated searches where live traversal gets old.

No telemetry. No account. No content indexing.

https://github.com/NotTanJune/locator
```

## LinkedIn

````markdown
I built a small developer tool called `locator`. The command is `lctr`.

It started from a very ordinary frustration: searching large folders and external drives from macOS was slower and less predictable than I wanted, especially when drives were disconnected and reconnected.

So I built the version I wanted:

- scan a directory or drive explicitly
- store a local SQLite metadata index
- search from a keyboard-first terminal UI
- use a scriptable CLI when automation matters
- keep everything local, with no telemetry or file content indexing

The positioning is intentionally narrow. This is not trying to be a full document search engine, and it is not trying to replace Spotlight for normal Mac usage. It is for terminal users who want fast, repeatable metadata search over directories they control.

Example:

```bash
lctr scan /Volumes/MyDrive
lctr search /Volumes/MyDrive
```

Repo:

https://github.com/NotTanJune/locator

I would love feedback from people who work with large project folders, external SSDs, archives, or terminal-heavy file workflows.
````

## Dev.to Or Personal Blog

Title:

```text
Building lctr: a terminal-first local file indexer in Rust
```

Outline:

````markdown
# Building lctr: a terminal-first local file indexer in Rust

## The Problem

Finder and Spotlight are good for many everyday searches, but external drives and large working directories can be unpredictable. Live CLI tools are excellent, but repeated searches over the same huge tree still pay traversal cost.

## The Design

`lctr` makes indexing explicit:

```bash
lctr scan /Volumes/MyDrive
lctr search /Volumes/MyDrive
```

It stores metadata in SQLite:

- paths
- filenames
- file kinds
- extensions
- sizes
- created dates
- modified dates

It does not index file contents.

## The Interface

There are two surfaces:

- `lctr search`, a keyboard-first TUI
- `lctr find`, a scriptable CLI

## The Tradeoffs

This is not a document search engine. If you need OCR, archive parsing, or content extraction, use something like `sist2`.

`lctr` is for fast local metadata search, visible filters, sort order, and terminal-native workflows.

## Privacy

There is no account, telemetry, cloud sync, or background service. Indexes are local SQLite files and can be deleted with:

```bash
lctr delete-index
```

## Repo

https://github.com/NotTanJune/locator
````

## Product Hunt

Tagline:

```text
Terminal-first local file search for huge folders and drives
```

Description:

```markdown
`lctr` is a local file metadata indexer for terminal users. Scan a folder or external drive once, then search filenames and paths instantly through a dense keyboard-first TUI or a scriptable CLI.

It stores a local SQLite index, does not upload data, does not index file contents, and lets you delete indexes whenever you want.
```

First comment:

```markdown
I built `lctr` because I wanted explicit, repeatable search over large folders and external drives.

Finder and Spotlight are fine for everyday usage, but external-drive indexing can be slow or inconsistent. Live CLI tools like `fd` are excellent, but they still traverse the tree each time.

`lctr` sits between those: scan once, search quickly, keep the index local.

Repo:

https://github.com/NotTanJune/locator
```

## General Reply For Comments

Use this when someone asks how it differs from other tools:

```markdown
The short version:

- `fd`: excellent live search, no persistent index
- Spotlight/Finder: OS-integrated, but less explicit and often awkward for external drives
- `sist2`: much heavier content indexing, web UI, OCR/archive/media support
- `lctr`: local metadata index, terminal-first TUI, scriptable CLI, no content indexing

So the target use case is fast repeated filename/path search over folders and drives you explicitly scan.
```

Use this when someone asks about privacy:

````markdown
Everything stays local. `lctr` stores metadata in a SQLite index: paths, filenames, extensions, kinds, sizes, and dates. It does not index file contents, upload anything, run telemetry, require an account, or sync to a cloud service.

Indexes can be deleted with:

```bash
lctr delete-index
lctr delete-index /Volumes/MyDrive
```
````
