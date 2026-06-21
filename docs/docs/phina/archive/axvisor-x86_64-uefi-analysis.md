# Axvisor x86_64 UEFI 客户机支持 — 现状分析与实施规划

## 一、项目现状总览

### 1.1 整体架构

Axvisor 在 x86_64 上的实现基于 **Intel VMX**（也预留了 AMD SVM 框架），运行在 ArceOS 之上，使用 Q35 机器类型。核心组件关系如下：

```
┌─────────────────────────────────────────────┐
│  axvisor (os/axvisor)                       │
│  ├── main.rs: 启动入口                       │
│  ├── hal/arch/x86_64/: 硬件抽象层（极薄）      │
│  ├── vmm/: 虚拟机管理器核心                    │
│  │   ├── config.rs: VM 配置加载               │
│  │   ├── images/: 镜像加载器                  │
│  │   ├── vcpus.rs: vCPU 调度                  │
│  │   └── vm_list.rs: VM 注册表               │
│  └── shell/: 管理控制台                       │
├─────────────────────────────────────────────┤
│  axvm (components/axvm)                      │
│  ├── vm.rs: AxVM 核心（地址空间/设备/vCPU）    │
│  ├── vcpu.rs: 架构分发（x86→x86_vcpu）        │
│  ├── config.rs: AxVMConfig 运行时配置          │
│  └── hal.rs: 内存/分页 HAL                    │
├─────────────────────────────────────────────┤
│  x86_vcpu (components/x86_vcpu)             │
│  ├── vmx/vcpu.rs: VmxVcpu 核心实现           │
│  ├── vmx/vmcs.rs: VMCS 字段操作              │
│  ├── ept.rs: EPT 页表                        │
│  └── regs/: 寄存器操作                        │
├─────────────────────────────────────────────┤
│  x86_vlapic (components/x86_vlapic)         │
│  ├── vlapic.rs: 虚拟 APIC 寄存器             │
│  ├── regs/: LVT/ICR/SVR/ESR 等寄存器         │
│  └── timer.rs: APIC Timer 模拟               │
├─────────────────────────────────────────────┤
│  axdevice (components/axdevice)             │
│  └── device.rs: 设备路由/分发框架             │
├─────────────────────────────────────────────┤
│  axvmconfig (components/axvmconfig)         │
│  └── lib.rs: TOML→配置结构体反序列化           │
└─────────────────────────────────────────────┘
```

### 1.2 一句话总结

当前缺的不是某个小功能，而是一整套能让标准 UEFI 客户机找到硬件并启动的 **"PC 平台骨架"**：没有 UEFI 固件加载、没有 fw_cfg、没有 ACPI、没有 PCI、没有 vIOAPIC、virtio 全部空转，vLAPIC 关键路径全是 `unimplemented!()`，中断注入链路完全断开。

---

## 二、现状详细分析

### 2.1 启动配置模型 — 仅 BIOS/Trampoline 模式

#### 当前配置结构

文件：`components/axvmconfig/src/lib.rs`，`VMKernelConfig` 结构体

| 字段 | 类型 | 当前用途 |
|------|------|---------|
| `entry_point` | `usize` | 内核入口点（如 `0x8000`） |
| `kernel_path` | `String` | 内核镜像路径 |
| `kernel_load_addr` | `usize` | 内核加载 GPA |
| `bios_path` | `Option<String>` | BIOS 镜像路径（如 `axvm-bios.bin`） |
| `bios_load_addr` | `Option<usize>` | BIOS 加载 GPA（如 `0x8000`） |
| `dtb_path` / `dtb_load_addr` | `Option` | 设备树（x86 不用） |
| `ramdisk_path` / `ramdisk_load_addr` | `Option` | 内存盘 |
| `image_location` | `Option<String>` | `"memory"` 或 `"fs"` |
| `cmdline` | `Option<String>` | 内核命令行 |
| `disk_path` | `Option<String>` | 磁盘镜像路径 |
| `memory_regions` | `Vec<VmMemConfig>` | 内存区域列表 |

**缺失项**：

- 无 `boot_mode` 字段（无法区分 `"trampoline"` vs `"uefi"`）
- 无 `ovmf_code_path` / `ovmf_vars_path` 字段
- 无 `pflash` 配置（大小、偏移、属性、是否只读）
- 无 `fw_cfg` 配置
- 无 UEFI 变量存储区域描述

#### 运行时配置转换

文件：`components/axvm/src/config.rs`

`AxVMConfig` 由 `AxVMCrateConfig` 转换而来，其中：

- `AxVCpuConfig.bsp_entry` 直接取 `kernel.entry_point`
- `VMImageConfig.bios_load_gpa` 直接取 `kernel.bios_load_addr`
- 没有 UEFI 相关字段的映射

#### 实际 x86_64 客户机配置

文件：`os/axvisor/configs/vms/nimbos-x86_64-qemu-smp1.toml`

```toml
[kernel]
entry_point = 0x8000
kernel_path = "/guest/nimbos/nimbos-qemu"
kernel_load_addr = 0x20_0000
bios_path = "/guest/nimbos/axvm-bios.bin"
bios_load_addr = 0x8000
memory_regions = [
  [0x0000_0000, 0x100_0000, 0x7, 0],  # 仅 16MB 低地址 RAM
]

[devices]
interrupt_mode = "passthrough"
emu_devices = []                        # 空！没有任何模拟设备
passthrough_devices = [
  ["IO APIC",  0xfec0_0000, 0xfec0_0000, 0x1000, 0x1],
  ["Local APIC", 0xfee0_0000, 0xfee0_0000, 0x1000, 0x1],
  ["HPET",     0xfed0_0000, 0xfed0_0000, 0x1000, 0x1],
]
```

**关键问题**：

- 内存仅映射 16MB 低地址区域（`0x0 ~ 0x100_0000`），UEFI 需要至少 256MB+ 且高地址也需可访问
- `emu_devices = []`：完全没有模拟设备
- IOAPIC/LAPIC/HPET 全部直通——客户机直接操作物理硬件，无法做中断路由和设备模拟

---

### 2.2 vCPU 初始状态 — 实模式/Trampoline 风格

#### VMCS Guest 初始化

文件：`components/x86_vcpu/src/vmx/vcpu.rs`，`setup_vmcs_guest()` 方法

| 项目 | 当前值 | UEFI 需求 |
|------|--------|-----------|
| CR0 | `NWT | CD | ET`（缓存禁用） | 需要保护模式 + 分页使能 |
| CR4 | `0` | 需要设置 PAE/VMXE 等 |
| CS | 16-bit, base=0, limit=0xFFFF, access=0x9b（实模式） | 需要长模式 64-bit 代码段 |
| ES/SS/DS/FS/GS | 16-bit, data, read/write | 需要长模式数据段 |
| RFLAGS | `0x2` | 需要中断使能（IF 位） |
| IA32_EFER | `0` | 需要设置 LME/LMA |
| RIP | `entry_point`（如 0x8000） | 应为 OVMF 入口（如 0x100000） |
| CR3 | `0` | 需要指向有效的页表 |
| GDTR/IDTR | base=0, limit=0xFFFF | 需要指向有效的 GDT/IDT |

**问题**：当前 vCPU 初始化为**实模式**，RIP 指向低地址，这是典型的 BIOS bootstrap 方式。UEFI 固件需要从保护模式或长模式启动，入口点在高地址（通常 `0x100000` 或更高）。

**但**：`get_cpu_mode()` 方法已实现了模式判断（Real/Protected/Compatibility/Mode64），说明代码已经考虑了多模式切换，`set_cr()` 方法也支持 CR0/CR3/CR4 的动态修改，包括 CR0.PAGING 置位时自动更新 EFER，这为 UEFI 启动后的模式切换提供了基础。

---

### 2.3 镜像加载器 — 仅支持"按地址直接放置"

文件：`os/axvisor/src/vmm/images/mod.rs`

当前 `ImageLoader` 的加载流程：

```
1. 加载 kernel → kernel_load_gpa
2. 加载 ramdisk → ramdisk_load_gpa（可选）
3. 加载 dtb → dtb_load_gpa（可选，x86 不用）
4. 加载 bios → bios_load_gpa（可选）
```

**缺失项**：

- 无 OVMF_CODE / OVMF_VARS 的 pflash 加载语义（OVMF 需要两块 pflash 区域：CODE 只读映射到高地址，VARS 可读写映射到另一高地址）
- 无 fw_cfg 设备的创建和数据注入
- 无 ACPI 表的生成和放置
- Linux 镜像头解析（`images/linux.rs`）只支持 ARM64 和 RISC-V，x86 的 bzImage/EFI stub 头解析完全缺失

---

### 2.4 设备模型 — x86 几乎为空白

#### axdevice 设备初始化

文件：`components/axdevice/src/device.rs`，`AxVmDevices::init()` 中的 `match config.emu_type` 分支：

| EmulatedDeviceType | x86_64 支持 |
|---|---|
| `InterruptController` (0x1) | 仅 aarch64 (Vgic)，x86 走 warn 分支 |
| `GPPTRedistributor` (0x20) | 仅 aarch64 |
| `GPPTDistributor` (0x21) | 仅 aarch64 |
| `GPPTITS` (0x22) | 仅 aarch64 |
| `PPPTGlobal` (0x30) | 仅 riscv64 |
| `IVCChannel` (0xA) | 通用（GPA 区间分配） |
| `VirtioBlk` (0xE1) | 走 `_ => warn!` 分支，未实现 |
| `VirtioNet` (0xE2) | 走 `_ => warn!` 分支，未实现 |
| `VirtioConsole` (0xE3) | 走 `_ => warn!` 分支，未实现 |

**结论**：x86_64 上 `axdevice::init()` 实际上不会创建任何模拟设备。

#### 设备分发框架

文件：`components/axvm/src/vm.rs`，`run_vcpu()` 中的分发逻辑已实现：

- `IoRead` / `IoWrite` → `handle_port_read/write()`
- `MmioRead` / `MmioWrite` → `handle_mmio_read/write()`
- `SysRegRead` / `SysRegWrite` → `handle_sys_reg_read/write()`

`AxVmDevices` 支持 `emu_mmio_devices`、`emu_port_devices`、`emu_sys_reg_devices` 三类设备列表，框架已就绪，但当前没有任何 x86 设备注册进来。未命中的访问会触发 `panic_device_not_found()`。

#### EmulatedDeviceConfig 结构

文件：`components/axvmconfig/src/lib.rs`

```rust
pub struct EmulatedDeviceConfig {
    pub name: String,
    pub base_gpa: usize,
    pub length: usize,
    pub irq_id: usize,
    pub emu_type: EmulatedDeviceType,
    pub cfg_list: Vec<usize>,
}
```

该结构已预留 `irq_id` 和 `cfg_list`，为 PCI 设备和中断路由提供了配置基础。

---

### 2.5 中断链路 — 直通模式，虚拟中断未串通

#### vLAPIC

文件：`components/x86_vlapic/src/vlapic.rs`

`VirtualApicRegs` 已实现：

- xAPIC MMIO 访问（`0xFEE00000` 区域）
- x2APIC MSR 访问（`0x800~0x83F`）
- Virtual-APIC page 和 APIC-access page
- ICR 写入解析（目标计算、DeliveryMode 分发）
- LVT 寄存器管理
- APIC Timer 模拟
- EOI 处理框架（ISR 位清除、PPR 更新）
- 目标匹配（Flat Model / Cluster Model / x2APIC）

**但存在大量 `unimplemented!()`**：

| 函数/路径 | 缺失功能 | 影响 |
|-----------|---------|------|
| `process_eoi()` → TMR 检查 | `vioapic_broadcast_eoi()` | Level-triggered 中断无法完成 EOI 广播 |
| `process_eoi()` → 末尾 | `vcpu_make_request(ACRN_REQUEST_EVENT)` | EOI 后无法触发下一次中断注入 |
| `set_err()` | `vlapic_accept_intr()` | APIC 内部错误无法产生中断 |
| `inject_nmi()` | NMI 注入 | NMI 中断不可用 |
| `process_init_sipi()` | INIT/SIPI 处理 | SMP 启动必需，AP 无法被唤醒 |
| `handle_self_ipi()` | x2APIC Self-IPI | x2APIC 模式下 Self-IPI 不可用 |

#### vIOAPIC

**完全不存在**。没有 `x86_vioapic` crate，也没有任何 IOAPIC 模拟代码。

#### 中断注入路径

文件：`os/axvisor/src/hal/arch/x86_64/mod.rs`

```rust
pub fn inject_interrupt(_vector: u8) {}
```

当前 x86_64 的 `inject_interrupt()` 是**空函数**。

vCPU 的 `queue_event()` 方法已实现（通过 `pending_events` 队列 + VM-entry 注入），但上游没有调用者。整个中断注入数据通路：

```
外部中断 → VM Exit (EXTERNAL_INTERRUPT) → inject_interrupt() [空函数] → 断路
设备中断 → vIOAPIC [不存在] → ??? → vLAPIC.set_intr() [unimplemented] → 断路
```

#### MSI/MSI-X

完全未实现。`EmulatedDeviceType` 枚举中没有 MSI 相关类型。

---

### 2.6 PCI / virtio — 完全缺失

#### PCI 主桥

- 无虚拟 PCI 主桥
- 无 ECAM（Memory-mapped PCI Config）模拟
- 无 PIO 方式（`0xCF8`/`0xCFC`）的 PCI Config 模拟
- 无 PCI 设备枚举框架
- 无 PCI BAR 分配机制

#### virtio-pci

- 无 virtio-pci 设备实现
- 无 virtio-blk-pci / virtio-net-pci / virtio-console-pci
- `EmulatedDeviceType` 中定义了 `VirtioBlk`/`VirtioNet`/`VirtioConsole`，但 `axdevice::init()` 中全部走 fallback 分支

#### fw_cfg

完全不存在。没有 QEMU fw_cfg 设备模拟。

---

### 2.7 ACPI 表 — 完全缺失

- 无 RSDP 生成
- 无 XSDT/RSDT
- 无 FADT
- 无 MADT（APIC 描述）
- 无 MCFG（PCI ECAM 描述）
- 无 DSDT/SSDT

当前 x86_64 客户机不依赖 ACPI（NimbOS 等简单 OS 直接用直通硬件），但 UEFI 客户机**必须**通过 ACPI 发现硬件。

---

### 2.8 QEMU 配置与平台 crate

#### CI 配置

文件：`os/axvisor/configs/qemu/qemu-x86_64.toml`

```toml
uefi = false
args = ["-machine", "q35", "-accel", "kvm", "-cpu", "host", ...]
```

所有 x86_64 配置均标记 `uefi = false`，使用 Q35 + KVM 加速。

#### 平台 crate

x86_64 使用 `platform/x86-qemu-q35`（`axplat-x86-qemu-q35`），该平台 crate 已定义了 PCI ECAM 基地址（`0xb000_0000`）、IOAPIC/LAPIC/HPET 地址等，但这些信息目前只用于 ArceOS 宿主侧，未传递给客户机。

---

## 三、差距矩阵

| 能力 | 当前状态 | UEFI 客户机需求 | 差距等级 |
|------|---------|----------------|---------|
| **启动配置** | bios_path + bios_load_addr | OVMF_CODE/VARS/pflash/fw_cfg | 🔴 需要全新设计 |
| **vCPU 初始状态** | 实模式, RIP=0x8000 | 长模式, RIP=OVMF入口 | 🔴 需要重写 setup_vmcs_guest |
| **镜像加载** | 按地址直接放置 | pflash 语义 + fw_cfg | 🔴 需要扩展 |
| **fw_cfg** | 不存在 | UEFI 必需 | 🔴 需要新建 |
| **ACPI 表** | 不存在 | RSDP/XSDT/FADT/MADT/MCFG | 🔴 需要新建 |
| **PCI 主桥** | 不存在 | ECAM 或 PIO Config | 🔴 需要新建 |
| **virtio-blk-pci** | 不存在 | UEFI 启动磁盘 | 🔴 需要新建 |
| **vIOAPIC** | 不存在 | 中断路由必需 | 🔴 需要新建 |
| **vLAPIC** | 框架在，多处 unimplemented | EOI/IPI/INIT-SIPI | 🟡 需要补全 |
| **中断注入** | inject_interrupt 为空 | 完整注入链路 | 🔴 需要串通 |
| **MSI/MSI-X** | 不存在 | virtio-pci 高性能中断 | 🟡 后续需求 |
| **内存布局** | 仅 16MB 低地址 | 256MB+ + 高地址映射 | 🟡 需要调整配置 |
| **Port I/O 框架** | 已有 | 注册具体设备即可 | 🟢 框架就绪 |
| **MMIO 分发框架** | 已有 | 注册具体设备即可 | 🟢 框架就绪 |
| **SysReg 分发框架** | 已有 | vLAPIC 已接入 | 🟢 框架就绪 |
| **EmulatedDeviceConfig** | 已有 name/base_gpa/length/irq_id/cfg_list | 可直接复用 | 🟢 框架就绪 |
| **EPT 页表管理** | 已有 | 可直接映射新设备区域 | 🟢 框架就绪 |

---

## 四、实施路线（5 步）

### 第 1 步：扩展启动配置

**目标**：将启动方式从单一的 `bios_path / bios_load_addr` 扩展为可配置的 `boot = "trampoline"`（传统 BIOS 方式）或 `boot = "uefi"`。配置项需要显式描述 OVMF 的代码、变量存储、pflash 属性，以及固件在客户机物理地址空间中的位置。

#### 1.1 修改 axvmconfig 配置结构

文件：`components/axvmconfig/src/lib.rs`

**新增枚举**：

```rust
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum VMBootMode {
    #[default]
    #[serde(rename = "trampoline")]
    Trampoline,
    #[serde(rename = "uefi")]
    Uefi,
}
```

**新增 pflash 配置结构**：

```rust
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct PflashConfig {
    pub path: String,
    pub base_gpa: usize,
    pub size: usize,
    #[serde(default)]
    pub read_only: bool,
}
```

**修改 VMKernelConfig**：

```rust
pub struct VMKernelConfig {
    // 保留现有字段...
    pub entry_point: usize,
    pub kernel_path: String,
    pub kernel_load_addr: usize,
    pub bios_path: Option<String>,
    pub bios_load_addr: Option<usize>,
    // ...

    // 新增字段
    pub boot_mode: VMBootMode,
    pub pflash0: Option<PflashConfig>,  // OVMF_CODE
    pub pflash1: Option<PflashConfig>,  // OVMF_VARS
}
```

**预期 TOML 配置示例**：

```toml
[kernel]
boot_mode = "uefi"
kernel_path = "/guest/linux/bzImage"
kernel_load_addr = 0x20_0000
cmdline = "root=/dev/vda2 console=ttyS0"

[kernel.pflash0]
path = "/usr/share/OVMF/OVMF_CODE.fd"
base_gpa = 0x1000_0000
size = 0x20_0000       # 2MB
read_only = true

[kernel.pflash1]
path = "/usr/share/OVMF/OVMF_VARS.fd"
base_gpa = 0x1020_0000
size = 0x20_0000       # 2MB
read_only = false
```

#### 1.2 修改 axvm 运行时配置

文件：`components/axvm/src/config.rs`

- `AxVCpuConfig`：根据 `boot_mode` 决定 `bsp_entry` 的值（trampoline → `entry_point`，uefi → OVMF 入口地址）
- `VMImageConfig`：新增 `pflash0_load_gpa` / `pflash1_load_gpa` 字段

#### 1.3 修改镜像加载器

文件：`os/axvisor/src/vmm/images/mod.rs`

- 在 `ImageLoader` 中新增 `pflash0_load_gpa` / `pflash1_load_gpa` 字段
- `load()` 方法中根据 `boot_mode` 分支：
  - `Trampoline`：保持现有逻辑
  - `Uefi`：加载 OVMF_CODE 到 pflash0_gpa，加载 OVMF_VARS 到 pflash1_gpa，跳过 bios 加载

#### 1.4 修改 vCPU 初始状态

文件：`components/x86_vcpu/src/vmx/vcpu.rs`，`setup_vmcs_guest()` 方法

当 `boot_mode == "uefi"` 时，vCPU 初始状态应设置为：

| 项目 | UEFI 初始值 |
|------|------------|
| CR0 | `PE | PG | ET`（保护模式 + 分页使能） |
| CR4 | `PAE | VMXE` |
| CR3 | 指向 OVMF 页表的 HPA |
| IA32_EFER | `LME | LMA`（长模式使能） |
| CS | 64-bit code segment, base=0, limit=0xFFFFF, access=0xA9B（L+D） |
| DS/ES/SS | 64-bit data segment, access=0xA93 |
| RFLAGS | `0x2 | IF` |
| RIP | OVMF 入口点（如 `0x100000`） |
| GDTR | 指向 OVMF GDT |
| IDTR | 指向 OVMF IDT |

**注意**：OVMF 的初始页表、GDT、IDT 等数据由固件自身提供，需要从 pflash 区域读取或由 Axvisor 在加载时解析。一种更简单的方案是让 OVMF 从实模式自行启动（OVMF 本身包含 Reset Vector 代码），这样不需要修改 vCPU 初始状态，但需要确保 pflash 区域被正确映射且 RIP 指向 `0xFFFFFFF0`（x86 reset vector）。

#### 1.5 涉及文件汇总

| 文件 | 修改内容 |
|------|---------|
| `components/axvmconfig/src/lib.rs` | 新增 `VMBootMode`、`PflashConfig`，修改 `VMKernelConfig` |
| `components/axvm/src/config.rs` | `From<AxVMCrateConfig>` 中增加 UEFI 字段映射 |
| `os/axvisor/src/vmm/images/mod.rs` | `ImageLoader` 增加 pflash 加载逻辑 |
| `components/x86_vcpu/src/vmx/vcpu.rs` | `setup_vmcs_guest()` 支持 UEFI 初始状态 |
| `os/axvisor/configs/vms/` | 新增 UEFI 客户机配置文件 |

---

### 第 2 步：补齐平台设备

**目标**：实现 OVMF 启动所依赖的"平台设备"——fw_cfg 固件配置接口和最小 ACPI 表集合。

#### 2.1 实现 fw_cfg 设备

**QEMU fw_cfg 规范**：fw_cfg 是 QEMU 定义的一个简单键值对接口，用于向固件传递启动参数（内存布局、CPU 数量、启动顺序、ACPI 表等）。OVMF 在启动早期会主动查询 fw_cfg 来获取这些信息。

**接口定义**：

| 端口 | 方向 | 功能 |
|------|------|------|
| `0x510` | 写 | Selector 端口：选择要访问的键（item） |
| `0x511` | 读 | Data 端口：读取选中 item 的数据（按字节顺序） |
| `0x510` | 读 | 保留（QEMU 中用于 DMA 接口，可暂不实现） |

**需要暴露的 fw_cfg items**：

| Item ID | 名称 | 内容 |
|---------|------|------|
| `0x0001` | `FW_CFG_SIGNATURE` | `"QEMU"` |
| `0x0003` | `FW_CFG_RAM_SIZE` | 客户机内存大小 |
| `0x0005` | `FW_CFG_NB_CPUS` | vCPU 数量 |
| `0x0008` | `FW_CFG_FILE_DIR` | 文件目录（用于传递 ACPI 表等） |
| `0x8000+` | 文件项 | ACPI 表、启动顺序等 |

**实现方案**：

1. 新建 `components/fw_cfg/` crate（或在 `axdevice` 中新增模块）
2. 实现 `BaseDeviceOps<PortRange>` trait
3. 在 `EmulatedDeviceType` 中新增 `FwCfg` 变体
4. 在 `axdevice::init()` 中增加 x86_64 的 `FwCfg` 分支
5. fw_cfg 设备在 `ImageLoader::load()` 的 UEFI 分支中创建并注册

**涉及文件**：

| 文件 | 修改内容 |
|------|---------|
| `components/axvmconfig/src/lib.rs` | `EmulatedDeviceType` 新增 `FwCfg` |
| `components/axdevice/src/device.rs` | `init()` 中增加 x86_64 的 `FwCfg` 分支 |
| 新建 `components/fw_cfg/` | fw_cfg 设备实现 |
| `os/axvisor/src/vmm/images/mod.rs` | UEFI 加载时创建 fw_cfg 并注入数据 |

#### 2.2 生成最小 ACPI 表

**UEFI 客户机最少需要的 ACPI 表**：

| 表 | 签名 | 用途 | 关键内容 |
|----|------|------|---------|
| RSDP | `"RSD PTR "` | 根系统描述指针 | XSDT 地址、校验和 |
| XSDT | `"XSDT"` | 扩展系统描述表 | 指向 FADT/MADT/MCFG 的指针数组 |
| FADT | `"FACP"` | 固定 ACPI 描述表 | PM1a_EVT/CTL 地址、DSDT 地址、SCI_INT、FW_CFG 端口 |
| MADT | `"APIC"` | 多 APIC 描述表 | LAPIC 地址、IOAPIC 地址/条目数、中断源覆盖 |
| MCFG | `"MCFG"` | PCI ECAM 描述表 | ECAM 基地址、段号、起始/结束总线号 |

**ACPI 表在客户机中的位置**：

- RSDP：放在低地址（如 `0xE0000 ~ 0xFFFFF` 的 BIOS 区域），或通过 fw_cfg 的 `etc/acpi/rsdp` 文件项传递
- 其余表：放在客户机 RAM 的高地址区域（如 `0x7FFE_0000` 附近），或通过 fw_cfg 的 `etc/acpi/tables` 文件项传递

**实现方案**：

1. 新建 `components/acpi_tables/` crate（或在 `axdevice` 中新增模块）
2. 定义各表的二进制布局结构（`#[repr(C, packed)]`）
3. 提供 `build_acpi_tables()` 函数，根据 VM 配置（CPU 数、内存大小、IOAPIC 地址、ECAM 基地址）生成完整表链
4. 在 `ImageLoader::load()` 的 UEFI 分支中调用并写入客户机内存
5. 同时将 ACPI 表注册到 fw_cfg 的文件项中

**涉及文件**：

| 文件 | 修改内容 |
|------|---------|
| 新建 `components/acpi_tables/` | ACPI 表生成实现 |
| `os/axvisor/src/vmm/images/mod.rs` | UEFI 加载时生成并写入 ACPI 表 |
| `components/fw_cfg/` | fw_cfg 中注册 ACPI 表文件项 |

---

### 第 3 步：补齐 PC 设备

**目标**：实现最小 PCI 主桥和配置空间，优先支持 virtio-pci block 设备，让 UEFI 固件能发现磁盘并从中启动。随后再加入 virtio-net 和 virtio-console 支持。

#### 3.1 PCI 主桥和配置空间

**两种 PCI 配置空间访问方式**：

| 方式 | 地址范围 | 机制 |
|------|---------|------|
| PIO | `0xCF8`（地址端口）+ `0xCFC`（数据端口） | Type0/Type1 配置周期 |
| ECAM | MMIO 区域（如 `0xb000_0000` 起） | 每个设备占 4KB，总线号/设备号/功能号编码在地址中 |

**实现方案**：

1. 新建 `components/pci_host/` crate
2. 实现 PIO 方式的 PCI Config 模拟（`0xCF8`/`0xCFC`）—— 注册为 Port I/O 设备
3. 实现 ECAM 方式的 PCI Config 模拟 —— 注册为 MMIO 设备
4. 维护一个虚拟 PCI 配置空间数组（256 字节/功能 × 功能数）
5. PCI 主桥自身在配置空间中表现为 Bus 0, Device 0, Function 0

**PCI 配置空间关键寄存器**：

| 偏移 | 寄存器 | 用途 |
|------|--------|------|
| `0x00-0x01` | Vendor ID | `0x1AF4`（Red Hat / virtio） |
| `0x02-0x03` | Device ID | `0x1001`（virtio-blk）等 |
| `0x04-0x05` | Command | I/O / Memory / Bus Master 使能 |
| `0x06-0x07` | Status | 中断状态等 |
| `0x08` | Revision ID | |
| `0x09-0x0B` | Class Code | 大类/子类/接口 |
| `0x0C` | Cache Line Size | |
| `0x0D` | Latency Timer | |
| `0x0E` | Header Type | Type 0（普通设备） |
| `0x0F` | BIST | |
| `0x10-0x27` | BAR[0-5] | 基地址寄存器 |
| `0x2C-0x2D` | Subsystem Vendor ID | |
| `0x2E-0x2F` | Subsystem Device ID | |
| `0x34` | Capabilities Pointer | 指向 MSI/MSI-X capability 链 |
| `0x3C` | Interrupt Line | IRQ 号 |
| `0x3D` | Interrupt Pin | INTx 引脚选择 |

**涉及文件**：

| 文件 | 修改内容 |
|------|---------|
| 新建 `components/pci_host/` | PCI 主桥 + Config Space 模拟 |
| `components/axvmconfig/src/lib.rs` | `EmulatedDeviceType` 新增 `PciHostBridge` |
| `components/axdevice/src/device.rs` | `init()` 中增加 PCI 主桥分支 |

#### 3.2 virtio-pci 设备

**virtio-pci 架构**：

```
┌──────────────────────────────────────────┐
│  virtio-pci 设备                          │
│  ├── PCI Config Space (0xCF8/0xCFC 或 ECAM) │
│  ├── Common Config (MMIO BAR)            │
│  │   ├── 设备特征/状态/驱动特征           │
│  │   ├── 通知区域偏移                     │
│  │   └── ISR 状态                         │
│  ├── Notify Config (MMIO BAR)            │
│  │   └── virtqueue 通知                   │
│  ├── ISR Config (MMIO BAR)               │
│  │   └── 中断状态                         │
│  └── Device-specific Config (MMIO BAR)   │
│      └── (如 virtio-blk 的容量/扇区大小)  │
├──────────────────────────────────────────┤
│  virtqueue (VRing)                        │
│  ├── Descriptor Table                     │
│  ├── Available Ring                       │
│  └── Used Ring                            │
└──────────────────────────────────────────┘
```

**实现方案**：

1. 新建 `components/virtio_pci/` crate
2. 定义 `VirtioPciDevice` 结构，包含 PCI 配置空间、Common/Notify/ISR/Device-specific 配置区域
3. 定义 `VirtQueue` / `VRing` 结构，实现 virtqueue 的 avail/used ring 管理
4. 实现 `VirtioBlkPci`：在 `VirtioPciDevice` 基础上增加 block 设备特有逻辑（读/写请求处理、扇区映射到后端存储）
5. 后续实现 `VirtioNetPci` 和 `VirtioConsolePci`

**virtio-blk 请求格式**：

| 字段 | 大小 | 用途 |
|------|------|------|
| type | 1 | 0=read, 1=write |
| reserved | 1 | 保留 |
| sector | 8 | 起始扇区号 |
| data | 可变 | 数据缓冲区 |
| status | 1 | 0=OK, 1=IO Error |

**涉及文件**：

| 文件 | 修改内容 |
|------|---------|
| 新建 `components/virtio_pci/` | virtio-pci 框架 + virtqueue |
| 新建 `components/virtio_blk_pci/` | virtio-blk-pci 实现 |
| `components/axdevice/src/device.rs` | `init()` 中增加 `VirtioBlk` 分支 |
| `components/pci_host/` | PCI 主桥中注册 virtio-pci 设备 |

---

### 第 4 步：完善中断链路

**目标**：短期利用 vIOAPIC + INTx 让 virtio-pci 的基本中断跑通；中期完善 vLAPIC 的 EOI、IPI、timer 以及 MSI/MSI-X 支持。

#### 4.1 实现 vIOAPIC

**Intel IOAPIC 规范**：

- MMIO 基地址：通常 `0xFEC00000`
- 寄存器选择端口：offset `0x00`（选择寄存器索引）
- 寄存器数据端口：offset `0x10`（读/写选中寄存器）
- 关键寄存器：

| 索引 | 寄存器 | 用途 |
|------|--------|------|
| `0x00` | IOAPICID | IOAPIC ID |
| `0x01` | IOAPICVER | 版本号（最大条目数） |
| `0x02` | IOAPICARB | 仲裁 ID |
| `0x10-0x3F` | IOREDTBL[0-23] | 重定向表项（每个 64 位，占 2 个索引） |

**IOREDTBL 条目格式**：

| 位 | 字段 | 用途 |
|----|------|------|
| 0-7 | Vector | 中断向量号 |
| 8-10 | Delivery Mode | 000=Fixed, 010=SMI, 100=NMI, 111=ExtINT |
| 11 | Dest Mode | 0=Physical, 1=Logical |
| 12 | Delivery Status | 0=Idle, 1=Send Pending |
| 13 | Polarity | 0=Active High, 1=Active Low |
| 14 | Remote IRR | 远程 IRR 状态 |
| 15 | Trigger Mode | 0=Edge, 1=Level |
| 16 | Mask | 0=Enabled, 1=Masked |
| 56-63 | Destination | 目标 APIC ID |

**实现方案**：

1. 新建 `components/x86_vioapic/` crate
2. 实现 `VirtualIoApic` 结构，包含 24 个重定向表项
3. 实现 `BaseDeviceOps<GuestPhysAddrRange>` trait（MMIO 设备）
4. 提供 `inject_irq(irq: u8)` 方法，供 virtio-pci 等设备调用
5. 中断注入流程：`设备 → vioapic.inject_irq() → vlapic.set_intr() → vcpu.queue_event()`
6. EOI 广播流程：`vlapic.process_eoi() → vioapic.broadcast_eoi() → 清除 Remote IRR`

**涉及文件**：

| 文件 | 修改内容 |
|------|---------|
| 新建 `components/x86_vioapic/` | vIOAPIC 实现 |
| `components/axvmconfig/src/lib.rs` | `EmulatedDeviceType` 新增 `IoApic` |
| `components/axdevice/src/device.rs` | `init()` 中增加 x86_64 的 `IoApic` 分支 |
| `components/x86_vlapic/src/vlapic.rs` | 补全 `process_eoi()` 中的广播逻辑 |

#### 4.2 补全 vLAPIC

文件：`components/x86_vlapic/src/vlapic.rs`

需要补全的关键函数：

| 函数 | 当前状态 | 需要实现 |
|------|---------|---------|
| `process_eoi()` | `unimplemented!()` | 调用 `vioapic_broadcast_eoi()` + `vcpu_make_request()` |
| `set_intr()` | `unimplemented!()` | 设置 IRR 位 + 触发 vCPU 中断注入 |
| `inject_nmi()` | `unimplemented!()` | 通过 VMCS 注入 NMI |
| `process_init_sipi()` | `unimplemented!()` | 处理 INIT（重置 vCPU）和 SIPI（设置 RIP 并启动 AP） |
| `handle_self_ipi()` | `unimplemented!()` | x2APIC Self-IPI 处理 |

**中断注入完整数据通路**：

```
┌──────────┐     inject_irq()     ┌──────────┐    set_intr()    ┌──────────┐
│  设备     │ ──────────────────→ │  vIOAPIC  │ ──────────────→ │  vLAPIC   │
│(virtio等) │                      │          │                  │          │
└──────────┘                      └──────────┘                  └────┬─────┘
                                                                     │
                                                    queue_event()     │ inject
                                                    (设置 pending)    ↓
                                                               ┌──────────┐
                                                               │  vCPU     │
                                                               │ VM-Entry  │
                                                               │ 注入中断   │
                                                               └──────────┘
                                                                     │
                                              Guest 执行中断处理程序    │
                                                                     ↓
                                                              ┌──────────┐
                                                              │ EOI 写入  │
                                                              └────┬─────┘
                                                                   │
                                              process_eoi()        │
                                              broadcast_eoi()      ↓
                                                               ┌──────────┐
                                                               │  vIOAPIC  │
                                                               │ 清除IRR   │
                                                               └──────────┘
```

#### 4.3 串通中断注入路径

文件：`os/axvisor/src/hal/arch/x86_64/mod.rs`

```rust
// 当前：
pub fn inject_interrupt(_vector: u8) {}

// 需要实现为：
pub fn inject_interrupt(vector: u8) {
    // 1. 找到当前 vCPU
    // 2. 调用 vcpu.queue_event(vector)
}
```

#### 4.4 MSI/MSI-X（中期目标）

**MSI（Message Signaled Interrupts）**：

- 通过 PCI Capability 结构配置
- 写入 MSI Address + Data 触发中断
- 不经过 IOAPIC，直接写入 LAPIC

**MSI-X**：

- 比 MSI 更灵活，支持更多向量
- 通过 MSI-X Capability + MSI-X Table + MSI-X PBA 实现
- Table 和 PBA 通过 BAR 映射

**实现方案**：

1. 在 `VirtioPciDevice` 的 PCI 配置空间中添加 MSI/MSI-X Capability
2. 实现 MSI Address/Data 解析和中断注入
3. MSI-X Table/PBA 的 MMIO 处理

**涉及文件**：

| 文件 | 修改内容 |
|------|---------|
| `components/virtio_pci/` | 添加 MSI/MSI-X Capability |
| `components/x86_vlapic/src/vlapic.rs` | 支持 MSI 写入触发的中断注入 |
| `components/pci_host/` | PCI 配置空间中 MSI Capability 的处理 |

---

### 第 5 步：验证闭环

**目标**：新建专门的 x86_64 UEFI 客户机配置，编写 QEMU 冒烟测试，验证路径从 OVMF 的 UEFI Shell 开始，逐步到通过 Linux EFI stub 和 virtio-block 启动完整内核。

#### 5.1 准备 UEFI 固件和镜像

| 资源 | 来源 | 用途 |
|------|------|------|
| `OVMF_CODE.fd` | 系统包 `ovmf` 或 EDK2 编译 | UEFI 固件代码 |
| `OVMF_VARS.fd` | 系统包 `ovmf` 或 EDK2 编译 | UEFI 变量存储 |
| Linux 内核（EFI stub） | 编译 `vmlinux` 或 `bzImage` | 测试用客户机 OS |
| rootfs 磁盘镜像 | Alpine/Debian 最小安装 | virtio-blk 后端存储 |

#### 5.2 新建 UEFI 客户机配置

文件：`os/axvisor/configs/vms/linux-x86_64-uefi.toml`（新建）

```toml
[base]
id = 0
name = "linux-uefi"
cpu_num = 1
vm_type = 0

[kernel]
boot_mode = "uefi"
kernel_path = "/guest/linux/bzImage"
kernel_load_addr = 0x20_0000
cmdline = "root=/dev/vda2 console=ttyS0"

[kernel.pflash0]
path = "/usr/share/OVMF/OVMF_CODE.fd"
base_gpa = 0x1000_0000
size = 0x20_0000
read_only = true

[kernel.pflash1]
path = "/usr/share/OVMF/OVMF_VARS.fd"
base_gpa = 0x1020_0000
size = 0x20_0000
read_only = false

memory_regions = [
  [0x0000_0000, 0x8000_0000, 0x7, 0],  # 2GB RAM
  [0xFEC0_0000, 0x1000, 0x7, 0],       # IOAPIC
  [0xFEE0_0000, 0x1000, 0x7, 0],       # LAPIC
]

[devices]
interrupt_mode = "emulated"
emu_devices = [
  { name = "fw-cfg", base_gpa = 0, length = 0, irq_id = 0, emu_type = "FwCfg", cfg_list = [] },
  { name = "ioapic", base_gpa = 0xFEC0_0000, length = 0x1000, irq_id = 0, emu_type = "IoApic", cfg_list = [] },
  { name = "pci-host", base_gpa = 0xb000_0000, length = 0x1000_0000, irq_id = 0, emu_type = "PciHostBridge", cfg_list = [] },
  { name = "virtio-blk", base_gpa = 0, length = 0, irq_id = 16, emu_type = "VirtioBlk", cfg_list = [] },
]
passthrough_devices = []
```

#### 5.3 QEMU 冒烟测试配置

文件：`os/axvisor/configs/qemu/qemu-x86_64-uefi.toml`（新建）

```toml
uefi = true
args = [
  "-nographic",
  "-cpu", "host",
  "-machine", "q35",
  "-smp", "1",
  "-accel", "kvm",
  "-m", "2G",
  "-drive", "if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE.fd",
  "-drive", "if=pflash,format=raw,file=/usr/share/OVMF/OVMF_VARS.fd",
  "-drive", "id=disk0,if=none,format=raw,file=${workspace}/tmp/axbuild/rootfs/rootfs-x86_64-alpine.img",
  "-device", "virtio-blk-pci,drive=disk0",
]
success_regex = ["UEFI Interactive Shell", "FS0:"]
fail_regex = ["ASSERT", "PANIC"]
```

#### 5.4 验证里程碑

| 阶段 | 验证内容 | 成功标志 |
|------|---------|---------|
| M1 | OVMF 加载并执行 | 看到 UEFI Shell 或 `UEFI Interactive Shell` 输出 |
| M2 | fw_cfg 被正确识别 | OVMF 日志中显示 fw_cfg 相关信息 |
| M3 | ACPI 表被正确解析 | `acpidump` 或 OVMF 日志中显示 RSDP/FADT/MADT/MCFG |
| M4 | PCI 设备被枚举 | UEFI Shell 中 `pci` 命令显示 virtio-blk 设备 |
| M5 | virtio-blk 磁盘被发现 | UEFI Shell 中 `map -r` 显示 `FS0:` 或 `Blk0:` |
| M6 | Linux EFI stub 启动 | 内核启动日志输出 |
| M7 | Linux 完整启动 | 到达 login 提示符 |

#### 5.5 涉及文件汇总

| 文件 | 修改内容 |
|------|---------|
| `os/axvisor/configs/vms/linux-x86_64-uefi.toml` | 新建 UEFI 客户机配置 |
| `os/axvisor/configs/qemu/qemu-x86_64-uefi.toml` | 新建 UEFI QEMU 测试配置 |
| `os/axvisor/configs/board/qemu-x86_64.toml` | 可能需要调整 features |

---

## 五、涉及文件全局汇总

### 需要新建的 crate / 模块

| Crate | 功能 |
|-------|------|
| `components/fw_cfg/` | QEMU fw_cfg 设备模拟 |
| `components/acpi_tables/` | ACPI 表生成（RSDP/XSDT/FADT/MADT/MCFG） |
| `components/pci_host/` | PCI 主桥 + Config Space 模拟 |
| `components/virtio_pci/` | virtio-pci 框架 + virtqueue |
| `components/virtio_blk_pci/` | virtio-blk-pci 实现 |
| `components/x86_vioapic/` | vIOAPIC 模拟 |

### 需要修改的现有文件

| 文件 | 步骤 | 修改内容 |
|------|------|---------|
| `components/axvmconfig/src/lib.rs` | 1, 2, 3 | 新增 `VMBootMode`/`PflashConfig`，`EmulatedDeviceType` 新增 `FwCfg`/`IoApic`/`PciHostBridge` |
| `components/axvm/src/config.rs` | 1 | `From<AxVMCrateConfig>` 中增加 UEFI 字段映射 |
| `os/axvisor/src/vmm/images/mod.rs` | 1, 2 | `ImageLoader` 增加 pflash/fw_cfg/ACPI 加载逻辑 |
| `components/x86_vcpu/src/vmx/vcpu.rs` | 1 | `setup_vmcs_guest()` 支持 UEFI 初始状态 |
| `components/axdevice/src/device.rs` | 2, 3, 4 | `init()` 中增加 x86_64 设备分支 |
| `components/x86_vlapic/src/vlapic.rs` | 4 | 补全 `process_eoi()`/`set_intr()`/`inject_nmi()`/`process_init_sipi()` |
| `os/axvisor/src/hal/arch/x86_64/mod.rs` | 4 | 实现 `inject_interrupt()` |
| `Cargo.toml`（workspace） | 2, 3, 4 | 添加新 crate 到 workspace |

### 需要新建的配置文件

| 文件 | 用途 |
|------|------|
| `os/axvisor/configs/vms/linux-x86_64-uefi.toml` | UEFI 客户机 VM 配置 |
| `os/axvisor/configs/qemu/qemu-x86_64-uefi.toml` | UEFI QEMU 测试配置 |
