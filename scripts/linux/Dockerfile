# Build environment for Flash for Linux release packages. Debian 12's glibc (2.36) is older
# than every supported distribution's, so binaries built here run on all of them.
FROM rust:1-bookworm
ARG TARGETARCH
RUN apt-get update -qq \
 && apt-get install -y -qq --no-install-recommends dpkg-dev rpm file xz-utils desktop-file-utils ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
# appimagetool 1.9.0 and the AppImage runtime it embeds (release 20251108), from the AppImage
# project's GitHub releases, each checked against its digest.
RUN set -e; \
    case "$TARGETARCH" in \
      amd64) a=x86_64; tool=46fdd785094c7f6e545b61afcfb0f3d98d8eab243f644b4b17698c01d06083d1; rt=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d ;; \
      arm64) a=aarch64; tool=04f45ea45b5aa07bb2b071aed9dbf7a5185d3953b11b47358c1311f11ea94a96; rt=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444 ;; \
      *) echo "unsupported architecture $TARGETARCH"; exit 1 ;; \
    esac; \
    curl -fsSL -o /usr/local/bin/appimagetool "https://github.com/AppImage/appimagetool/releases/download/1.9.0/appimagetool-$a.AppImage"; \
    curl -fsSL -o /usr/local/share/appimage-runtime "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-$a"; \
    echo "$tool  /usr/local/bin/appimagetool" | sha256sum -c -; \
    echo "$rt  /usr/local/share/appimage-runtime" | sha256sum -c -; \
    # Clear the AppImage magic (bytes 8-10) so a host's binfmt_misc AppImage handler, which
    # the container can't see, doesn't take over: it runs as the plain ELF it is.
    printf '\0\0\0' | dd of=/usr/local/bin/appimagetool bs=1 seek=8 conv=notrunc status=none; \
    chmod +x /usr/local/bin/appimagetool
# No FUSE in containers: AppImages unpack themselves instead.
ENV APPIMAGE_EXTRACT_AND_RUN=1
