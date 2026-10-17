# Context: repository root, after package-rootfs.sh creates the verified closure.
# No network, Rust compiler, mutable package repository, or guessed ELF loader.
FROM scratch
COPY artifacts/live-relay-rootfs/ /
USER 0:0
ENV AUTHORITY_PATH=/data/relay/authority.json
EXPOSE 8080
ENTRYPOINT ["/bin/bmux-live-relay"]
