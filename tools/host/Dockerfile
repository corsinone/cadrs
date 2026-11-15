# The cadrs team host (cadrs --host): the always-online owner of a team's shared documents. Its
# identity (its cadrs ID), its users (users.ron), its settings (host.ron) and the documents it
# owns are in /data. Users are added with `docker exec cadrs-host cadrs --host-user …`
# (tools/host/README.md).
FROM ubuntu:26.04
RUN apt-get update \
 && apt-get install -y --no-install-recommends libudev1 libwayland-client0 libasound2t64 libffi8 ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY cadrs /usr/local/bin/cadrs
ENV RUST_LOG=info
# Ctrl+C, which the host stops on after saving everything (it doesn't catch SIGTERM).
STOPSIGNAL SIGINT
ENTRYPOINT ["/usr/local/bin/cadrs", "--host", "--data-dir", "/data"]
