# syntax=docker/dockerfile:1
ARG EDGERUNNER_BASE_IMAGE=registry.fpl.dev/edgerunner-runner:latest

FROM rust:1.98-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p legion-of-bom-cli
RUN cargo build --release --locked --manifest-path edgerunner-runner/Cargo.toml

FROM ${EDGERUNNER_BASE_IMAGE}
USER root
RUN apt-get update \
    && apt-get install -y --no-install-recommends kicad kicad-symbols kicad-footprints ngspice poppler-utils python3-venv \
    && rm -rf /var/lib/apt/lists/*
COPY requirements.txt /opt/lob/requirements.txt
RUN python3 -m venv /opt/lob/venv \
    && /opt/lob/venv/bin/pip install --no-cache-dir -r /opt/lob/requirements.txt
COPY --from=build /src/target/release/lob /usr/local/bin/lob
COPY --from=build /src/edgerunner-runner/target/release/lob-edgerunner-runner /usr/local/bin/lob-edgerunner-runner
ENV PATH=/opt/lob/venv/bin:$PATH KICAD9_SYMBOL_DIR=/usr/share/kicad/symbols
RUN lob doctor && edgerunner-report --help >/dev/null
USER edgerunner
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/lob-edgerunner-runner"]
