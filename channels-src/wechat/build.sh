#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

echo "Building WeChat channel WASM component..."

# Check for debug mode
if [ "$1" = "--debug" ]; then
    PROFILE="debug-wasm"
    echo "  (debug build with DWARF symbols)"
else
    PROFILE="release"
fi

# Build the WASM module
cargo build --profile "$PROFILE" --target wasm32-wasip2

WASM_PATH="target/wasm32-wasip2/$PROFILE/wechat_channel.wasm"

if [ -f "$WASM_PATH" ]; then
    if command -v wasm-tools >/dev/null 2>&1; then
        wasm-tools component new "$WASM_PATH" -o wechat.wasm 2>/dev/null || cp "$WASM_PATH" wechat.wasm
        # Optimize the component (skip strip for debug builds to preserve DWARF)
        if [ "$PROFILE" = "debug-wasm" ]; then
            echo "  Skipping wasm-tools strip to preserve debug symbols"
        else
            wasm-tools strip wechat.wasm -o wechat.wasm
        fi
    else
        cp "$WASM_PATH" wechat.wasm
        echo "wasm-tools not found; copied raw wasm output without component conversion/strip"
    fi

    echo "Built: wechat.wasm ($(du -h wechat.wasm | cut -f1))"
    echo ""
    echo "To install:"
    echo "  mkdir -p ~/.ironclaw/channels"
    echo "  cp wechat.wasm wechat.capabilities.json ~/.ironclaw/channels/"
else
    echo "Error: WASM output not found at $WASM_PATH"
    exit 1
fi
