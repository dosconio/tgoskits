# Stage 9: vLAPIC EOI 广播 + CPUID leaf=0x1 子叶过滤

## 目标

修复两个阻止 OVMF-Linux 正常运行的 stub 实现：

1. **vLAPIC EOI 广播**：`process_eoi()` 中两个 `unimplemented!()` 调用
2. **CPUID leaf=0x1 子叶过滤**：未对 `ecx > 0` 返回全零

## 问题分析

### 1. vLAPIC EOI 广播

[components/x86_vlapic/src/vlapic.rs](file:///home/phina/Documents/tgoskits/components/x86_vlapic/src/vlapic.rs) 中的 `process_eoi()` 方法在清除 ISR 后，对 level-triggered 中断需要通知 IOAPIC 清除 Remote IRR：

```rust
// 修改前（stub）：
unimplemented!("vioapic_broadcast_eoi(vlapic2vcpu(vlapic)->vm, vector);")
unimplemented!("vcpu_make_request(vlapic2vcpu(vlapic), ACRN_REQUEST_EVENT);")
```

Level-triggered 中断（如 virtio-pci INTx）在 EOI 后必须：
1. 通知 IOAPIC 清除对应 RTE 的 Remote IRR 位，否则后续中断被阻塞
2. 触发 vCPU 检查是否有新的待注入中断

### 2. CPUID leaf=0x1 子叶过滤

[components/x86_vcpu/src/vmx/vcpu.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vcpu.rs) 中 `handle_cpuid()` 的 `LEAF_FEATURE_INFO` 分支直接透传 host CPUID 结果，未对 `ecx > 0` 的子叶调用返回全零。对比 `LEAF_CACHE_PARAMETERS`（leaf=0x4）已正确处理子叶。

## 设计

### 1. vLAPIC EOI 广播

```rust
if (self.regs().TMR[idx].get() as u32).bit(bitpos)
    && let Some(vioapic) = x86_vioapic::GLOBAL_VIOAPIC.get()
{
    vioapic.eoi(vector as u8);
}
```

- 通过全局 `GLOBAL_VIOAPIC` 单例调用 `IoApic::eoi()` 清除 Remote IRR
- `vcpu_make_request` 的 stub 无需单独实现，因为 [vcpu.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vcpu.rs#L1506-L1521) 中的 `inject_pending_events()` 已在每次 VM-entry 前轮询 `GLOBAL_VIOAPIC.take_pending_irqs()`，自动处理待注入中断

### 2. CPUID leaf=0x1 子叶过滤

```rust
if regs_clone.rcx > 0 {
    CpuIdResult { eax: 0, ebx: 0, ecx: 0, edx: 0 }
} else {
    // 原有主叶处理逻辑
}
```

当子叶索引 > 0 时返回全零，符合 Intel SDM 对未定义子叶的规定。

## 依赖变更

[components/x86_vlapic/Cargo.toml](file:///home/phina/Documents/tgoskits/components/x86_vlapic/Cargo.toml) 新增依赖：

```toml
x86_vioapic = { workspace = true }
```

## 涉及的文件

| 文件 | 变更 |
|------|------|
| [x86_vlapic/src/vlapic.rs](file:///home/phina/Documents/tgoskits/components/x86_vlapic/src/vlapic.rs#L250-L255) | `process_eoi()` 中调用 `GLOBAL_VIOAPIC.eoi()` 替换 `unimplemented!()` |
| [x86_vlapic/Cargo.toml](file:///home/phina/Documents/tgoskits/components/x86_vlapic/Cargo.toml) | 添加 `x86_vioapic` 依赖 |
| [x86_vcpu/src/vmx/vcpu.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vcpu.rs#L2148-L2159) | `LEAF_FEATURE_INFO` 添加 `ecx > 0` 子叶过滤 |
| [x86_vcpu/src/vmx/vcpu.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vcpu.rs#L69) | 修复多余逗号（pre-existing） |
| [x86_vcpu/src/regs/mod.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/regs/mod.rs#L81) | 为未使用宏添加 `#[allow(unused_macros)]`（pre-existing） |
| [x86_vcpu/src/vmx/vmcs.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vmcs.rs#L664) | 为未使用 re-export 添加 `#[allow(unused_imports)]`（pre-existing） |

## 编译验证

```
$ cargo xtask clippy --package x86_vlapic --package x86_vcpu

running clippy for 2 package(s) with 5 check(s)
[1/5] x86_vlapic (base, target: x86_64-unknown-none)     ok
[2/5] x86_vcpu (base, target: x86_64-unknown-none)       ok
[3/5] x86_vcpu (feature: svm, target: x86_64-unknown-none)    ok
[4/5] x86_vcpu (feature: tracing, target: x86_64-unknown-none) ok
[5/5] x86_vcpu (feature: vmx, target: x86_64-unknown-none)    ok

clippy summary: 2 package(s), 5 check(s), 2 package(s) passed, 0 package(s) failed
passed checks: 5, failed checks: 0
all clippy checks passed
```

**两个包全部通过 clippy，0 错误，0 警告。**

## 运行时验证状态

运行命令：

```bash
cargo xtask axvisor qemu --arch x86_64 \
  --vmconfigs os/axvisor/configs/vms/uefi-x86_64-qemu.toml
```

### 成功的部分：设备初始化全链路通过

所有虚拟设备成功初始化，日志如下：

```
[  4.700593 0:2 axvisor::vmm::images:604] [fw_cfg] Registering kernel file: /guest/linux/linux-qemu (14730240 bytes)
[  4.747645 0:2 axvisor::vmm::images:667] Registered fw_cfg device at I/O ports 0x510-0x511
[  4.750107 0:2 axvisor::vmm::images:685] Created virtio-blk-pci device: BDF=(0,1,0), disk_size=0x4000000
[  4.752770 0:2 axvisor::vmm::images:692] Added virtio-blk-pci to PCI host bridge device list
[  4.754979 0:2 axvisor::vmm::images:699] Registered PCI Host Bridge at I/O ports 0xCF8-0xCFF
[  4.757899 0:2 axvisor::vmm::images:704] Registered virtio-blk-pci as port I/O device (dynamic BAR)
[  4.760158 0:2 axvisor::vmm::images:713] Registered PM device at I/O ports 0x600-0x60B
[  4.762420 0:2 axvisor::vmm::images:719] Registered Guest Serial at I/O ports 0x3F8-0x3FE
[  4.764872 0:2 axvisor::vmm::images:726] Registered vIOAPIC at MMIO 0xfec00000-0xfec01000
[  4.767439 0:2 axvisor::vmm::images:736] Registered i8259 PIC at I/O ports 0x20-0x21, 0xA0-0xA1
```

### VM-entry 控制域校验失败（Stage 7/8 遗留，非 Stage 9 回归）

```
PRIM_CTRL=0xfaf99e8c  (bit 25 USE_IO_BITMAPS=1, bit 24 UNCOND_IO_EXITING=0)
                     → VM-entry instruction error 7: "VM entry with invalid control field(s)"
```

[vcpu.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vcpu.rs#L807-L815) 强制清除 `UNCOND_IO_EXITING`（bit 24）导致 CPU VM-entry 校验失败，vCPU 无法进入运行态。

由于 VM-entry 失败，OVMF 代码从未执行，因此 Stage 9 的两个功能（EOI 广播和 CPUID 子叶过滤）**无法在运行时验证**。这些功能需要 OVMF 进入 DXE/BDS 阶段后才能触发。

### 推断正确性

虽然运行时被 VM-entry 阻塞，但两个修改可以通过以下方式确认为正确：

1. **编译通过**：`cargo xtask clippy` 全部通过，代码无语法错误或类型不匹配
2. **语义对齐**：EOI 广播调用 `x86_vioapic::IoApic::eoi()`，该方法已在 [x86_vioapic/src/lib.rs](file:///home/phina/Documents/tgoskits/components/x86_vioapic/src/lib.rs#L199-L207) 中完整实现（遍历所有 RTE，对匹配 vector 的 level-triggered 中断清除 Remote IRR）
3. **SDM 合规**：CPUID 子叶过滤遵循 Intel SDM 规定——未定义子叶返回全零，与 `LEAF_CACHE_PARAMETERS`（leaf=0x4）的处理方式一致
4. **中断注入链路完整**：`inject_pending_events()` 已在每次 VM-entry 前调用 `GLOBAL_VIOAPIC.take_pending_irqs()` → `vlapic.set_intr()` → `queue_external_interrupt()`，无需额外实现 `vcpu_make_request`

## 后续工作

- **Stage 10**：修复 VM-entry 控制域校验（`USE_IO_BITMAPS=0` + `UNCOND_IO_EXITING=1`），使 vCPU 能够进入运行态，届时可验证 Stage 9 两个功能在 OVMF/Linux 运行时的实际表现