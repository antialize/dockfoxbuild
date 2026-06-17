# dockfoxbuild

A [Buildah](https://buildah.io/)-based image build driver that only breaks
layers at **explicit cache boundaries** instead of creating a new layer for
every instruction.

In a normal Docker/OCI build, every `RUN`, `COPY`, `ENV`, … produces its own
layer, and the cache is invalidated from the first changed instruction onward.
`dockfoxbuild` instead groups consecutive instructions into a single layer and
only starts a new cacheable layer where you explicitly ask for one with a
`# CHECKPOINT` comment (or where a new build stage starts with `FROM`). Each
layer is content-addressed with a [BLAKE3](https://github.com/BLAKE3-team/BLAKE3)
hash of its instructions (and, for `COPY`, the actual file contents), so a cache
hit reuses a previously committed image and skips execution entirely.

## How it works

The Dockerfile is split into **chunks**. A new chunk begins at every `FROM` and
at every `# CHECKPOINT` line:

```dockerfile
FROM docker.io/library/rust:1 AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN cargo fetch
# CHECKPOINT: dependencies are cached up to here
COPY src ./src
RUN cargo build --release
```

For each chunk, `dockfoxbuild`:

1. Computes a BLAKE3 hash of the chunk — the base image, the `ENV`/`RUN`/
   `WORKDIR`/`LABEL` lines (after variable substitution), and the contents of
   any files referenced by `COPY` (honouring `.dockerignore`).
2. Looks the hash up in a local SQLite cache and, optionally, a remote registry
   (`--cache-from`).
3. On a **cache hit**, reuses the committed image and skips running the chunk.
4. On a **cache miss**, executes the chunk with `buildah`, commits the result as
   a single layer, records it in the cache, and (optionally) pushes it to a
   remote registry (`--cache-to`).

Locally, cached layers are stored as Buildah images named
`localhost/dockfoxbuild_cache:<hash>`, and cache metadata lives in an SQLite
database under `$XDG_CACHE_HOME/dockfoxbuild/cache.db` (falling back to
`$HOME/.cache/dockfoxbuild/cache.db`).

## Requirements

- Linux
- [`buildah`](https://buildah.io/) installed and available on `PATH`

## Installation

```sh
cargo build --release
# binary at target/release/dockfoxbuild
```

## Usage

### Build

```sh
dockfoxbuild build [OPTIONS] [CONTEXT]
```

| Option | Description |
| --- | --- |
| `[CONTEXT]` | Build context directory (default: `.`) |
| `-f, --file <FILE>` | Path to the Dockerfile (default: `<CONTEXT>/Dockerfile`) |
| `--build-arg <KEY=VALUE>` | Set a build argument (repeatable) |
| `-t, --tag <TAG>` | Tag the final image (repeatable) |
| `--no-cache` | Do not reuse cached layers |
| `--cache-from <REF>` | Import cache from a registry repository (tagged by hash) |
| `--cache-to <REF>` | Export cache to a registry repository (tagged by hash) |
| `--format <oci\|docker>` | Output image format |
| `--pull` | Accepted for `docker build` compatibility (no-op; images are always pulled) |
| `--layers` | Accepted for `docker build` compatibility (no-op; layers are always built) |

Example:

```sh
dockfoxbuild build -t myimage:latest --build-arg VERSION=1.2.3 .
```

With a remote cache shared across CI runners:

```sh
dockfoxbuild build \
  --cache-from registry.example.com/myimage/cache \
  --cache-to   registry.example.com/myimage/cache \
  -t registry.example.com/myimage:latest .
```

### Prune

Remove old Buildah images and stray build containers to reclaim disk space.

```sh
dockfoxbuild prune [OPTIONS]
```

| Option | Default | Description |
| --- | --- | --- |
| `--min-age <DUR>` | `6h` | Images younger than this are never pruned |
| `--max-age <DUR>` | `336h` | Images older than this are pruned regardless of cache size |
| `--max-cache-size <SIZE>` | `100GB` | Older images are pruned until the cache is under this size |
| `-q, --quiet` | | Only print summary output |

Durations accept `m`, `h`, `d`, `w` suffixes (e.g. `90m`, `2d`, `2w`); sizes
accept `KB`, `MB`, `GB` suffixes (e.g. `512MB`, `50GB`).

## Supported Dockerfile instructions

`FROM` (including `AS` for multi-stage builds), `ARG`, `ENV`, `RUN`, `WORKDIR`,
`COPY` (including `--from`), `LABEL`, line continuations (`\`), and the
`# CHECKPOINT` cache-boundary comment.

`RUN` commands are executed with `sh -c`. Variable substitution uses build args
and environment variables in scope for the current stage.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

