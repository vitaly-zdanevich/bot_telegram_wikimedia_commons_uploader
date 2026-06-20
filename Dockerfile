# Builds the Lambda zip WITH HEIC support for arm64 (provided.al2023).
# Compiles libde265 + libheif from source so the runtime ABI matches Amazon Linux 2023,
# links the bot with --features heic, and bundles the shared libraries under lib/.
# Build with: docker buildx build --platform linux/arm64 -o type=local,dest=build .
# (or just run scripts/build-lambda-docker.sh)

FROM public.ecr.aws/amazonlinux/amazonlinux:2023 AS builder

RUN dnf -y install gcc gcc-c++ make cmake git pkgconfig zip tar gzip xz findutils which \
    && dnf clean all

RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

# HEVC decoder used by libheif.
RUN git clone --depth 1 https://github.com/strukturag/libde265.git /src/libde265 \
    && cmake -S /src/libde265 -B /src/libde265/build -DCMAKE_BUILD_TYPE=Release \
       -DCMAKE_INSTALL_PREFIX=/usr/local -DENABLE_SDL=OFF \
    && cmake --build /src/libde265/build -j"$(nproc)" \
    && cmake --install /src/libde265/build

# HEIC/HEIF container library.
ENV PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:/usr/local/lib64/pkgconfig
RUN git clone --depth 1 https://github.com/strukturag/libheif.git /src/libheif \
    && cmake -S /src/libheif -B /src/libheif/build -DCMAKE_BUILD_TYPE=Release \
       -DCMAKE_INSTALL_PREFIX=/usr/local -DWITH_EXAMPLES=OFF -DBUILD_TESTING=OFF \
    && cmake --build /src/libheif/build -j"$(nproc)" \
    && cmake --install /src/libheif/build

ENV LD_LIBRARY_PATH=/usr/local/lib:/usr/local/lib64

WORKDIR /app
COPY Cargo.toml ./
COPY Cargo.lock* ./
COPY src ./src
RUN cargo build --release --features heic --bin telegram-wikimedia-commons-uploader-bot

RUN mkdir -p /out/lib \
    && cp target/release/telegram-wikimedia-commons-uploader-bot /out/bootstrap \
    && for lib in libheif libde265; do \
         find /usr/local -name "${lib}.so*" -exec cp -P {} /out/lib/ \; ; \
       done \
    && cd /out && zip -r9 /lambda.zip bootstrap lib

FROM scratch AS export
COPY --from=builder /lambda.zip /lambda.zip
