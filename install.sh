#!/bin/bash

# vim-mcp installer script
# Builds the Rust MCP server and prints the Claude Code configuration.

set -e

echo "======================================="
echo "       vim-mcp Installer"
echo "======================================="
echo ""

# Check the Rust toolchain
check_rust() {
    if ! command -v cargo &> /dev/null; then
        echo "Error: cargo (the Rust toolchain) is not installed"
        echo "Install it from https://rustup.rs"
        exit 1
    fi

    RUST_VERSION=$(rustc --version | awk '{print $2}')
    echo "✓ Rust $RUST_VERSION detected"
}

# Check Vim/Neovim
check_vim() {
    VIM_FOUND=false

    if command -v vim &> /dev/null; then
        if vim --version | grep -q "+channel"; then
            echo "✓ Vim with +channel support detected"
            VIM_FOUND=true
        else
            echo "Vim found but lacks +channel support"
        fi
    fi

    if command -v nvim &> /dev/null; then
        echo "✓ Neovim detected"
        VIM_FOUND=true
    fi

    if [ "$VIM_FOUND" = false ]; then
        echo "Error: No compatible Vim/Neovim found"
        echo "Please install Vim 8.0+ with +channel or Neovim 0.5+"
        exit 1
    fi
}

# Build the server
build_server() {
    echo ""
    echo "Building the MCP server (release)..."
    cargo build --release --manifest-path server/Cargo.toml
    echo "✓ Server built"
}

# Show configuration instructions
show_config() {
    SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
    BINARY="$SCRIPT_DIR/server/target/release/vim-mcp"

    echo ""
    echo "======================================="
    echo "     Installation Complete!"
    echo "======================================="
    echo ""
    echo "Next step: Configure Claude Code MCP settings"
    echo ""
    echo "Use this configuration:"
    echo '{'
    echo '  "mcpServers": {'
    echo '    "vim-mcp": {'
    echo "      \"command\": \"$BINARY\","
    echo '      "args": []'
    echo '    }'
    echo '  }'
    echo '}'
    echo ""
    echo "(Optional) Copy the binary onto your PATH for a shorter config:"
    echo "  cp \"$BINARY\" ~/.local/bin/vim-mcp"
    echo ""
    echo "After configuration:"
    echo "1. Restart Claude Code"
    echo "2. Open Vim and run :VimMCPStatus to verify connection"
}

# Main installation
main() {
    echo "Checking prerequisites..."
    check_rust
    check_vim

    build_server
    show_config
}

main
