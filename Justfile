#!/usr/bin/env just --justfile
name := 'wlrix-idle'

rootdir := ''
prefix := '/usr'

base-dir := absolute_path(clean(rootdir / prefix))

bin-src := 'target' / 'release' / name
bin-dst := base-dir / 'bin' / name

default:
  @just --list

release:
  cargo build --release

lint:
  cargo clippy

test:
  cargo test

# Install the idle manager.
#
# Deliberately does not build: this is normally run as root, and building as root leaves a
# target directory nobody can write to afterwards.
#
#     just release && sudo just install
[doc("Install the idle manager (build first; run as root)")]
install:
  #!/usr/bin/env bash
  set -euo pipefail
  if [ ! -x '{{bin-src}}' ]; then
    echo "no release build -- run 'just release' first" >&2
    exit 1
  fi
  install -Dm0755 '{{bin-src}}' '{{bin-dst}}'
  echo "installed {{bin-dst}}"

# Remove what `install` put down.
[doc("Remove what install put down")]
uninstall:
  #!/usr/bin/env bash
  set -euo pipefail
  rm -f '{{bin-dst}}'
  echo "removed {{bin-dst}}"

clean:
  cargo clean
