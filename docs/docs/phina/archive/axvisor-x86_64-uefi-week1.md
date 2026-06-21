# Axvisor x86_64 UEFI 客户机支持 — 第一周第一次总结

## 总体目标

让 Axvisor 能够启动标准的 x86_64 UEFI 客户机：

- 使用 OVMF / EDK2 作为客户机固件，从固件入口自然进入 UEFI 环境
- 通过 ACPI 表、PCI 总线、virtio 设备等标准 PC 发现路径，逐步启动 Linux EFI stub、UEFI 应用或其他 UEFI 感知的操作系统

最小可行目标包括：

1. OVMF_CODE 与 OVMF_VARS 的加载和地址映射
2. fw_cfg（QEMU 风格的固件配置接口）
3. 基本 ACPI 表（最少要包含 RSDP、XSDT/RSDT、FADT、MADT、MCFG）
4. 一个最小可用的 PCI 主桥和配置空间（ECAM 或 PIO 方式）
5. virtio-block 设备（通过 virtio-pci 暴露），能用来枚举磁盘并启动 OS
6. 基本中断链路：vLAPIC、vIOAPIC、i8259、INTx，后续再完善 MSI/MSI-X


## 第 1 阶段：扩展启动配置

### 目标

将启动方式从单一的 `bios_path / bios_load_addr` 扩展为可配置的 `boot = "trampoline"`（传统 BIOS 方式）或 `boot = "uefi"`。配置项需要显式描述 OVMF 的代码、变量存储、pflash 属性，以及固件在客户机物理地址空间中的位置。

具体来说，当用户在 TOML 中写 `boot_mode = "uefi"` 时，Axvisor 应当：

- 知道需要加载 OVMF_CODE.fd 和 OVMF_VARS.fd 而非 axvm-bios.bin
- 知道需要将它们映射到 pflash 语义的 GPA 区域（而非简单的"把二进制塞到某个地址"）
- 为 vCPU 设置正确的初始状态，使其能从 OVMF 的入口点开始执行
- 后续阶段创建的 fw_cfg、ACPI 等设备也依赖本阶段建立的配置模型来获取参数

### 涉及的模块及现状

#### 1. axvmconfig（`components/axvmconfig/src/lib.rs`）

**现状**：`VMKernelConfig` 结构体定义了从 TOML 反序列化得到的配置字段。当前与启动相关的字段有：

- `entry_point: usize` — 内核入口地址，在 trampoline 模式下固定为 `0x8000` 等低地址
- `bios_path: Option<String>` — BIOS 镜像路径（如 `axvm-bios.bin`）
- `bios_load_addr: Option<usize>` — BIOS 加载 GPA（如 `0x8000`）
- `kernel_path: String` — 内核镜像路径
- `kernel_load_addr: usize` — 内核加载 GPA

**问题**：没有 `boot_mode` 字段来区分启动方式；没有 OVMF_CODE / OVMF_VARS 的路径和 pflash 属性字段；没有描述固件在客户机物理地址空间中位置的机制。

#### 2. axvm::config（`components/axvm/src/config.rs`）

**现状**：`AxVMConfig` 是运行时配置结构，由 `AxVMCrateConfig`（即 axvmconfig 的 TOML 结构）转换而来。关键子结构：

- `AxVCpuConfig { bsp_entry, ap_entry }` — vCPU 入口地址，当前直接取 `kernel.entry_point`
- `VMImageConfig { kernel_load_gpa, bios_load_gpa, dtb_load_gpa, ramdisk }` — 镜像加载地址

`From<AxVMCrateConfig> for AxVMConfig` 的实现中，`bsp_entry` 和 `ap_entry` 都直接取 `cfg.kernel.entry_point`，`bios_load_gpa` 直接取 `cfg.kernel.bios_load_addr`。没有任何 UEFI 相关字段的映射。

**问题**：UEFI 模式下，BSP 入口地址不应该是 `kernel.entry_point`，而应该是 OVMF 固件的入口地址（通常是 pflash 区域的起始地址或 x86 reset vector `0xFFFFFFF0`）。同时缺少 pflash 加载地址的映射。

#### 3. ImageLoader（`os/axvisor/src/vmm/images/mod.rs`）

**现状**：`ImageLoader` 负责将镜像数据写入客户机内存。当前流程：

1. 从 `AxVMConfig` 读取 `kernel_load_gpa`、`bios_load_gpa`、`dtb_load_gpa`
2. 按 `image_location`（"memory" 或 "fs"）分支加载
3. 依次加载 kernel → ramdisk → dtb → bios，全部是"将二进制数据直接写入指定 GPA"

**问题**：没有 pflash 加载语义。OVMF 的 pflash 与普通 BIOS 加载有本质区别：

- pflash0（OVMF_CODE）是只读的固件代码区，需要映射到高地址（如 `0x10000000`）
- pflash1（OVMF_VARS）是可读写的 UEFI 变量存储区，需要映射到另一高地址
- 两块 pflash 的加载顺序、地址、大小、只读属性都需要由配置显式描述

此外，当前 `ImageLoader` 在 UEFI 模式下还需要额外创建 fw_cfg 设备并生成 ACPI 表（第 2 阶段的内容），这些都需要在加载流程中预留扩展点。

#### 4. x86_vcpu（`components/x86_vcpu/src/vmx/vcpu.rs`）

**现状**：`setup_vmcs_guest()` 方法将 vCPU 初始化为实模式：

```rust
// CR0: 缓存禁用，无分页，无保护模式
let cr0_val = Cr0Flags::NOT_WRITE_THROUGH | Cr0Flags::CACHE_DISABLE | Cr0Flags::EXTENSION_TYPE;
self.set_cr(0, cr0_val.bits());
self.set_cr(4, 0);

// 段寄存器：全部 16-bit 实模式属性
set_guest_segment!(CS, 0x9b);  // 16-bit code, exec/read
set_guest_segment!(ES, 0x93);  // 16-bit data, read/write
// ...

// EFER = 0（未开启长模式）
VmcsGuest64::IA32_EFER.write(0)?;

// RIP = entry_point（低地址如 0x8000）
VmcsGuestNW::RIP.write(entry.as_usize())?;

// RFLAGS = 0x2（中断未使能）
VmcsGuestNW::RFLAGS.write(0x2)?;
```

**问题**：OVMF 固件需要从 x86 reset vector（`0xFFFFFFF0`）开始执行。OVMF 自身包含 Reset Vector 代码，会从实模式开始，自行切换到保护模式再进入长模式。因此有两种方案：

**方案**：让 vCPU 从实模式启动，RIP 设为 `0xFFFFFFF0`，由 OVMF 自行完成模式切换。这要求 EPT 将 `0xFFFFFFF0` 附近映射到 pflash0 的 Reset Vector 位置。这是最符合物理硬件行为的方案。


**弃用方案**：直接将 vCPU 初始化为长模式，RIP 设为 OVMF 的 PEI 入口。这需要解析 OVMF 的内部结构来获取入口地址，复杂且脆弱。


### 各模块修改方案

#### 1.1 修改 axvmconfig

**新增 `VMBootMode` 枚举**：

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

**为什么**：需要一种机制让用户在 TOML 中声明启动方式。枚举比布尔值更具扩展性（未来可能支持 `directboot` 等其他模式），且与 serde 的 rename 配合后 TOML 中写 `boot_mode = "uefi"` 即可。

**新增 `PflashConfig` 结构**：

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

**为什么**：OVMF 需要两块 pflash 区域（CODE 和 VARS），它们有不同的属性（只读 vs 可读写）和不同的 GPA 映射位置。不能用现有的 `bios_path + bios_load_addr` 来表示，因为那是一对一的简单映射，而 pflash 需要描述路径、地址、大小、只读属性四个维度。

**修改 `VMKernelConfig`**：

```rust
pub struct VMKernelConfig {
    // 保留所有现有字段...
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

**为什么**：`boot_mode` 决定 Axvisor 的启动逻辑分支；`pflash0`/`pflash1` 提供 OVMF 固件的加载参数。使用 `Option` 包裹使得 trampoline 模式下不需要填写这些字段，保持向后兼容。

**预期 TOML 配置**：

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

#### 1.2 修改 axvm::config

**修改 `VMImageConfig`**：

```rust
pub struct VMImageConfig {
    pub kernel_load_gpa: GuestPhysAddr,
    pub bios_load_gpa: Option<GuestPhysAddr>,
    pub dtb_load_gpa: Option<GuestPhysAddr>,
    pub ramdisk: Option<RamdiskInfo>,
    // 新增
    pub pflash0_load_gpa: Option<GuestPhysAddr>,
    pub pflash1_load_gpa: Option<GuestPhysAddr>,
}
```

**修改 `AxVCpuConfig`**：

```rust
pub struct AxVCpuConfig {
    pub bsp_entry: GuestPhysAddr,
    pub ap_entry: GuestPhysAddr,
    // 新增
    pub boot_mode: VMBootMode,
}
```

**修改 `From<AxVMCrateConfig> for AxVMConfig`**：

```rust
cpu_config: AxVCpuConfig {
    bsp_entry: match cfg.kernel.boot_mode {
        VMBootMode::Trampoline => GuestPhysAddr::from(cfg.kernel.entry_point),
        VMBootMode::Uefi => {
            // UEFI 模式下，BSP 从 reset vector 启动
            // x86 reset vector 固定在 0xFFFFFFF0
            GuestPhysAddr::from(0xFFFFFFF0usize)
        }
    },
    ap_entry: GuestPhysAddr::from(cfg.kernel.entry_point),
    boot_mode: cfg.kernel.boot_mode.clone(),
},
image_config: VMImageConfig {
    kernel_load_gpa: GuestPhysAddr::from(cfg.kernel.kernel_load_addr),
    bios_load_gpa: cfg.kernel.bios_load_addr.map(GuestPhysAddr::from),
    dtb_load_gpa: cfg.kernel.dtb_load_addr.map(GuestPhysAddr::from),
    ramdisk: cfg.kernel.ramdisk_load_addr.map(|addr| RamdiskInfo {
        load_gpa: GuestPhysAddr::from(addr),
        size: None,
    }),
    pflash0_load_gpa: cfg.kernel.pflash0.map(|p| GuestPhysAddr::from(p.base_gpa)),
    pflash1_load_gpa: cfg.kernel.pflash1.map(|p| GuestPhysAddr::from(p.base_gpa)),
},
```

**为什么**：`bsp_entry` 在 UEFI 模式下必须指向 reset vector（`0xFFFFFFF0`），而非 `kernel.entry_point`。这是因为 OVMF 固件从 reset vector 开始执行，而非从内核入口点开始。`pflash0_load_gpa` / `pflash1_load_gpa` 则为 ImageLoader 提供加载目标地址。

#### 1.3 修改 ImageLoader

**新增字段**：

```rust
pub struct ImageLoader {
    main_memory: VMMemoryRegion,
    vm: VMRef,
    config: AxVMCrateConfig,
    kernel_load_gpa: GuestPhysAddr,
    bios_load_gpa: Option<GuestPhysAddr>,
    dtb_load_gpa: Option<GuestPhysAddr>,
    // 新增
    pflash0_load_gpa: Option<GuestPhysAddr>,
    pflash1_load_gpa: Option<GuestPhysAddr>,
}
```

**修改 `load()` 方法**：在读取配置时增加 pflash 地址的获取，并在加载逻辑中根据 `boot_mode` 分支：

```rust
self.vm.with_config(|config| {
    self.kernel_load_gpa = config.image_config.kernel_load_gpa;
    self.dtb_load_gpa = config.image_config.dtb_load_gpa;
    self.bios_load_gpa = config.image_config.bios_load_gpa;
    self.pflash0_load_gpa = config.image_config.pflash0_load_gpa;
    self.pflash1_load_gpa = config.image_config.pflash1_load_gpa;
});

match self.config.kernel.boot_mode {
    VMBootMode::Trampoline => { /* 保持现有逻辑 */ }
    VMBootMode::Uefi => self.load_vm_images_uefi(),
}
```

**新增 `load_vm_images_uefi()` 方法**：

```rust
fn load_vm_images_uefi(&self) -> AxResult {
    // 1. 加载 OVMF_CODE 到 pflash0_gpa
    // 2. 加载 OVMF_VARS 到 pflash1_gpa
    // 3. 在 GPA 0xFFFFFFF0 附近映射 reset vector（指向 pflash0 末尾的跳转指令）
    // 4. 加载 kernel 到 kernel_load_gpa（UEFI 模式下可选，OVMF 从磁盘自行加载）
    // 5. 加载 ramdisk（可选）
    // 6. [第 2 阶段] 创建 fw_cfg 设备并注入启动信息
    // 7. [第 2 阶段] 生成 ACPI 表并写入客户机内存
}
```

**为什么**：UEFI 的 pflash 加载与 BIOS 加载有本质区别——pflash 是固定大小的闪存区域，CODE 区只读，VARS 区可读写。此外，reset vector 的映射是 UEFI 启动的关键：x86 CPU 上电后第一条指令从 `0xFFFFFFF0` 取指，OVMF 在该位置放置了一条远跳转指令跳转到固件入口。因此 ImageLoader 必须确保 `0xFFFFFFF0` 附近的 EPT 映射指向 pflash0 的正确偏移。

#### 1.4 修改 x86_vcpu

**修改 `setup_vmcs_guest()` 方法**：接收 `boot_mode` 参数，在 UEFI 模式下调整初始状态：

```rust
fn setup_vmcs_guest(&mut self, entry: GuestPhysAddr, boot_mode: VMBootMode) -> AxResult {
    match boot_mode {
        VMBootMode::Trampoline => {
            // 保持现有实模式初始化逻辑不变
        }
        VMBootMode::Uefi => {
            // 方案 A：从实模式启动，RIP = 0xFFFFFFF0
            // 与 trampoline 模式类似，但 entry 地址不同
            // CR0/CS 等保持实模式设置，由 OVMF 自行切换模式
        }
    }
}
```

**为什么**：采用方案 A（从实模式启动），vCPU 初始状态与 trampoline 模式基本相同，只是 `entry` 地址变为 `0xFFFFFFF0`。OVMF 的 Reset Vector 代码会自行完成实模式→保护模式→长模式的切换。这样 `setup_vmcs_guest()` 的改动最小，且与真实 PC 的启动行为一致。

**但需要注意**：`0xFFFFFFF0` 位于 4GB 地址空间的顶部，EPT 必须将此地址映射到 pflash0 的末尾（OVMF 将 reset vector 放在固件映像的最后 16 字节）。这需要在 ImageLoader 或 VM 初始化阶段确保 EPT 映射正确。

#### 1.5 涉及文件汇总

| 文件 | 修改内容 | 修改原因 |
|------|---------|---------|
| `components/axvmconfig/src/lib.rs` | 新增 `VMBootMode` 枚举、`PflashConfig` 结构，`VMKernelConfig` 新增 `boot_mode`/`pflash0`/`pflash1` 字段 | 让 TOML 配置能描述 UEFI 启动方式和 pflash 参数 |
| `components/axvm/src/config.rs` | `VMImageConfig` 新增 pflash 字段，`AxVCpuConfig` 新增 `boot_mode`，修改 `From` 实现 | 将 TOML 配置转换为运行时配置，UEFI 模式下 `bsp_entry` 指向 reset vector |
| `os/axvisor/src/vmm/images/mod.rs` | `ImageLoader` 新增 pflash 字段和 `load_vm_images_uefi()` 方法 | 支持 OVMF_CODE/VARS 的 pflash 加载和 reset vector 映射 |
| `components/x86_vcpu/src/vmx/vcpu.rs` | `setup_vmcs_guest()` 接收 `boot_mode` 参数 | UEFI 模式下 vCPU 从 reset vector 启动 |

---

## 第 2 阶段：补齐平台设备

### 目标

实现 OVMF 启动所依赖的"平台设备"——先实现一个 QEMU 兼容的 fw_cfg 设备，用以向固件传递启动信息；接着生成并暴露最小 ACPI 表集合（RSDP、XSDT/RSDT、FADT、MADT、MCFG），让 OVMF 能通过标准路径发现硬件拓扑。

为什么 OVMF 需要这两个设备：

- **fw_cfg**：OVMF 在 SEC 阶段最早访问的设备就是 fw_cfg。它通过 fw_cfg 查询内存大小、CPU 数量、启动顺序等关键信息。没有 fw_cfg，OVMF 无法知道客户机有多少内存和 CPU，也无法找到内核镜像和命令行。
- **ACPI 表**：OVMF 在 DXE 阶段会安装 ACPI 表。如果 Hypervisor 已经提供了 ACPI 表（通过 fw_cfg 的文件项传递），OVMF 会直接使用它们；否则 OVMF 需要自行生成，但 OVMF 自行生成的表不包含 Hypervisor 的设备拓扑信息（如虚拟 IOAPIC 地址、PCI ECAM 基地址等），导致客户机 OS 无法正确发现硬件。

### 涉及的模块及现状

#### 1. fw_cfg — 完全不存在

当前没有任何 fw_cfg 设备的实现代码。`EmulatedDeviceType` 枚举中也没有 `FwCfg` 变体。

fw_cfg 是 QEMU 定义的一个简单键值对接口，通过两个 I/O 端口暴露：

| 端口 | 方向 | 功能 |
|------|------|------|
| `0x510` | 写 | Selector 端口：选择要访问的键（item） |
| `0x511` | 读 | Data 端口：读取选中 item 的数据（按字节顺序） |

OVMF 在启动早期（SEC 阶段）通过 `in`/`out` 指令访问这两个端口来获取启动参数。

#### 2. ACPI 表 — 完全不存在

当前没有任何 ACPI 表的生成代码。x86_64 客户机不依赖 ACPI（NimbOS 等简单 OS 直接用直通硬件），但 UEFI 客户机**必须**通过 ACPI 发现硬件。

UEFI 客户机最少需要的 ACPI 表：

| 表 | 签名 | 用途 | 关键内容 |
|----|------|------|---------|
| RSDP | `"RSD PTR "` | 根系统描述指针 | XSDT 地址、校验和 |
| XSDT | `"XSDT"` | 扩展系统描述表 | 指向 FADT/MADT/MCFG 的指针数组 |
| FADT | `"FACP"` | 固定 ACPI 描述表 | PM1a_EVT/CTL 地址、DSDT 地址、SCI_INT |
| MADT | `"APIC"` | 多 APIC 描述表 | LAPIC 地址、IOAPIC 地址/条目数、中断源覆盖 |
| MCFG | `"MCFG"` | PCI ECAM 描述表 | ECAM 基地址、段号、起始/结束总线号 |

#### 3. axdevice（`components/axdevice/src/device.rs`）

**现状**：`AxVmDevices::init()` 中的 `match config.emu_type` 分支对 x86_64 不友好——所有 x86 相关的设备类型（包括 `VirtioBlk`/`VirtioNet`/`VirtioConsole`）都走 `_ => warn!` 分支，不会创建任何设备实例。

**但**：设备分发框架已经就绪。`AxVmDevices` 支持 `emu_mmio_devices`、`emu_port_devices`、`emu_sys_reg_devices` 三类设备列表，`add_port_dev()` / `add_mmio_dev()` / `add_sys_reg_dev()` 方法可以直接使用。新设备只需要实现 `BaseDeviceOps<PortRange>` 或 `BaseDeviceOps<GuestPhysAddrRange>` trait 并注册即可。

#### 4. axvmconfig（`components/axvmconfig/src/lib.rs`）

**现状**：`EmulatedDeviceType` 枚举中没有 `FwCfg` 变体。现有的枚举值中，`InterruptController`(0x1) 在 x86_64 上走 warn 分支，`VirtioBlk`(0xE1) 等也走 warn 分支。

**但**：`EmulatedDeviceConfig` 结构已经预留了 `irq_id` 和 `cfg_list` 字段，可以用来传递设备的 IRQ 号和额外参数。

### 各模块修改方案

#### 2.1 新建 fw_cfg crate

**新建 `components/fw_cfg/`**，实现 QEMU 兼容的 fw_cfg 设备。

**核心数据结构**：

```rust
pub struct FwCfgDevice {
    selector: u16,            // 当前选中的 item ID
    offset: usize,            // 当前 item 内的读取偏移
    items: BTreeMap<u16, FwCfgItem>,  // 预注册的 item 集合
    files: BTreeMap<String, FwCfgFile>,  // 文件项集合
}

pub struct FwCfgItem {
    data: Vec<u8>,
}

pub struct FwCfgFile {
    selector: u16,   // 分配的 item ID（从 0x8000 起）
    size: usize,
    name: String,
}
```

**实现 `BaseDeviceOps<PortRange>`**：

fw_cfg 是 Port I/O 设备，地址范围为 `0x510~0x511`：

```rust
impl BaseDeviceOps<PortRange> for FwCfgDevice {
    fn address_range(&self) -> PortRange { PortRange::new(Port(0x510), 2) }

    fn handle_read(&self, addr: Port, width: AccessWidth) -> AxResult<usize> {
        match addr.0 {
            0x510 => { /* DMA 接口，暂不实现 */ }
            0x511 => { /* 返回当前 item 的下一个字节 */ }
            _ => {}
        }
    }

    fn handle_write(&mut self, addr: Port, width: AccessWidth, value: usize) -> AxResult {
        match addr.0 {
            0x510 => { self.selector = value as u16; self.offset = 0; }
            0x511 => { /* 忽略写入 */ }
            _ => {}
        }
        Ok(())
    }
}
```

**需要暴露的 fw_cfg items**：

| Item ID | 名称 | 内容 | 来源 |
|---------|------|------|------|
| `0x0001` | `FW_CFG_SIGNATURE` | `"QEMU"` | 固定值 |
| `0x0003` | `FW_CFG_RAM_SIZE` | 客户机内存大小 | 从 VM 配置获取 |
| `0x0005` | `FW_CFG_NB_CPUS` | vCPU 数量 | 从 VM 配置获取 |
| `0x0008` | `FW_CFG_FILE_DIR` | 文件目录项数 + 文件项数组 | 动态构建 |
| `0x8000+` | 文件项 | ACPI 表、启动命令行等 | 动态注册 |

**为什么**：OVMF 的 `OvmfPkg/Library/QemuFwCfgLib` 在 SEC 阶段首先读取 `FW_CFG_SIGNATURE` 确认 fw_cfg 存在，然后依次读取 `FW_CFG_RAM_SIZE` 和 `FW_CFG_NB_CPUS` 来初始化内存和 CPU 信息。如果缺少这些 item，OVMF 会使用默认值或直接卡住。

#### 2.2 修改 axvmconfig

**在 `EmulatedDeviceType` 中新增 `FwCfg`**：

```rust
pub enum EmulatedDeviceType {
    // 现有字段...
    // 0x10 - 0x1F: Platform devices
    FwCfg = 0x10,
    // ...
}
```

**为什么**：fw_cfg 是平台级设备，不属于中断控制器（0x20 段）也不属于 virtio（0xE0 段），放在 0x10 段的"平台设备"区间是合理的。

#### 2.3 修改 axdevice

**在 `init()` 中增加 x86_64 的 `FwCfg` 分支**：

```rust
EmulatedDeviceType::FwCfg => {
    #[cfg(target_arch = "x86_64")]
    {
        let fw_cfg = FwCfgDevice::new(/* 从 config 获取参数 */);
        this.add_port_dev(Arc::new(fw_cfg));
    }
}
```

**为什么**：fw_cfg 是 Port I/O 设备，通过 `add_port_dev()` 注册后，当客户机执行 `in`/`out` 指令访问 `0x510`/`0x511` 时，VM Exit 的 `IoRead`/`IoWrite` 会被分发到 fw_cfg 的 `handle_read`/`handle_write` 方法。

#### 2.4 新建 ACPI 表生成 crate

**新建 `components/acpi_tables/`**，实现最小 ACPI 表集合的生成。

**核心接口**：

```rust
pub struct AcpiTableBuilder {
    cpu_num: usize,
    ram_size: usize,
    ioapic_addr: usize,
    ioapic_id: u8,
    lapic_addr: usize,
    ecam_base_addr: usize,
    ecam_bus_start: u8,
    ecam_bus_end: u8,
}

impl AcpiTableBuilder {
    pub fn build(&self) -> AcpiTables { /* ... */ }
}

pub struct AcpiTables {
    pub rsdp: Vec<u8>,    // RSDP 表数据
    pub tables: Vec<u8>,  // XSDT + FADT + MADT + MCFG 的连续数据
}
```

**各表的关键字段**：

**RSDP**（Root System Description Pointer）：
- 放在 GPA `0xE0000~0xFFFFF` 区域或通过 fw_cfg 传递
- 包含 XSDT 的物理地址和校验和
- 签名：`"RSD PTR "`

**XSDT**（Extended System Description Table）：
- 包含指向 FADT、MADT、MCFG 的 64 位指针数组
- 入口数 = 3（FADT + MADT + MCFG）

**FADT**（Fixed ACPI Description Table）：
- `PM1a_EVT_BLK`：电源管理事件寄存器地址（可以指向一个模拟的 MMIO 区域）
- `SCI_INT`：ACPI 中断的 IRQ 号（通常为 9，通过 IOAPIC 的 GSI 9 传递）
- `DSDT`：指向 DSDT 的地址（最小实现中可以为空表）

**MADT**（Multiple APIC Description Table）：
- `Local APIC Address`：`0xFEE00000`
- IOAPIC 条目：ID、地址（`0xFEC00000`）、GSI 基数（0）
- 每个 vCPU 的 LAPIC 条目：APIC ID、Flags（Enabled）
- 中断源覆盖：ISA IRQ 0 → GSI 2（标准 PC 约定）

**MCFG**（PCI Express Memory-mapped Configuration Space）：
- `Base Address`：ECAM 基地址（如 `0xB0000000`）
- `Segment Group`：0
- `Start Bus`：0
- `End Bus`：0xFF（或更小）

**为什么**：这些表是 UEFI 客户机发现硬件的标准路径。OVMF 在 DXE 阶段会安装这些表，客户机 OS 启动后也依赖它们来发现 IOAPIC（MADT）、PCI 总线（MCFG）等。如果 Hypervisor 不提供这些表，OVMF 只能看到直通的硬件，看不到虚拟设备。

#### 2.5 修改 ImageLoader

在 `load_vm_images_uefi()` 中增加 fw_cfg 创建和 ACPI 表生成：

```rust
fn load_vm_images_uefi(&self) -> AxResult {
    // 1. 加载 OVMF_CODE / OVMF_VARS（第 1 阶段已实现）
    // 2. 创建 fw_cfg 设备，注入启动信息
    let mut fw_cfg = FwCfgDevice::new();
    fw_cfg.add_item(FW_CFG_SIGNATURE, b"QEMU");
    fw_cfg.add_item(FW_CFG_RAM_SIZE, &ram_size.to_le_bytes());
    fw_cfg.add_item(FW_CFG_NB_CPUS, &cpu_num.to_le_bytes());
    // 3. 生成 ACPI 表
    let acpi = AcpiTableBuilder::new(cpu_num, ram_size, ioapic_addr, ecam_base)
        .build();
    // 4. 将 ACPI 表写入客户机内存
    self.vm.write_guest_memory(acpi_gpa, &acpi.tables);
    // 5. 将 ACPI 表注册到 fw_cfg 文件项
    fw_cfg.add_file("etc/acpi/tables", &acpi.tables);
    fw_cfg.add_file("etc/acpi/rsdp", &acpi.rsdp);
    // 6. 将 fw_cfg 注册到 VM 的设备列表
    self.vm.add_port_device(Arc::new(fw_cfg));
}
```

**为什么**：fw_cfg 和 ACPI 表的创建时机必须在 ImageLoader 的加载阶段，因为此时 VM 的 EPT 映射正在建立，ACPI 表需要写入客户机内存，fw_cfg 需要在 OVMF 开始执行前就注册好。如果延迟到 VM 运行时创建，OVMF 的 SEC 阶段已经尝试访问 fw_cfg，会导致 VM Exit 到未注册的端口而 panic。

#### 2.6 涉及文件汇总

| 文件 | 修改内容 | 修改原因 |
|------|---------|---------|
| 新建 `components/fw_cfg/` | fw_cfg 设备实现 | OVMF 启动的第一个依赖，提供内存/CPU/启动信息 |
| 新建 `components/acpi_tables/` | ACPI 表生成 | UEFI 客户机发现硬件的标准路径 |
| `components/axvmconfig/src/lib.rs` | `EmulatedDeviceType` 新增 `FwCfg` | 让 TOML 配置能声明 fw_cfg 设备 |
| `components/axdevice/src/device.rs` | `init()` 增加 x86_64 的 `FwCfg` 分支 | 将 fw_cfg 注册到 VM 的 Port I/O 设备列表 |
| `os/axvisor/src/vmm/images/mod.rs` | `load_vm_images_uefi()` 中创建 fw_cfg 和 ACPI 表 | 在 OVMF 执行前准备好所有平台设备 |

---

## 第 3 阶段：补齐 PC 设备

### 目标

实现最小 PCI 主桥和配置空间，优先支持 virtio-pci block 设备，让 UEFI 固件能发现磁盘并从中启动。随后再加入 virtio-net 和 virtio-console 支持，构成一套基本可用的输入输出环境。

为什么 UEFI 需要 PCI 和 virtio-pci：

- **PCI 主桥**：UEFI 的 PCI Bus Driver 在 DXE 阶段会枚举 PCI 总线，发现所有 PCI 设备。如果没有 PCI 主桥，UEFI 根本看不到任何 PCI 设备，也就无法找到 virtio-blk 磁盘。PCI 主桥提供了两种配置空间访问方式（PIO `0xCF8/0xCFC` 和 ECAM MMIO），UEFI 通过这两种方式之一来读写 PCI 设备的配置空间。
- **virtio-pci block**：这是 UEFI 启动 OS 的最简路径。OVMF 包含 `OvmfPkg/VirtioBlkDxe` 驱动，能自动识别 virtio-blk-pci 设备并从中读取磁盘。磁盘上可以放置 EFI System Partition，UEFI 从中加载 OS 的 EFI stub。

### 涉及的模块及现状

#### 1. PCI 主桥 — 完全不存在

当前没有虚拟 PCI 主桥、没有 ECAM 模拟、没有 PIO Config 模拟、没有 PCI 设备枚举框架、没有 BAR 分配机制。

PCI 配置空间有两种访问方式：

| 方式 | 地址范围 | 机制 |
|------|---------|------|
| PIO | `0xCF8`（地址端口）+ `0xCFC`（数据端口） | 32 位地址端口编码 Bus/Dev/Func/Reg |
| ECAM | MMIO 区域（如 `0xb000_0000` 起） | 每个功能占 4KB，地址编码 Bus/Dev/Func |

Q35 机器类型同时支持两种方式。OVMF 通常优先使用 ECAM。

#### 2. virtio-pci — 完全不存在

`EmulatedDeviceType` 中定义了 `VirtioBlk`(0xE1)、`VirtioNet`(0xE2)、`VirtioConsole`(0xE3)，但 `axdevice::init()` 中全部走 `_ => warn!` 分支。

virtio-pci 设备的架构：

```
┌──────────────────────────────────────────┐
│  virtio-pci 设备                          │
│  ├── PCI Config Space                    │
│  │   ├── Vendor/Device ID               │
│  │   ├── BAR[0-5]                       │
│  │   ├── MSI/MSI-X Capability           │
│  │   └── Interrupt Line/Pin             │
│  ├── Common Config (MMIO BAR0)           │
│  │   ├── 设备特征/状态/驱动特征           │
│  │   ├── 通知区域偏移                     │
│  │   └── ISR 状态                         │
│  ├── Notify Config (MMIO BAR0 偏移)      │
│  │   └── virtqueue 通知                   │
│  ├── ISR Config (MMIO BAR0 偏移)         │
│  │   └── 中断状态                         │
│  └── Device-specific Config (MMIO BAR0)  │
│      └── (如 virtio-blk 的容量/扇区大小)  │
├──────────────────────────────────────────┤
│  virtqueue (VRing)                        │
│  ├── Descriptor Table                     │
│  ├── Available Ring                       │
│  └── Used Ring                            │
└──────────────────────────────────────────┘
```

现代 virtio-pci 使用 "modern" (virtio 1.0) 布局：所有配置区域通过一个 MMIO BAR 暴露，使用不同的偏移区分 Common/Notify/ISR/Device-specific 区域。

#### 3. axdevice（`components/axdevice/src/device.rs`）

**现状**：设备分发框架已就绪，`add_mmio_dev()` 和 `add_port_dev()` 可用，但 x86_64 上没有注册任何设备。

#### 4. axvmconfig（`components/axvmconfig/src/lib.rs`）

**现状**：`EmulatedDeviceConfig` 结构有 `base_gpa`、`length`、`irq_id`、`cfg_list` 字段，可以为 PCI 设备传递配置参数。但缺少 `PciHostBridge` 设备类型。

### 各模块修改方案

#### 3.1 新建 PCI 主桥 crate

**新建 `components/pci_host/`**，实现虚拟 PCI 主桥。

**核心数据结构**：

```rust
pub struct PciHostBridge {
    config_address: u32,           // PIO 0xCF8 的地址寄存器
    devices: Vec<Arc<dyn PciDevice>>,  // 挂载的 PCI 设备列表
    ecam_base: GuestPhysAddr,      // ECAM MMIO 基地址
    ecam_size: usize,              // ECAM 区域大小
}

pub trait PciDevice: Send + Sync {
    fn config_space(&self) -> &PciConfigSpace;
    fn config_space_mut(&mut self) -> &mut PciConfigSpace;
    fn bar_write(&mut self, bar_idx: usize, offset: usize, data: u64);
    fn bar_read(&self, bar_idx: usize, offset: usize) -> u64;
}

pub struct PciConfigSpace {
    data: [u8; 256],   // 标准配置空间（256 字节）
}
```

**PIO 方式实现**（注册为 Port I/O 设备，端口 `0xCF8~0xCFF`）：

```rust
impl BaseDeviceOps<PortRange> for PciHostBridge {
    fn address_range(&self) -> PortRange { PortRange::new(Port(0xCF8), 8) }

    fn handle_read(&self, addr: Port, width: AccessWidth) -> AxResult<usize> {
        match addr.0 {
            0xCF8 => Ok(self.config_address as usize),  // 读地址寄存器
            0xCFC..=0xCFF => {
                // 解码 config_address 得到 Bus/Dev/Func/Reg
                // 从对应设备的配置空间读取数据
            }
            _ => Ok(0),
        }
    }

    fn handle_write(&mut self, addr: Port, width: AccessWidth, value: usize) -> AxResult {
        match addr.0 {
            0xCF8 => { self.config_address = value as u32; }
            0xCFC..=0xCFF => {
                // 解码 config_address，写入对应设备的配置空间
            }
            _ => {}
        }
        Ok(())
    }
}
```

**ECAM 方式实现**（注册为 MMIO 设备）：

ECAM 地址编码：`ecam_base + (bus << 20) | (device << 15) | (function << 12) | register`

```rust
impl BaseDeviceOps<GuestPhysAddrRange> for PciHostBridge {
    fn address_range(&self) -> GuestPhysAddrRange {
        GuestPhysAddrRange::new(self.ecam_base, self.ecam_size)
    }

    fn handle_read(&self, addr: GuestPhysAddr, width: AccessWidth) -> AxResult<usize> {
        let offset = addr.as_usize() - self.ecam_base.as_usize();
        let bus = (offset >> 20) & 0xFF;
        let device = (offset >> 15) & 0x1F;
        let function = (offset >> 12) & 0x7;
        let reg = offset & 0xFFF;
        // 从对应设备的配置空间读取
    }
}
```

**为什么**：PCI 主桥是 UEFI 发现所有 PCI 设备的入口。OVMF 的 PCI Bus Driver 通过 PIO 或 ECAM 方式遍历 Bus 0 上的所有设备号，读取 Vendor ID / Device ID 来发现设备。没有 PCI 主桥，OVMF 看到的 PCI 总线是空的。

#### 3.2 新建 virtio-pci crate

**新建 `components/virtio_pci/`**，实现 virtio-pci 框架和 virtqueue。

**核心数据结构**：

```rust
pub struct VirtioPciDevice {
    config_space: PciConfigSpace,
    common_cfg: VirtioPciCommonCfg,
    notify_cfg: VirtioPciNotifyCfg,
    isr_cfg: VirtioPciIsrCfg,
    device_cfg: Vec<u8>,           // Device-specific 配置
    virtqueues: Vec<VirtQueue>,
    bar_addr: [u64; 6],            // BAR 地址
    bar_size: [u64; 6],            // BAR 大小
    irq_pin: u8,                   // INTx 引脚号
    irq_line: u8,                  // IRQ 号
}

pub struct VirtioPciCommonCfg {
    device_feature_select: u32,
    device_feature: u32,
    driver_feature_select: u32,
    driver_feature: u32,
    msix_config: u16,
    num_queues: u16,
    device_status: u8,
    config_generation: u8,
    queue_select: u16,
    queue_size: u16,
    queue_msix_vector: u16,
    queue_enable: u16,
    queue_notify_off: u16,
    queue_desc: u64,
    queue_driver: u64,
    queue_device: u64,
}

pub struct VirtQueue {
    max_size: u16,
    size: u16,
    ready: bool,
    desc_table_gpa: u64,
    avail_ring_gpa: u64,
    used_ring_gpa: u64,
}
```

**virtio-blk 请求处理**：

```rust
pub struct VirtioBlkPci {
    pci: VirtioPciDevice,
    disk: Arc<dyn BlockBackend>,  // 后端存储接口
}

pub struct VirtioBlkReq {
    req_type: u32,   // 0=read, 1=write
    reserved: u32,
    sector: u64,
    data: Vec<u8>,
    status: u8,      // 0=OK, 1=IO Error
}
```

**为什么**：virtio-blk-pci 是 UEFI 启动 OS 的最简路径。OVMF 的 `VirtioBlkDxe` 驱动会：

1. 通过 PCI 枚举发现 Vendor ID = `0x1AF4`、Device ID = `0x1001`（virtio-blk）的设备
2. 读取 BAR 地址，映射 Common/Notify/ISR/Device-specific 配置区域
3. 协商特征、设置 virtqueue、使能设备
4. 通过 virtqueue 提交 I/O 请求，读取磁盘内容
5. 从磁盘的 EFI System Partition 加载 OS 的 EFI stub

#### 3.3 修改 axvmconfig

**在 `EmulatedDeviceType` 中新增 `PciHostBridge`**：

```rust
pub enum EmulatedDeviceType {
    // 现有字段...
    PciHostBridge = 0x11,
}
```

#### 3.4 修改 axdevice

**在 `init()` 中增加 x86_64 的 PCI 主桥和 virtio-blk 分支**：

```rust
EmulatedDeviceType::PciHostBridge => {
    #[cfg(target_arch = "x86_64")]
    {
        let pci_host = PciHostBridge::new(config.base_gpa, config.length);
        this.add_port_dev(Arc::new(pci_host.clone()));
        this.add_mmio_dev(Arc::new(pci_host));
    }
}

EmulatedDeviceType::VirtioBlk => {
    #[cfg(target_arch = "x86_64")]
    {
        let virtio_blk = VirtioBlkPci::new(config.base_gpa, config.irq_id, /* disk_path */);
        // 注册到 PCI 主桥
        pci_host.add_device(Arc::new(virtio_blk.clone()));
        // 注册 MMIO BAR 区域
        this.add_mmio_dev(Arc::new(virtio_blk));
    }
}
```

**为什么**：PCI 主桥需要同时注册为 Port I/O 设备（PIO 方式）和 MMIO 设备（ECAM 方式）。virtio-blk-pci 的 PCI 配置空间由 PCI 主桥管理，但其 MMIO BAR 区域需要单独注册为 MMIO 设备，这样客户机访问 BAR 区域时 VM Exit 的 `MmioRead`/`MmioWrite` 才能正确分发。

#### 3.5 涉及文件汇总

| 文件 | 修改内容 | 修改原因 |
|------|---------|---------|
| 新建 `components/pci_host/` | PCI 主桥 + Config Space 模拟 | UEFI 枚举 PCI 设备的入口 |
| 新建 `components/virtio_pci/` | virtio-pci 框架 + virtqueue | virtio 设备的 PCI 传输层 |
| 新建 `components/virtio_blk_pci/` | virtio-blk-pci 实现 | UEFI 启动磁盘 |
| `components/axvmconfig/src/lib.rs` | `EmulatedDeviceType` 新增 `PciHostBridge` | 让 TOML 配置能声明 PCI 主桥 |
| `components/axdevice/src/device.rs` | `init()` 增加 x86_64 的 PCI/virtio 分支 | 将 PCI 主桥和 virtio-blk 注册到 VM |

---

## 第 4 阶段：完善中断链路

### 目标

短期：利用 vIOAPIC + INTx 让 virtio-pci 的基本中断跑通。中期：完善 vLAPIC 的 EOI、IPI、timer 以及 MSI/MSI-X 支持。

为什么中断链路是必需的：

- virtio-pci 设备在完成 I/O 操作后需要通过中断通知客户机。具体来说，当 Hypervisor 将 I/O 结果写入 virtqueue 的 Used Ring 后，需要通过 IOAPIC 的 INTx（或 MSI/MSI-X）向客户机注入中断。客户机的 virtio 驱动在中断处理程序中检查 Used Ring，获取 I/O 结果。
- 如果中断链路不通，virtio-blk 的 I/O 请求发出后永远得不到完成通知，UEFI 的 `VirtioBlkDxe` 驱动会无限等待，客户机卡死。
- vLAPIC 的 EOI 广播是 Level-triggered 中断的关键：virtio-pci 使用 Level-triggered INTx 中断，EOI 后必须通知 IOAPIC 清除 Remote IRR 位，否则后续中断无法注入。

### 涉及的模块及现状

#### 1. vIOAPIC — 完全不存在

没有 `x86_vioapic` crate，也没有任何 IOAPIC 模拟代码。

Intel IOAPIC 的核心功能：

- 24 个重定向表项（IOREDTBL），每个表项描述一个 GSI（Global System Interrupt）到 CPU 中断向量的映射
- MMIO 寄存器接口（`0xFEC00000`）：选择端口（offset 0x00）+ 数据端口（offset 0x10）
- 中断注入：当 GSI 被触发时，根据重定向表项的配置，向目标 CPU 的 LAPIC 发送中断
- EOI 广播：当 LAPIC 对 Level-triggered 中断执行 EOI 时，IOAPIC 需要清除对应 GSI 的 Remote IRR 位

#### 2. vLAPIC（`components/x86_vlapic/src/vlapic.rs`）

**现状**：框架已实现，但关键路径全是 `unimplemented!()`：

| 函数 | 当前状态 | 影响 |
|------|---------|------|
| `process_eoi()` | TMR 检查后 `unimplemented!("vioapic_broadcast_eoi")` | Level-triggered 中断无法完成 EOI 广播 |
| `process_eoi()` | 末尾 `unimplemented!("vcpu_make_request")` | EOI 后无法触发下一次中断注入 |
| `set_intr()` / `vlapic_accept_intr()` | `unimplemented!()` | 无法向 vCPU 注入中断 |
| `inject_nmi()` | `unimplemented!()` | NMI 中断不可用 |
| `process_init_sipi()` | `unimplemented!()` | AP 无法被唤醒（SMP 必需） |
| `handle_self_ipi()` | `unimplemented!()` | x2APIC Self-IPI 不可用 |

#### 3. 中断注入路径（`os/axvisor/src/hal/arch/x86_64/mod.rs`）

**现状**：`inject_interrupt()` 是空函数：

```rust
pub fn inject_interrupt(_vector: u8) {}
```

vCPU 的 `queue_event()` 方法已实现（通过 `pending_events` 队列 + VM-entry 注入），但上游没有调用者。

整个中断注入数据通路当前完全断开：

```
外部中断 → VM Exit → inject_interrupt() [空] → 断路
设备中断 → vIOAPIC [不存在] → ??? → vLAPIC.set_intr() [unimplemented] → 断路
```

#### 4. MSI/MSI-X — 完全不存在

`EmulatedDeviceType` 枚举中没有 MSI 相关类型。MSI/MSI-X 是中期目标，短期先用 INTx。

### 各模块修改方案

#### 4.1 新建 vIOAPIC crate

**新建 `components/x86_vioapic/`**，实现虚拟 IOAPIC。

**核心数据结构**：

```rust
pub struct VirtualIoApic {
    id: u8,
    reg_sel: u32,                        // 寄存器选择器
    redirect_table: [IoApicRedirEntry; 24],  // 24 个重定向表项
    base_gpa: GuestPhysAddr,             // MMIO 基地址
}

pub struct IoApicRedirEntry {
    vector: u8,
    delivery_mode: DeliveryMode,   // Fixed, SMI, NMI, ExtINT
    dest_mode: DestMode,           // Physical, Logical
    polarity: Polarity,            // Active High, Active Low
    trigger_mode: TriggerMode,     // Edge, Level
    mask: bool,
    remote_irr: bool,              // Level-triggered 中断的远程 IRR 状态
    destination: u8,               // 目标 APIC ID
}
```

**MMIO 接口实现**（注册为 MMIO 设备）：

```rust
impl BaseDeviceOps<GuestPhysAddrRange> for VirtualIoApic {
    fn address_range(&self) -> GuestPhysAddrRange {
        GuestPhysAddrRange::new(self.base_gpa, 0x1000)
    }

    fn handle_read(&self, addr: GuestPhysAddr, width: AccessWidth) -> AxResult<usize> {
        let offset = addr.as_usize() - self.base_gpa.as_usize();
        match offset {
            0x00 => Ok(self.reg_sel as usize),  // 读选择器
            0x10 => self.read_register(self.reg_sel),  // 读数据
            _ => Ok(0),
        }
    }

    fn handle_write(&mut self, addr: GuestPhysAddr, width: AccessWidth, value: usize) -> AxResult {
        let offset = addr.as_usize() - self.base_gpa.as_usize();
        match offset {
            0x00 => { self.reg_sel = value as u32; }
            0x10 => { self.write_register(self.reg_sel, value as u64); }
            _ => {}
        }
        Ok(())
    }
}
```

**中断注入方法**：

```rust
impl VirtualIoApic {
    pub fn inject_irq(&mut self, gsi: u8) {
        let entry = &self.redirect_table[gsi as usize];
        if entry.mask { return; }
        if entry.trigger_mode == TriggerMode::Level {
            self.redirect_table[gsi as usize].remote_irr = true;
        }
        // 向目标 vLAPIC 注入中断
        vlapic.set_intr(entry.destination, entry.vector, entry.trigger_mode);
    }

    pub fn broadcast_eoi(&mut self, vector: u8) {
        for (gsi, entry) in self.redirect_table.iter_mut().enumerate() {
            if entry.vector == vector && entry.trigger_mode == TriggerMode::Level {
                entry.remote_irr = false;
            }
        }
    }
}
```

**为什么**：vIOAPIC 是 virtio-pci INTx 中断的桥梁。virtio-blk 完成I/O 后调用 `vioapic.inject_irq(irq)`，vIOAPIC 根据重定向表项找到目标 vCPU 和中断向量，调用 vLAPIC 的 `set_intr()` 注入中断。没有 vIOAPIC，virtio 设备的中断无法到达客户机。

#### 4.2 补全 vLAPIC

**补全 `process_eoi()`**：

```rust
fn process_eoi(&mut self) {
    let vector = self.isrv;
    if vector == 0 { return; }

    // 清除 ISR 位
    let (idx, bitpos) = extract_index_and_bitpos_u32(vector);
    let mut isr = self.regs().ISR[idx].get();
    isr &= !(1 << bitpos);
    self.regs().ISR[idx].set(isr);

    // 更新 ISRV 和 PPR
    self.isrv = self.find_isrv();
    self.update_ppr();

    // 如果是 Level-triggered 中断，广播 EOI 到 IOAPIC
    if (self.regs().TMR[idx].get() as u32).bit(bitpos) {
        vioapic_broadcast_eoi(vector);  // 需要回调机制
    }

    // 触发 vCPU 事件，检查是否有待注入的中断
    vcpu_make_request(ACRN_REQUEST_EVENT);  // 需要回调机制
}
```

**补全 `set_intr()` / `vlapic_accept_intr()`**：

```rust
pub fn set_intr(&mut self, vector: u8, trigger_mode: TriggerMode) {
    let (idx, bitpos) = extract_index_and_bitpos_u32(vector);

    // 设置 IRR（Interrupt Request Register）位
    let mut irr = self.regs().IRR[idx].get();
    irr |= 1 << bitpos;
    self.regs().IRR[idx].set(irr);

    // 设置 TMR 位（Level-triggered 标记）
    if trigger_mode == TriggerMode::Level {
        let mut tmr = self.regs().TMR[idx].get();
        tmr |= 1 << bitpos;
        self.regs().TMR[idx].set(tmr);
    }

    // 通知 vCPU 有待处理的中断
    vcpu_make_request(ACRN_REQUEST_EVENT);
}
```

**补全 `process_init_sipi()`**：

```rust
fn process_init_sipi(&mut self, icr_low: InterruptCommandRegisterLowLocal) {
    match icr_low.delivery_mode() {
        APICDeliveryMode::Init => {
            // INIT：重置 vCPU 状态，等待 SIPI
            self.wait_for_sipi = true;
            // 暂停 AP vCPU
        }
        APICDeliveryMode::StartUp => {
            // SIPI：设置 AP 的入口地址并启动
            let vector = icr_low.vector();
            let ap_entry = (vector as u64) << 12;  // SIPI vector 页对齐
            self.ap_entry = ap_entry;
            self.wait_for_sipi = false;
            // 启动 AP vCPU
        }
        _ => {}
    }
}
```

**为什么**：

- `process_eoi()` 是 Level-triggered 中断（virtio-pci INTx 使用）的关键：EOI 后必须通知 IOAPIC 清除 Remote IRR，否则后续中断被阻塞；同时必须触发 vCPU 检查是否有更多待注入的中断。
- `set_intr()` 是中断注入的入口：vIOAPIC 调用它将中断请求记录到 vLAPIC 的 IRR 中，vCPU 在 VM-entry 时检查 IRR 并注入最高优先级的中断。
- `process_init_sipi()` 是 SMP 启动的关键：BSP 通过 ICR 发送 INIT+SIPI 给 AP，AP 被唤醒后从 SIPI vector 指定的地址开始执行。

#### 4.3 串通中断注入路径

**修改 `inject_interrupt()`**：

```rust
// os/axvisor/src/hal/arch/x86_64/mod.rs
pub fn inject_interrupt(vector: u8) {
    // 获取当前 vCPU
    // 调用 vcpu.queue_event(vector)
}
```

**完整的中断注入数据通路**：

```
┌──────────┐     inject_irq()     ┌──────────┐    set_intr()    ┌──────────┐
│  设备     │ ──────────────────→ │  vIOAPIC  │ ──────────────→ │  vLAPIC   │
│(virtio等) │    (GSI → vector)   │          │  (设置 IRR 位)  │          │
└──────────┘                      └──────────┘                  └────┬─────┘
                                                                     │
                                                    queue_event()     │ VM-Entry
                                                    (设置 pending)    ↓ 注入中断
                                                               ┌──────────┐
                                                               │  vCPU     │
                                                               │ 客户机执行 │
                                                               │ 中断处理   │
                                                               └────┬─────┘
                                                                    │
                                              写 EOI 寄存器          │
                                                                     ↓
                                                              ┌──────────┐
                                                              │ vLAPIC    │
                                                              │process_eoi│
                                                              └────┬─────┘
                                                                   │
                                              broadcast_eoi()      │
                                                                   ↓
                                                               ┌──────────┐
                                                               │  vIOAPIC  │
                                                               │清除Remote │
                                                               │   IRR     │
                                                               └──────────┘
```

**为什么**：这条数据通路是所有虚拟设备中断的基础。没有它，virtio-blk 完成 I/O 后无法通知客户机，客户机的 virtio 驱动会无限等待。

#### 4.4 MSI/MSI-X（中期目标）

MSI/MSI-X 不经过 IOAPIC，直接通过 MMIO 写入触发中断。实现要点：

1. 在 `VirtioPciDevice` 的 PCI 配置空间中添加 MSI/MSI-X Capability 结构
2. 客户机驱动写入 MSI Address + Data 寄存器时，解析目标 APIC ID 和中断向量
3. 直接调用 vLAPIC 的 `set_intr()` 注入中断
4. MSI-X 还需要处理 MSI-X Table 和 PBA（通过 BAR 映射的 MMIO 区域）

MSI/MSI-X 的优势：每个 virtqueue 可以独立使用一个中断向量，避免了 INTx 的共享中断和 Level-triggered 的 EOI 开销。

#### 4.5 涉及文件汇总

| 文件 | 修改内容 | 修改原因 |
|------|---------|---------|
| 新建 `components/x86_vioapic/` | vIOAPIC 实现 | INTx 中断路由的桥梁 |
| `components/x86_vlapic/src/vlapic.rs` | 补全 `process_eoi()`/`set_intr()`/`process_init_sipi()` | 串通中断注入和 EOI 广播 |
| `os/axvisor/src/hal/arch/x86_64/mod.rs` | 实现 `inject_interrupt()` | 连接外部中断到 vCPU 注入 |
| `components/axvmconfig/src/lib.rs` | `EmulatedDeviceType` 新增 `IoApic` | 让 TOML 配置能声明 vIOAPIC |
| `components/axdevice/src/device.rs` | `init()` 增加 x86_64 的 `IoApic` 分支 | 将 vIOAPIC 注册到 VM 的 MMIO 设备列表 |

---

## 第 5 阶段：验证闭环

### 目标

新建一个专门的 x86_64 UEFI 客户机配置，并编写对应的 QEMU 冒烟测试。验证路径从 OVMF 的 UEFI Shell 开始，逐步到通过 Linux EFI stub 和 virtio-block 启动完整内核。

为什么需要验证闭环：

- 前四个阶段的所有改动都是为了一件事：让 OVMF 能跑起来并启动 OS。没有端到端的验证，无法确认各模块的改动是否正确衔接。
- UEFI 启动是一个严格有序的过程：SEC → PEI → DXE → BDS → TSL → RT，每个阶段依赖前一个阶段提供的协议和设备。如果某个阶段的设备缺失或行为不正确，后续阶段会静默失败或卡死。
- 验证闭环确保每一步都能观察到正确的输出，便于定位问题。

### 涉及的模块及现状

#### 1. QEMU 配置（`os/axvisor/configs/qemu/qemu-x86_64.toml`）

**现状**：`uefi = false`，所有 x86_64 配置都标记为非 UEFI。QEMU 命令行参数中没有 pflash 驱动，没有 OVMF 固件。

#### 2. VM 配置（`os/axvisor/configs/vms/`）

**现状**：只有 `nimbos-x86_64-qemu-smp1.toml`，使用 trampoline 启动方式。没有 UEFI 客户机配置。

#### 3. Board 配置（`os/axvisor/configs/board/qemu-x86_64.toml`）

**现状**：features 列表为 `["ept-level-4", "fs", "vmx"]`，`vm_configs = []`。可能需要添加新 feature 或调整配置。

### 各模块修改方案

#### 5.1 准备 UEFI 固件和镜像

| 资源 | 来源 | 用途 |
|------|------|------|
| `OVMF_CODE.fd` | 系统包 `ovmf` 或 EDK2 编译 | UEFI 固件代码（只读） |
| `OVMF_VARS.fd` | 系统包 `ovmf` 或 EDK2 编译 | UEFI 变量存储（可读写） |
| Linux 内核（EFI stub） | 编译 `vmlinux` 或 `bzImage` | 测试用客户机 OS |
| rootfs 磁盘镜像 | Alpine/Debian 最小安装 | virtio-blk 后端存储 |

#### 5.2 新建 UEFI 客户机配置

**文件**：`os/axvisor/configs/vms/linux-x86_64-uefi.toml`

```toml
[base]
id = 0
name = "linux-uefi"
cpu_num = 1
vm_type = 2  # VMTLinux

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
  [0x0000_0000, 0x8000_0000, 0x7, 0],  # 2GB RAM, R|W|EXECUTE
  [0xFEC0_0000, 0x1000, 0x7, 0],       # vIOAPIC MMIO
  [0xFEE0_0000, 0x1000, 0x7, 0],       # vLAPIC MMIO
  [0xB000_0000, 0x1000_0000, 0x7, 0],  # PCI ECAM
]

[devices]
interrupt_mode = "emulated"
emu_devices = [
  { name = "fw-cfg", base_gpa = 0, length = 2, irq_id = 0, emu_type = "FwCfg", cfg_list = [] },
  { name = "ioapic", base_gpa = 0xFEC0_0000, length = 0x1000, irq_id = 0, emu_type = "IoApic", cfg_list = [] },
  { name = "pci-host", base_gpa = 0xB000_0000, length = 0x1000_0000, irq_id = 0, emu_type = "PciHostBridge", cfg_list = [] },
  { name = "virtio-blk", base_gpa = 0, length = 0, irq_id = 16, emu_type = "VirtioBlk", cfg_list = [] },
]
passthrough_devices = []
```

**为什么**：

- `boot_mode = "uefi"`：触发 UEFI 启动流程
- `memory_regions`：2GB RAM（UEFI 需要大量内存），加上 vIOAPIC/vLAPIC/ECAM 的 MMIO 区域
- `interrupt_mode = "emulated"`：使用模拟中断控制器而非直通
- `emu_devices`：声明所有需要的模拟设备，包括 fw_cfg、vIOAPIC、PCI 主桥、virtio-blk
- `passthrough_devices = []`：UEFI 客户机不直通任何硬件

#### 5.3 新建 QEMU 冒烟测试配置

**文件**：`os/axvisor/configs/qemu/qemu-x86_64-uefi.toml`

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

**为什么**：`uefi = true` 标记此配置为 UEFI 测试。QEMU 命令行使用 pflash 驱动加载 OVMF 固件，使用 virtio-blk-pci 挂载磁盘。`success_regex` 匹配 UEFI Shell 的输出表示 OVMF 成功启动；`fail_regex` 匹配 OVMF 的 ASSERT 或 PANIC 表示启动失败。

#### 5.4 验证里程碑

| 阶段 | 验证内容 | 成功标志 | 依赖的前序阶段 |
|------|---------|---------|---------------|
| M1 | OVMF 加载并执行 | 看到 `UEFI Interactive Shell` 输出 | 第 1 阶段（pflash 加载 + reset vector 映射） |
| M2 | fw_cfg 被正确识别 | OVMF 日志中显示 fw_cfg 相关信息，内存大小和 CPU 数量正确 | 第 2 阶段（fw_cfg 实现） |
| M3 | ACPI 表被正确解析 | OVMF 日志中显示 ACPI 表安装信息 | 第 2 阶段（ACPI 表生成） |
| M4 | PCI 设备被枚举 | UEFI Shell 中 `pci` 命令显示 virtio-blk 设备 | 第 3 阶段（PCI 主桥） |
| M5 | virtio-blk 磁盘被发现 | UEFI Shell 中 `map -r` 显示 `FS0:` 或 `Blk0:` | 第 3 阶段（virtio-blk-pci）+ 第 4 阶段（中断） |
| M6 | Linux EFI stub 启动 | 内核启动日志输出 | M5 + 磁盘上有 EFI System Partition |
| M7 | Linux 完整启动 | 到达 login 提示符 | M6 + rootfs 完整 |

**为什么**：这些里程碑是严格有序的，每个里程碑依赖前一个的成功。M1~M3 验证平台设备（第 1~2 阶段），M4~M5 验证 PCI 和 virtio（第 3 阶段），M5 同时验证中断链路（第 4 阶段），M6~M7 验证完整的 OS 启动流程。

#### 5.5 涉及文件汇总

| 文件 | 修改内容 | 修改原因 |
|------|---------|---------|
| `os/axvisor/configs/vms/linux-x86_64-uefi.toml` | 新建 UEFI 客户机 VM 配置 | 声明 UEFI 启动所需的所有参数和设备 |
| `os/axvisor/configs/qemu/qemu-x86_64-uefi.toml` | 新建 UEFI QEMU 测试配置 | 定义 QEMU 启动参数和成功/失败匹配规则 |
| `os/axvisor/configs/board/qemu-x86_64.toml` | 可能需要调整 features | 确保构建时包含 UEFI 相关 feature |

## 名词解释

以下是对笔者来说比较生疏的名词或者不熟练的技术。

| 名词 | 全称 | 说明 |
|------|------|------|
| **UEFI** | Unified Extensible Firmware Interface | 统一可扩展固件接口，替代传统 BIOS 的现代固件标准 |
| **OVMF** | Open Virtual Machine Firmware | 基于 EDK2 的开源 UEFI 固件，用于 QEMU 等虚拟机 |
| **EDK2** | EFI Development Kit 2 | UEFI 开发工具包 |
| **pflash** | Physical Flash | 物理闪存，UEFI 固件的存储介质 |
| **EFI stub** | EFI Stub | ✨Linux 内核的 EFI 启动支持，使内核可作为 EFI 可执行文件直接启动 |
| **ESP** | EFI System Partition | EFI 系统分区，FAT32 格式，存放 EFI 引导程序 |
| **SEC/PEI/DXE/BDS/TSL/RT** | - | ✨UEFI 启动阶段：安全验证→PEI核心→驱动执行环境→启动设备选择→操作系统加载→运行时服务 |
| **reset vector** | - | x86 CPU 上电后执行的第一条指令地址（`0xFFFFFFF0`） |
| **EFER** | Extended Feature Enable Register | 扩展特性使能寄存器，控制长模式等，我知道要通過MSR等訪問 |
| **vLAPIC** | Virtual LAPIC | 虚拟化的 LAPIC |
| **vIOAPIC** | Virtual IOAPIC | 虚拟化的 IOAPIC |
| **IRR/ISR/TMR** | Interrupt Request/Service/Trigger Mode Register | 中断请求/服务/触发模式寄存器 |
| **IPI** | Inter-Processor Interrupt | 处理器间中断，用于 CPU 间通信 |
| **MSI** | Message Signaled Interrupt | 消息信号中断，通过内存写入触发，不经过 IOAPIC |
| **MSI-X** | MSI Extended | MSI 的扩展版本，支持更多中断向量 |
| **Remote IRR** | Remote Interrupt Request Register | 远程中断请求寄存器，Level-triggered 中断的状态位 |
| **INIT/SIPI** | Init/Startup IPI | 初始化/启动 IPI，用于唤醒 AP 核心 |
| **BSP/AP** | Bootstrap/Application Processor | 引导处理器/应用处理器 |
| **PCI** | Peripheral Component Interconnect | 外设组件互连，标准总线协议 |
| **PCIe** | PCI Express | PCI 的串行版本，更高带宽 |
| **ECAM** | Enhanced Configuration Access Mechanism | 增强配置访问机制，通过 MMIO 访问 PCI 配置空间 |
| **BAR** | Base Address Register | 基地址寄存器，描述设备的 MMIO/PIO 区域 |
| **Config Space** | Configuration Space | PCI 配置空间（256 字节），包含设备信息和 BAR |
| **Vendor ID/Device ID** | - | 厂商 ID/设备 ID，用于识别 PCI 设备 |
| **Capability** | - | PCI 能力结构，如 MSI/MSI-X/Power Management 等 |
| **virtio** | Virtual I/O | 虚拟化 I/O 框架，定义了高效的前后端通信协议 |
| **virtqueue** | - | virtio 队列，用于前后端数据传输 |
| **VRing** | Virtual Ring | 虚拟环，virtqueue 的底层实现 |
| **Descriptor Table** | - | 描述符表，描述数据缓冲区 |
| **Available Ring** | - | 可用环，前端提交请求 |
| **Used Ring** | - | 已用环，后端返回结果 |
| **Common Config** | - | virtio-pci 的通用配置区域 |
| **Notify Config** | - | virtio-pci 的通知区域，前端通知后端 |
| **ISR Config** | - | virtio-pci 的中断状态区域 |
| **ACPI** | Advanced Configuration and Power Interface | 高级配置与电源接口，描述硬件拓扑和电源管理，注意不是 APIC |
| **RSDP** | Root System Description Pointer | 根系统描述指针，ACPI 表的入口 |
| **RSDT** | Root System Description Table | 根系统描述表（32 位指针） |
| **XSDT** | Extended System Description Table | 扩展系统描述表（64 位指针） |
| **FADT** | Fixed ACPI Description Table | 固定 ACPI 描述表，包含电源管理寄存器地址 |
| **MADT** | Multiple APIC Description Table | 多 APIC 描述表，描述 LAPIC/IOAPIC 拓扑 |
| **MCFG** | Memory-mapped Configuration Space | PCI ECAM 的基地址描述 |
| **DSDT** | Differentiated System Description Table | 差异化系统描述表，包含设备定义 |
| **SSDT** | Secondary System Description Table | 辅助系统描述表 |
| **HPET** | High Precision Event Timer | 高精度事件定时器 |
| **SPCR** | Serial Port Console Redirection | 串口控制台重定向表 |
| **SCI** | System Control Interrupt | 系统控制中断，ACPI 使用的中断号 |
| **PM1a_EVT/CTL** | Power Management 1a Event/Control | 电源管理事件/控制寄存器 |
| **VMX** | Virtual Machine Extensions | Intel 虚拟机扩展指令集 |
| **VMCS** | Virtual Machine Control Structure | 虚拟机控制结构，保存 vCPU 状态 |
| **EPT** | Extended Page Tables | 扩展页表，Intel 的二级地址翻译机制 |
| **MMIO** | Memory-Mapped I/O | 内存映射 I/O，通过内存访问设备寄存器 |
| **passthrough** | - | 设备直通，将物理设备直接分配给客户机 |
| **fw_cfg** | Firmware Configuration | QEMU 的固件配置接口，键值对形式传递启动参数 |
| **Selector** | - | 选择器端口（0x510），选择要访问的配置项 |
| **Data** | - | 数据端口（0x511），读取选中项的数据 |
| **item** | - | fw_cfg 的配置项，如 RAM 大小、CPU 数量 |
| **file** | - | fw_cfg 的文件项，如 ACPI 表、内核镜像 |

## 进度及其他参考

- 成功在 Ubuntu22 上 Docker 中编译运行 Axvisor 代码。
- 验证性分支项目：由 **Mecocoa** 提供，用于验证 UEFI 启动流程。目前已经实现并验证 Intel VT-x 功能的“初始化”（截止5月15日）。目前有 BIOS32, BIOS64, UEFI64 三种启动模式。
- **《Intel® 64 and IA-32 Architectures Software Developer’s Manual》 (Volume 3C: System Programming Guide, Part 3)**：这是你案头必备的“圣经”，尤其是 VMCS 字段定义和 Exit Reason 的解释。
- **Cloud Hypervisor / kvmtool 源码**：相较于庞大的 QEMU，这两个项目更加纯粹和现代化，重点参考它们是如何生成 ACPI 表和构建 fw_cfg 的。

