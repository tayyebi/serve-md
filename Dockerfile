# Alpine rather than scratch, on purpose.
#
# The binary really is fully static and would run in `FROM scratch` — but
# `search_docs` shells out to a search tool, and a scratch image has none. The
# whole agent surface would come up with search disabled. Alpine plus ripgrep
# costs a few megabytes and makes the image actually useful.

FROM rust:1-alpine AS build
WORKDIR /src
RUN apk add --no-cache musl-dev
COPY Cargo.toml ./
COPY src ./src
COPY templates ./templates
RUN cargo build --release

FROM alpine:3
RUN apk add --no-cache ripgrep
COPY --from=build /src/target/release/serve-md /usr/local/bin/serve-md

# The documents to serve are mounted here:
#   docker run --rm -p 8080:8080 -v "$PWD:/docs" ghcr.io/tayyebi/serve-md
WORKDIR /docs
EXPOSE 8080

# 0.0.0.0 because a container-local bind would be unreachable from the host,
# and --no-open because there is no browser in here to open.
ENTRYPOINT ["serve-md", "--host", "0.0.0.0", "--port", "8080", "--dir", "/docs", "--no-open"]
CMD ["--plugin", "webmcp", "--fresh"]
