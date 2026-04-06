#!/bin/bash

SSH_KEY="~/mac2026.pem"
REMOTE="ubuntu@82.156.194.147"
PROJECT_DIR="~/projects/delayMirror"
BRANCH="dev"

current_branch=$(git branch --show-current)

if [ "$current_branch" != "$BRANCH" ]; then
    echo "Current branch is '$current_branch', skipping remote build (only '$BRANCH' triggers build)"
    exit 0
fi

echo "=========================================="
echo "Pushing to origin..."
echo "=========================================="
git push origin "$BRANCH"

if [ $? -ne 0 ]; then
    echo "Push failed, aborting remote build"
    exit 1
fi

echo ""
echo "=========================================="
echo "Triggering remote build on $REMOTE..."
echo "=========================================="
ssh -i "$SSH_KEY" "$REMOTE" "source ~/.cargo/env && cd $PROJECT_DIR && git pull origin $BRANCH && cargo build --release --features server"

echo ""
echo "=========================================="
echo "Build complete!"
echo "=========================================="
