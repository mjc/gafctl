ARG BASE_IMAGE=docker.io/library/ubuntu:24.04
FROM ${BASE_IMAGE}
RUN printf '%s\n' 'path-include=/usr/share/doc/gafctl' 'path-include=/usr/share/doc/gafctl/*' > /etc/dpkg/dpkg.cfg.d/zz-gafctl-notices
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends systemd-sysv dbus curl ca-certificates libdbus-1-3 dpkg-dev && rm -rf /var/lib/apt/lists/*
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
