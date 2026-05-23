# Stage 1: 扩展启动配置 — 实施总结

QEMU 启动 → SeaBIOS（QEMU 固件）→ 加载 Axvisor 内核 → Axvisor 启动
         → 创建 VM → 加载 OVMF（UEFI 固件）→ VM guest 启动

## 目标

将 Axvisor 的启动方式从单一的 BIOS/trampoline 模式扩展为可配置的双模式启动：
- `boot_mode = "trampoline"`：传统 BIOS 方式（默认，向后兼容）
- `boot_mode = "uefi"`：UEFI 方式，支持 OVMF 固件加载

当用户配置 `boot_mode = "uefi"` 时，Axvisor 应当：
1. 加载 OVMF_CODE.fd 和 OVMF_VARS.fd 而非 axvm-bios.bin
2. 将它们映射到 pflash 语义的 GPA 区域
3. 为 vCPU 设置正确的初始状态（从 x86 reset vector 0xFFFFFFF0 开始执行）

---

## 修改的文件

### 1. `components/axvmconfig/src/lib.rs`

**新增类型：**

- `VMBootMode` 枚举：`Trampoline`（默认）和 `Uefi`，支持 TOML 反序列化（`#[serde(rename = "trampoline")]` / `#[serde(rename = "uefi")]`）
- `PflashConfig` 结构体：描述一个 pflash 区域的配置
  - `path: String` — 固件镜像文件路径
  - `base_gpa: usize` — 在客户机物理地址空间中的基地址
  - `size: usize` — 区域大小
  - `read_only: bool` — 是否只读（默认 false）

**扩展 VMKernelConfig：**

- `boot_mode: VMBootMode` — 启动模式，默认 `Trampoline`
- `pflash0: Option<PflashConfig>` — pflash0（OVMF_CODE）配置
- `pflash1: Option<PflashConfig>` — pflash1（OVMF_VARS）配置

### 2. `components/axvmconfig/src/templates.rs`

在 `VMKernelConfig` 构造中补充新字段的默认值：
- `boot_mode: Default::default()`
- `pflash0: None`
- `pflash1: None`

### 3. `components/axvmconfig/src/test.rs`

在测试用例的 `VMKernelConfig` 构造中补充新字段默认值，确保测试编译通过。

### 4. `components/axvm/src/config.rs`

**扩展 AxVCpuConfig：**

- 新增 `boot_mode: VMBootMode` 字段

**扩展 VMImageConfig：**

- 新增 `pflash0_load_gpa: Option<GuestPhysAddr>` — pflash0 加载地址
- 新增 `pflash1_load_gpa: Option<GuestPhysAddr>` — pflash1 加载地址

**修改 From\<AxVMCrateConfig\> for AxVMConfig：**

- BSP 入口地址根据 `boot_mode` 分支：
  - `Trampoline`：使用 `cfg.kernel.entry_point`（向后兼容）
  - `Uefi`：硬编码为 `0xFFFFFFF0`（x86 reset vector）
- pflash 加载地址从 `PflashConfig.base_gpa` 映射

### 5. `components/axvm/src/vcpu.rs`

新增 x86_64 平台的重新导出：
- `pub use x86_vcpu::X86VCpuSetupConfig as AxVCpuSetupConfig`

### 6. `components/axvm/src/vm.rs`

在 VM 初始化的 vCPU setup 阶段，根据 `boot_mode` 构造 `X86VCpuSetupConfig`：
```rust
#[cfg(target_arch = "x86_64")]
let setup_config = {
    use axvmconfig::VMBootMode;
    let boot_mode = match inner_mut.config.boot_mode() {
        VMBootMode::Trampoline => x86_vcpu::X86BootMode::Trampoline,
        VMBootMode::Uefi => x86_vcpu::X86BootMode::Uefi,
    };
    crate::vcpu::AxVCpuSetupConfig { boot_mode }
};
```

### 7. `components/x86_vcpu/src/boot_mode.rs`（新建）

将 `X86BootMode` 和 `X86VCpuSetupConfig` 从 `vmx/vcpu.rs` 提取到独立的共享模块，VMX 和 SVM feature 均可使用：
- `X86BootMode` 枚举：`Trampoline`（默认）和 `Uefi`
- `X86VCpuSetupConfig` 结构体：包含 `boot_mode` 字段

### 8. `components/x86_vcpu/src/lib.rs`

- 新增 `mod boot_mode`
- 顶层导出 `pub use boot_mode::{X86BootMode, X86VCpuSetupConfig}`
- 移除之前从 `vmx` 模块的重新导出

### 9. `components/x86_vcpu/src/vmx/vcpu.rs`

- 导入改为 `use crate::boot_mode::{X86BootMode, X86VCpuSetupConfig}`
- `setup_vmcs()` 和 `setup_vmcs_guest()` 新增 `boot_mode: X86BootMode` 参数
- UEFI 模式下设置 CS 基地址为 `0xFFFF0000`，使 `CS_base + IP = 0xFFFF0000 + 0xFFF0 = 0xFFFFFFF0`
- `AxArchVCpu::SetupConfig` 从 `()` 改为 `X86VCpuSetupConfig`
- 公共 `setup()` 方法签名同步更新

### 10. `components/x86_vcpu/src/vmx/mod.rs`

移除 `pub use crate::boot_mode::{X86BootMode, X86VCpuSetupConfig}`（已由 lib.rs 顶层导出）。

### 11. `components/x86_vcpu/src/svm/vcpu.rs`

- 导入新增 `crate::boot_mode::X86VCpuSetupConfig`
- `AxArchVCpu::SetupConfig` 从 `()` 改为 `X86VCpuSetupConfig`（SVM 暂不实现 UEFI 特殊逻辑，仅统一接口）

### 12. `os/axvisor/src/vmm/images/mod.rs`

**扩展 ImageLoader：**

- 新增 `pflash0_load_gpa` 和 `pflash1_load_gpa` 字段
- `load_vm_images()` 根据 `boot_mode` 分支：
  - `Trampoline`：走原有的 kernel → ramdisk → dtb → bios 加载流程
  - `Uefi`：调用新增的 `load_vm_images_uefi()` 方法
- 新增 `load_vm_images_uefi()` 方法：
  - 加载 OVMF_CODE（pflash0）到配置的 GPA
  - 加载 OVMF_VARS（pflash1）到配置的 GPA
  - 可选加载内核和 ramdisk（供 OVMF 后续引导使用）

---

## 遇到的问题及解决

### 问题 1：SVM feature 编译失败 — `X86BootMode` 和 `X86VCpuSetupConfig` 未导出

**现象**：`cargo xtask clippy --package axvm` 在 SVM feature 下报错：
```
error[E0432]: unresolved import `x86_vcpu::X86VCpuSetupConfig`
error[E0433]: cannot find `X86BootMode` in `x86_vcpu`
```

**原因**：最初将 `X86BootMode` 和 `X86VCpuSetupConfig` 定义在 `vmx/vcpu.rs` 中，仅在 VMX feature 下导出。SVM feature 无法访问这些类型，但 `axvm` 的 `vm.rs` 需要为 x86_64 平台统一使用它们。

**解决**：将 `X86BootMode` 和 `X86VCpuSetupConfig` 提取到独立的 `boot_mode.rs` 模块，该模块不受 VMX/SVM feature gate 限制。`lib.rs` 顶层导出，VMX 和 SVM 均可使用。

### 问题 2：`vmx::vcpu` 模块是私有的

**现象**：`lib.rs` 中 `pub use vmx::vcpu::{X86BootMode, X86VCpuSetupConfig}` 报错 `module vcpu is private`。

**原因**：`vmx/mod.rs` 中 `vcpu` 模块声明为 `mod vcpu`（私有），虽然通过 `pub use` 重新导出了类型，但通过 `vmx::vcpu::Xxx` 路径访问仍然不合法。

**解决**：改为通过 `vmx` 模块的重新导出访问，最终直接从 `boot_mode` 模块导出，不再经过 `vmx`。

### 问题 3：`setup_vmcs` 参数不匹配

**现象**：`VmxVcpu::setup()` 公共方法调用 `setup_vmcs(entry, ept_root)` 时缺少新增的 `boot_mode` 参数。

**原因**：修改了 `setup_vmcs` 的签名但遗漏了 `setup()` 方法中的调用点。

**解决**：同步更新 `setup()` 方法签名，增加 `boot_mode` 参数。

### 问题 4：Write 工具意外截断文件

**现象**：使用 Write 工具修改 `svm/vcpu.rs` 和 `vmx/vcpu.rs` 时，只写入了部分内容，导致 800+ 行的文件被截断为 25 行。

**原因**：Write 工具会覆盖整个文件，当时只提供了文件头部的修改内容，未包含文件其余部分。

**解决**：通过 `git checkout --` 恢复文件，改用 Edit 工具进行精确的局部修改。

### 问题 5：`axvmconfig` crate 未链接到 `axvisor`

**现象**：编译 `axvisor` 时报错：
```
error[E0432]: unresolved import `axvmconfig`
  --> os/axvisor/src/vmm/images/mod.rs:20:5
   |
20 | use axvmconfig::VMBootMode;
   |     ^^^^^^^^^^ use of unresolved module or unlinked crate `axvmconfig`
```

**原因**：`axvmconfig` 不是 `axvisor` 的直接依赖（`Cargo.toml` 中未声明），而是通过 `axvm` 间接依赖。`images/mod.rs` 中直接 `use axvmconfig::VMBootMode` 导致编译器无法解析。

**解决**：`axvm::config` 模块已经重新导出了 `VMBootMode`，修改导入路径：
```rust
// 错误
use axvmconfig::VMBootMode;

// 正确
use axvm::config::VMBootMode;
```

### 问题 6：`ax_err_type!` 宏返回类型不匹配

**现象**：编译时报错：
```
error[E0308]: mismatched types
   --> os/axvisor/src/vmm/images/mod.rs:218:20
    |
218 |               return ax_errno::ax_err_type!(
    |  ____________________^
219 | |                 InvalidInput,
220 | |                 "UEFI boot mode requires pflash0 (OVMF_CODE) configuration"
221 | |             );
    | |_____________^ expected `Result<(), AxError>`, found `AxError`
```

**原因**：`ax_err_type!` 宏返回 `AxError` 类型，而非 `Result<AxError>`。函数签名要求返回 `AxResult`（即 `Result<(), AxError>`），直接 `return ax_err_type!(...)` 类型不匹配。

**解决**：将返回值包装在 `Err(...)` 中：
```rust
// 错误
return ax_errno::ax_err_type!(InvalidInput, "...");

// 正确
return Err(ax_errno::ax_err_type!(InvalidInput, "..."));
```

或者使用 `ax_bail!` 宏（内部已包装 `Err`）：
```rust
ax_errno::ax_bail!(InvalidInput, "...");
```

---

## 实现的功能

1. **双模式启动配置**：TOML 配置文件中可通过 `boot_mode = "uefi"` 或 `boot_mode = "trampoline"`（默认）选择启动方式
2. **pflash 区域描述**：通过 `pflash0` 和 `pflash1` 配置项描述 OVMF 固件在客户机地址空间中的位置、大小和读写属性
3. **vCPU UEFI 初始状态**：UEFI 模式下 vCPU 从 x86 reset vector（0xFFFFFFF0）启动，CS 基地址设为 0xFFFF0000
4. **OVMF 固件加载**：ImageLoader 在 UEFI 模式下自动加载 OVMF_CODE 和 OVMF_VARS 到配置的 GPA 区域
5. **向后兼容**：不配置 `boot_mode` 时默认为 `Trampoline`，所有现有配置和行为不变

---

## 验证方法

### 1. 编译验证

确认所有修改的包通过 clippy 检查：

```bash
cargo xtask clippy --package axvmconfig
cargo xtask clippy --package x86_vcpu
cargo xtask clippy --package axvm
```

### 2. 配置文件验证

现有的 VM 配置文件位于 **`os/axvisor/configs/vms/`** 目录下。例如：
- `nimbos-x86_64-qemu-smp1.toml` — 使用传统 trampoline 模式

要验证 UEFI 模式，可在该目录下创建一个新的配置文件（如 `uefi-x86_64-qemu.toml`）：

```toml
[base]
id = 1
name = "uefi-vm"
vm_type = 1
cpu_num = 1
phys_cpu_sets = [1]

[kernel]
# Entry point is ignored in UEFI mode; BSP starts at 0xFFFFFFF0 (reset vector)
entry_point = 0x10000000
image_location = "fs"
# Kernel path is empty - OVMF will load kernel from disk (e.g., EFI partition)
kernel_path = ""
kernel_load_addr = 0x10000000

# UEFI boot mode - uses OVMF firmware instead of axvm-bios
boot_mode = "uefi"

# Memory regions MUST be inside [kernel] section, before pflash configs!
# Format: [base_gpa, size, flags, map_type]
# flags: 0x7 = R|W|X, 0x5 = R|X, 0x3 = R|W
# map_type: 0 = MAP_ALLOC, 1 = MAP_IDENTICAL, 2 = MAP_RESERVED
memory_regions = [
  [0x0000_0000, 0x100_0000, 0x7, 0],   # Low RAM 16 MiB R|W|X
  [0x1000_0000, 0x20_0000, 0x5, 0],    # pflash0 (OVMF_CODE) 2 MiB R|X
  [0x1020_0000, 0x20_0000, 0x3, 0],    # pflash1 (OVMF_VARS) 2 MiB R|W
]

# pflash configs use inline table format (not [kernel.pflash0] section)
pflash0 = { path = "/guest/ovmf/OVMF_CODE.fd", base_gpa = 0x10000000, size = 0x200000, read_only = true }
pflash1 = { path = "/guest/ovmf/OVMF_VARS.fd", base_gpa = 0x10200000, size = 0x200000, read_only = false }

[devices]
interrupt_mode = "passthrough"
emu_devices = []
passthrough_devices = [
  ["IO APIC", 0xfec0_0000, 0xfec0_0000, 0x1000, 0x1],
  ["Local APIC", 0xfee0_0000, 0xfee0_0000, 0x1000, 0x1],
  ["HPET", 0xfed0_0000, 0xfed0_0000, 0x1000, 0x1],
]
```

**重要注意事项：**

1. **`memory_regions` 必须在 `[kernel]` section 内部**，不能放在 `[kernel.pflash0]` 之后
2. **`pflash0`/`pflash1` 使用内联表格式**，避免 TOML section 嵌套问题
3. **`kernel_path` 设为空字符串**，因为 UEFI 模式下 OVMF 从磁盘加载内核
4. **内存大小需合理**：QEMU 默认 128 MiB，VM 内存分配不能超过 Hypervisor 可用内存

### 3. 准备 OVMF 固件文件

Hypervisor 运行在 QEMU 内部的裸机环境，访问的是 rootfs 镜像中的文件系统，而非主机的 `/usr/share/OVMF/`。需要将 OVMF 文件复制到 rootfs 镜像中：

```bash
# 挂载 rootfs 镜像
sudo mkdir -p /mnt/rootfs
sudo mount -o loop ~/Documents/tgoskits/tmp/axbuild/rootfs/rootfs-x86_64-alpine.img /mnt/rootfs

# 创建目录并复制 OVMF 文件
sudo mkdir -p /mnt/rootfs/guest/ovmf
sudo cp /usr/share/OVMF/OVMF_CODE.fd /mnt/rootfs/guest/ovmf/
sudo cp /usr/share/OVMF/OVMF_VARS.fd /mnt/rootfs/guest/ovmf/

# 卸载镜像
sudo umount /mnt/rootfs
```

### 4. 运行验证

```bash
# 构建 Axvisor
cargo xtask axvisor build --config os/axvisor/configs/board/qemu-x86_64.toml

# 运行 UEFI 模式
cargo xtask axvisor qemu \
  --config os/axvisor/configs/board/qemu-x86_64.toml \
  --vmconfigs os/axvisor/configs/vms/uefi-x86_64-qemu.toml
```

### 5. 预期结果

Stage 1 验证成功的日志特征：

```
[axvisor::vmm::images:194] Loading VM[1] images in UEFI mode
[axvisor::vmm::images:200] Loading pflash0 (OVMF_CODE) from /guest/ovmf/OVMF_CODE.fd into GPA @0x10000000
[axvisor::vmm::images:228] Loading pflash1 (OVMF_VARS) from /guest/ovmf/OVMF_VARS.fd into GPA @0x10200000
[axvm::vm:177] VM created: id=1
[x86_vcpu::vmx::vcpu:140] [HV] created VmxVcpu(vmcs: PA:0x...)
[axvm::vm:469] Booting VM[1]
[axvisor::vmm:77] VM[1] boot success
[axvisor::vmm::vcpus:449] VM[1] VCpu[0] running...
```

OVMF 开始执行后会触发 `EXCEPTION_NMI`，这是**预期行为**，因为：
- Stage 1 只实现了配置框架和固件加载
- OVMF 需要访问 **fw_cfg 设备**、**ACPI 表**、**PCI 设备** 才能继续
- 这些设备将在 **Stage 2**（fw_cfg、ACPI）和 **Stage 3**（PCI/virtio）实现

### 6. 常见问题排查

| 问题 | 原因 | 解决方案 |
|------|------|----------|
| `missing field 'memory_regions'` | `memory_regions` 放在了 `[kernel.pflash0]` 之后 | 将 `memory_regions` 移到 `[kernel]` section 内部 |
| `Failed to open /usr/share/OVMF/...` | OVMF 文件不在 rootfs 镜像中 | 挂载镜像并复制 OVMF 文件到 `/guest/ovmf/` |
| `memory allocation of ... bytes failed` | VM 内存分配超过 Hypervisor 可用内存 | 减少 `memory_regions` 大小或增加 QEMU 内存 |
| `Failed to translate kernel image load address` | pflash GPA 没有对应的内存区域映射 | 在 `memory_regions` 中添加 pflash 区域 |

### 7. 单元测试验证

```bash
cargo test --package axvmconfig
```

验证 `VMBootMode` 和 `PflashConfig` 的序列化/反序列化正确性。

## 启动顺序

### 当前 Stage 1 启动顺序（不完整）
```
┌─────────────────────────────────────────────────────────────────┐
│  QEMU 启动                                                       │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  SeaBIOS (QEMU 固件)                                             │
│  - 初始化硬件                                                    │
│  - 从 virtio-blk 加载内核                                        │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  Axvisor (Hypervisor) 启动                                       │
│  - 初始化内存、调度器、文件系统                                   │
│  - 启用 VMX 硬件虚拟化                                           │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  创建 VM (boot_mode = "uefi")                                    │
│  - 解析 TOML 配置                                                │
│  - 分配 guest 内存 (memory_regions)                              │
│  - 加载 OVMF_CODE.fd → GPA 0x10000000                           │
│  - 加载 OVMF_VARS.fd → GPA 0x10200000                           │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  vCPU 初始化                                                     │
│  - 设置 CS base = 0xFFFF0000                                     │
│  - 设置 RIP = 0xFFF0 (线性地址 0xFFFFFFF0)                       │
│  - 设置实模式状态                                                │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  VM 启动 → vCPU 开始执行                                         │
│  - 从 x86 reset vector (0xFFFFFFF0) 开始                        │
│  - OVMF 执行前几条指令                                           │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  ❌ OVMF 访问 fw_cfg 设备 → 不存在 → EXCEPTION_NMI               │
│  (Stage 1 缺少 fw_cfg、ACPI、PCI 设备)                           │
└─────────────────────────────────────────────────────────────────┘
```
### 预期完备系统启动顺序（Stage 1 + 2 + 3）
```
┌─────────────────────────────────────────────────────────────────┐
│  QEMU 启动                                                       │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  SeaBIOS (QEMU 固件)                                             │
│  - 初始化硬件                                                    │
│  - 从 virtio-blk 加载内核                                        │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  Axvisor (Hypervisor) 启动                                       │
│  - 初始化内存、调度器、文件系统                                   │
│  - 启用 VMX 硬件虚拟化                                           │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  创建 VM (boot_mode = "uefi")                                    │
│  - 解析 TOML 配置                                                │
│  - 分配 guest 内存 (memory_regions)                              │
│  - 加载 OVMF_CODE.fd → GPA 0x10000000                           │
│  - 加载 OVMF_VARS.fd → GPA 0x10200000                           │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  【Stage 2】生成 ACPI 表并写入 guest 内存                        │
│  - RSDP (Root System Description Pointer) @ 0x000F0000          │
│  - XSDT (Extended System Description Table)                     │
│  - FADT (Fixed ACPI Description Table) → 包含 PM1a_EVT_BLK      │
│  - MADT (Multiple APIC Description Table)                       │
│    • Local APIC 列表 (每个 vCPU 一个)                            │
│    • IO APIC 地址 (0xFEC00000)                                   │
│  - MCFG (Memory Mapped Configuration Space) → PCI ECAM 基地址   │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  【Stage 2】创建 fw_cfg 设备 (QEMU Firmware Configuration)       │
│  - 端口 I/O: 0x510 (selector) / 0x511 (data)                    │
│  - 提供:                                                         │
│    • etc/bootorder - 启动顺序                                    │
│    • etc/system-states - 电源管理                                │
│    • etc/e820 - 内存布局                                         │
│    • file://kernel - 内核镜像                                    │
│    • file://initrd - initramfs                                   │
│    • file://cmdline - 内核命令行                                 │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  【Stage 3】创建 PCI/PCIe 总线模拟                                │
│  - ECAM (Enhanced Configuration Access Mechanism)               │
│  - 虚拟 PCI 设备:                                                │
│    • virtio-blk (块设备)                                         │
│    • virtio-net (网络)                                           │
│    • virtio-console (串口)                                       │
│  - PCI 中断路由 (INTx → GSI → IO APIC)                          │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  【Stage 3】创建中断控制器模拟                                    │
│  - IO APIC (0xFEC00000) - 处理外部中断                           │
│  - Local APIC (0xFEE00000) - 每个 vCPU 一个                      │
│  - 中断路由: PCI INTx → IO APIC → Local APIC → vCPU             │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  vCPU 初始化                                                     │
│  - 设置 CS base = 0xFFFF0000                                     │
│  - 设置 RIP = 0xFFF0 (线性地址 0xFFFFFFF0)                       │
│  - 设置实模式状态                                                │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  VM 启动 → vCPU 开始执行                                         │
│  - 从 x86 reset vector (0xFFFFFFF0) 开始                        │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  OVMF (UEFI 固件) 初始化                                         │
│  - 从实模式切换到保护模式 → 长模式                               │
│  - 初始化 UEFI Boot Services                                     │
│  - 输出 "TianoCore" / "EDK2" 标志                                │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  OVMF 发现硬件 (通过 ACPI 表)                                    │
│  - 读取 RSDP → XSDT → FADT/MADT/MCFG                            │
│  - 发现 Local APIC (MADT) → 每个 vCPU                           │
│  - 发现 IO APIC (MADT) → 中断控制器                              │
│  - 发现 PCI 总线 (MCFG) → 设备枚举                               │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  OVMF 读取启动信息 (通过 fw_cfg 设备)                            │
│  - 读取 etc/e820 → 内存布局                                      │
│  - 读取 file://kernel → 内核镜像                                 │
│  - 读取 file://cmdline → 内核命令行                              │
│  - 读取 file://initrd → initramfs                                │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  OVMF 枚举 PCI 设备                                              │
│  - 发现 virtio-blk → 读取磁盘                                    │
│  - 发现 EFI 系统分区 (ESP)                                       │
│  - 加载 EFI 引导加载程序 (GRUB/systemd-boot)                     │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  EFI 引导加载程序加载 Linux 内核                                 │
│  - 加载 vmlinuz 到内存                                           │
│  - 设置内核命令行                                                 │
│  - 调用 ExitBootServices()                                       │
│  - 跳转到内核入口点                                               │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  Linux 内核启动                                                  │
│  - 初始化内存管理                                                │
│  - 初始化中断控制器 (APIC)                                       │
│  - 初始化 PCI 设备驱动                                           │
│  - 挂载根文件系统                                                 │
│  - 启动 init 进程                                                 │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  ✅ Guest OS 完整启动                                            │
└─────────────────────────────────────────────────────────────────┘
```

## 文档中的配置错误
文档中的示例配置使用了 错误的 GPA 地址 ：

```
# 文档中的错误示例
pflash0 = { path = "/guest/ovmf/OVMF_CODE.fd", base_gpa = 0x10000000, ... }
```
这是错误的！x86 reset vector 在 0xFFFFFFF0 ，pflash 必须映射到高地址才能覆盖这个地址。
