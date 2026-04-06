#!/bin/bash

SSH_KEY="~/mac2026.pem"
REMOTE="ubuntu@82.156.194.147"
PROJECT_DIR="~/projects/delayMirror"

echo "=========================================="
echo "Setting up remote server for delayMirror"
echo "=========================================="

ssh -i "$SSH_KEY" "$REMOTE" << 'EOF'
set -e

# 创建项目目录
mkdir -p ~/projects
cd ~/projects

# 检查仓库是否存在
if [ ! -d "delayMirror" ]; then
    echo "Repository not found. Please clone manually first:"
    echo "  git clone git@github.com:Fz0x00/testRepo.git delayMirror"
    exit 1
fi

cd delayMirror

# 安装 Rust（如果未安装）
if ! command -v cargo &> /dev/null; then
    echo "Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source ~/.cargo/env
fi

# 安装编译依赖
echo "Installing build dependencies..."
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev

echo ""
echo "=========================================="
echo "Setup complete!"
echo "=========================================="
EOF
