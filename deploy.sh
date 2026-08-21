#!/bin/sh
set -e

cargo build --release
cargo install --path .
