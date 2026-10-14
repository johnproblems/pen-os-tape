#!/bin/sh

set -e

export_archives() {
  for path in "$@"; do
    paths="$paths${paths:+:}$path"
  done

  export PEN_OS_ARCHIVES=$paths
}

while getopts o:t: option; do
  case $option in
  o)
    output=$OPTARG
    ;;
  t)
    target=$OPTARG
    ;;
  esac
done

shift $(expr $OPTIND - 1)

if [ -z "$output" ]; then
  exit 1
elif [ -n "$target" ]; then
  target_option="--target $target"
fi

cd $(dirname $0)/ffi/application

export_archives "$@"

# --target-dir keeps this build in ffi/application/target (where the binary
# is copied from below) instead of the workspace target dir.
cargo build --release --quiet $target_option --target-dir target

binary=target/$target/release/os-app

if [ -r $binary.wasm ]; then
  binary=$binary.wasm
fi

cp $binary $output

if [ $(uname) = Darwin ]; then
  codesign -s - $output
fi
