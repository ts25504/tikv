#!/usr/bin/env bash

set -e

if [[ "$1" != "--use-cache" ]]; then
	rm -r cache &>/dev/null || true
fi

cd `dirname $0`
mkdir cache &>/dev/null || true
CARGO_HOME=./cache cargo fetch
docker build -f release_dockerfile . -t tikv-release-builder

