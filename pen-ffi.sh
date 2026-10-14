#!/bin/sh

set -e

while getopts t: option; do
  case $option in
  t)
    target=$OPTARG
    ;;
  esac
done

shift $(expr $OPTIND - 1)

if [ -z $target ]; then
  exit 1
fi

cd $(dirname $0)/ffi/library
# --target-dir keeps this build in ffi/library/target (where the archive is
# copied from below) instead of the workspace target dir.
cargo build --release --quiet --target $target --target-dir target
# spell-checker: disable-next-line
cp target/$target/release/libos.a $1
