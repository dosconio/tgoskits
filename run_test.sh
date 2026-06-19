#!/bin/bash
set -e
cd /home/phina/Documents/tgoskits

echo "=== Starting build and run ==="

# Kill any existing qemu processes
pkill -9 qemu-system-x86_64 2>/dev/null || true
sleep 1

# Run the axvisor
timeout 30 cargo xtask axvisor qemu --config qemu-x86_64-uefi --arch x86_64 --qemu-config os/axvisor/configs/qemu/qemu-x86_64-uefi.toml --vmconfigs os/axvisor/vms/uefi-x86_64-qemu.toml --smp 4 2>&1 | tee /tmp/axvisor5.log

echo "=== Done, exit code: $? ==="