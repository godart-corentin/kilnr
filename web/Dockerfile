FROM node:22-alpine AS frontend
WORKDIR /src
COPY frontend/package*.json ./
RUN if [ -f package-lock.json ]; then npm ci --no-audit --no-fund; else npm install --no-audit --no-fund; fi
COPY frontend/ ./
RUN npm run build

FROM rust:1.85-alpine AS backend
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM scratch
COPY --from=frontend /src/dist /opt/kilnr/static
COPY --from=backend /src/target/release/kilnr /opt/kilnr/web
USER 65532:65532
CMD ["/opt/kilnr/web"]
