# Close to GitHub's ubuntu-24.04 runner: Ubuntu 24.04 on x86_64, a non-root
# runner user, Rust stable with rustfmt, clippy, and wasm32, and Google Chrome
# stable. Build and run it with run.sh beside this file.
FROM --platform=linux/amd64 ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl gnupg build-essential pkg-config git rsync procps psmisc stress-ng lsof \
 && curl -fsSL https://dl.google.com/linux/linux_signing_key.pub | gpg --dearmor -o /usr/share/keyrings/google-chrome.gpg \
 && echo "deb [arch=amd64 signed-by=/usr/share/keyrings/google-chrome.gpg] http://dl.google.com/linux/chrome/deb/ stable main" \
      > /etc/apt/sources.list.d/google-chrome.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends google-chrome-stable \
 && rm -rf /var/lib/apt/lists/*

RUN useradd -m -s /bin/bash runner
USER runner
WORKDIR /home/runner
ENV PATH=/home/runner/.cargo/bin:$PATH CARGO_TERM_COLOR=never
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable -c rustfmt,clippy -t wasm32-unknown-unknown
# Named volumes mounted here keep the runner's ownership. The checkout goes
# where GitHub puts it, work/pulp/pulp, since a test once failed only there.
RUN mkdir -p /home/runner/target /home/runner/.cargo/registry /home/runner/.cargo/git /home/runner/work/pulp/pulp
COPY run-ci.sh /usr/local/bin/run-ci.sh
