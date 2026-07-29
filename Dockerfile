# Aizen in a container — for `aizen serve` (the Telegram/Discord daemon) and for running the agent
# with a blast radius you control.
#
# WHY containerize a tool that advertises "no Docker": that promise is about INSTALLING aizen — you
# never need Docker to use it. This image is for the other direction: the agent has a shell and can
# edit files, so a container is the cheapest honest sandbox for `--approval yolo`. And `aizen serve`
# is a long-lived daemon, which is the shape orchestrators are good at.
#
# Build:  docker build -t aizen:local .
# Build without the dense retrieval tier (smaller, faster):
#         docker build --build-arg FEATURES= -t aizen:lean .

# ── stage 1: build ────────────────────────────────────────────────────────────────────
# Pinned to the toolchain this tree is actually verified against (`rustc -V` on the dev box and in
# CI). Cargo.lock is lockfile v4 and several deps sit on recent MSRVs, so "latest stable" would be a
# moving target and an older pin would fail in ways that look like our bug. Bump deliberately.
FROM rust:1.96-slim-bookworm AS build

# `--features dense` ships the model2vec semantic retrieval tier (matches what release.yml builds,
# so the container behaves like the published binaries). The model WEIGHTS are not baked in — see
# the runtime stage. Pass `--build-arg FEATURES=` for a lean build.
ARG FEATURES=dense

WORKDIR /src

# Cargo needs a real git for some registry operations; nothing else here is a C toolchain (the whole
# dependency tree is pure Rust, TLS included — rustls, not OpenSSL).
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Copy the manifests first so the dependency build layer is cached independently of source edits.
# `build.rs` is required at this step: it's declared in Cargo.toml, so `cargo build` refuses to run
# without it. The stub `main.rs` gets replaced below.
COPY Cargo.toml Cargo.lock build.rs ./
RUN mkdir -p src/agent \
 && echo 'fn main() {}' > src/main.rs \
 && : > src/agent/system_prompt.md \
 && : > src/agent/system_prompt_strict.md \
 && cargo build --release --bin aizen ${FEATURES:+--features $FEATURES} 2>/dev/null || true

# Now the real source. Touching main.rs defeats any stale-mtime caching of the stub.
COPY src ./src
COPY bench-fixtures ./bench-fixtures
RUN touch src/main.rs \
 && cargo build --release --bin aizen ${FEATURES:+--features $FEATURES} \
 && strip target/release/aizen || true

# ── stage 2: runtime ──────────────────────────────────────────────────────────────────
# debian-slim, NOT alpine: the binary is built against glibc here (as are the published Linux
# releases), so a musl-based image would fail to run it. Distroless would work for the binary but
# not for us — the agent needs a shell and git to do its job.
FROM debian:bookworm-slim

# - git: time machine checkpoints, the codebase index, and project-root detection all shell out to
#   it. Without git, edits still work (it degrades benignly) but every checkpoint is silently lost.
# - tini: the agent spawns builds, tests, and language servers. As PID 1 without a reaper those
#   become zombies that accumulate for the life of the container.
# - ca-certificates: TLS trust roots for the model endpoint and Telegram.
# - curl, less: what the agent reaches for constantly. Not strictly required; drop for a smaller image.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git ca-certificates tini curl less \
 && rm -rf /var/lib/apt/lists/*

# Run as a non-root user. The agent executes model-chosen shell commands, so root in the container
# is one misconfiguration (a bind mount, a shared namespace) away from root on things you care about.
# UID 10001 is arbitrary but fixed — a volume's ownership must survive an image rebuild.
RUN useradd --uid 10001 --create-home --shell /bin/bash aizen

COPY --from=build /src/target/release/aizen /usr/local/bin/aizen

# `~/.aizen` holds EVERYTHING that must outlive the container: cli-config.json (including the
# allowed_chat_ids that pairing writes), hostbot/bots.json (sub-bot tokens), hostbot/sessions/
# (per-chat context), cli-memory/ (memory + codebase index), models/ (the dense model cache).
# Declared as a volume so an anonymous one is created even if the operator forgets to mount one —
# losing this directory means losing owner pairing and all memory.
ENV AIZEN_HOME=/home/aizen/.aizen
VOLUME ["/home/aizen/.aizen"]

# /work is where the agent operates on your code. Mount your project here.
RUN mkdir -p /work && chown aizen:aizen /work
WORKDIR /work

USER aizen

# The daemon self-updates by renaming its own executable — meaningless on an immutable image, and
# actively harmful if it half-succeeds. The update CHECK is what we disable; there is no auto-apply.
ENV AIZEN_NO_UPDATE_CHECK=1

# Trust the container as a workspace: git refuses to operate on a tree owned by another uid, which
# is exactly what a bind-mounted host directory looks like from in here.
RUN git config --global --add safe.directory '*'

# Reads the heartbeat `aizen serve` writes; see `src/hostbot/health.rs`. `--interval` is longer than
# the daemon's 15s beat so a single slow tick doesn't flap. Kubernetes ignores this and uses the
# probes in the manifests instead.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD ["aizen", "serve", "--health"]

# tini reaps the agent's grandchildren and forwards SIGTERM to aizen, which now handles it (see
# `hostbot::daemon::shutdown_signal`): kill the process tree, drop the heartbeat, exit clean.
ENTRYPOINT ["/usr/bin/tini", "--", "aizen"]
CMD ["serve"]
