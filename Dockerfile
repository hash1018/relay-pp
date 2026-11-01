# relay-pp on Linux, with what its GPU conversions need: NVIDIA's libraries
# come from the machine, through the NVIDIA container runtime, and are loaded
# when first used, so the same image runs without a GPU. See "Docker" in
# docs/status.md for how to run it.
#
#   docker build -t relay-pp .

FROM ubuntu:26.04 AS ffmpeg
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential ca-certificates git curl zip unzip tar pkg-config \
        ninja-build nasm autoconf automake libtool python3 clang libclang-dev \
        libvulkan-dev

# FFmpeg 8.0.1, which media-pp's bindings need and no distribution promises,
# built as media-pp says to. In a stage of its own, ahead of relay-pp's
# sources, since it takes tens of minutes and changes rarely: this revision
# only names that script, and need not follow Cargo.toml's. OpenH264 for
# `convert.video = "any"`, nvcodec for the GPU's, Opus for WebRTC's sound; all
# of it LGPL.
ARG SETUP_FFMPEG_REV=940f938698c19a9db0ddbbfa71223289ca4dce25
RUN curl -fsSL "https://raw.githubusercontent.com/hash1018/media-pp/$SETUP_FFMPEG_REV/scripts/setup-ffmpeg.sh" \
        | sh -s -- --into /opt/ffmpeg --features openh264,nvcodec,vulkan,opus \
    && rm -rf /opt/ffmpeg/vcpkg/buildtrees /opt/ffmpeg/vcpkg/downloads /opt/ffmpeg/vcpkg/packages

FROM ffmpeg AS build
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY assets assets
# The cache mounts keep a rebuild from fetching and compiling every dependency
# again; a cached target/ is not in the image, so the program is copied out.
RUN --mount=type=cache,target=/opt/cargo/registry --mount=type=cache,target=/opt/cargo/git \
    --mount=type=cache,target=/src/target \
    FFMPEG_DIR=/opt/ffmpeg/vcpkg/installed/x64-linux-dynamic \
    LD_LIBRARY_PATH=/opt/ffmpeg/vcpkg/installed/x64-linux-dynamic/lib \
    cargo build --release --locked \
    && mkdir -p /dist/bin /dist/lib \
    && cp target/release/relay-pp /dist/bin/ \
    && cp -a /opt/ffmpeg/vcpkg/installed/x64-linux-dynamic/lib/*.so* /dist/lib/

FROM ubuntu:26.04
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libvulkan1 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1001 --create-home --home-dir /data relay
COPY --from=build /dist/bin/relay-pp /usr/local/bin/relay-pp
COPY --from=build /dist/lib/ /usr/local/lib/
# A library the build found and this image lacks stops the build here, not the
# first run.
RUN ldconfig && ! ldd /usr/local/bin/relay-pp | grep 'not found'

# Where a relay-pp.toml is read from when none is named, and where the
# recordings and media-pp's logs go unless it says otherwise.
USER relay
WORKDIR /data
# The default ports; a `listen` of another moves them. WebRTC takes one UDP
# port a thread, from 8189 on.
EXPOSE 8888/tcp 8554/tcp 1935/tcp 8189/udp 8190/udp
ENTRYPOINT ["relay-pp"]
