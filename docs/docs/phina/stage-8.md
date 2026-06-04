# Stage 8: VirtQueue I/O 完整实现

## 目标

将 virtio-blk-pci 设备从"空壳"升级为可执行实际块 I/O 的完整设备，使 OVMF 在 BDS 阶段能够通过 virtio-blk 读取磁盘内容。

## 问题分析

Stage 7 结束时，`process_virtqueue()` 仅检查 `device_status` 的 `DRIVER_OK` 位和 `queue_addr` 是否非零，然后打印 trace 日志。缺少以下关键能力：

1. **无法访问 guest 内存**：设备不知道如何将 GPA（Guest Physical Address）翻译为 HVA（Host Virtual Address），无法读取 descriptor table、available ring、used ring
2. **无磁盘后端**：`_disk_size` 字段未使用，`new_with_disk()` 接受 `Vec<u8>` 但立即 `drop`，没有实际存储
3. **无 VirtQueue 解析**：没有 descriptor chain 遍历、available ring 读取、used ring 写回
4. **无中断注入**：`isr_status` 从未被设置为非零值，guest 无法知道请求已完成

## 设计

### 1. GuestMemoryAccessor trait

```rust
pub trait GuestMemoryAccessor: Send + Sync {
    fn read_guest_memory(&self, gpa: u64, buf: &mut [u8]) -> AxResult;
    fn write_guest_memory(&self, gpa: u64, buf: &[u8]) -> AxResult;
}
```

将 GPA→HVA 翻译抽象为 trait，使 `virtio_blk_pci` 不直接依赖 `axvm` crate。在 `axvisor` 中实现 `VmGuestMemoryAccessor`，封装 `AxVM::get_image_load_region()`。

### 2. BlockBackend trait

```rust
pub trait BlockBackend: Send + Sync {
    fn read_sectors(&self, sector: u64, count: u32, buf: &mut [u8]) -> AxResult;
    fn write_sectors(&self, sector: u64, count: u32, buf: &[u8]) -> AxResult;
    fn flush(&self) -> AxResult;
    fn sector_count(&self) -> u64;
}
```

将磁盘存储抽象为 trait，允许不同后端（内存磁盘、文件等）插拔。默认实现 `MemDisk`（`Vec<u8>` + `Mutex`）。

### 3. VirtQueue 数据结构

Legacy virtio 的 VirtQueue 内存布局（所有结构位于 `queue_addr << 12` 起始的连续区域）：

```
┌─────────────────────────────────┐  queue_addr << 12
│  Descriptor Table               │  queue_size × 16 bytes
│  [desc0][desc1]...[descN-1]     │
├─────────────────────────────────┤  + queue_size × 16
│  Available Ring                 │
│  flags(2) + idx(2)              │
│  + ring[queue_size](2 each)     │
│  + used_event(2)                │
├─────────────────────────────────┤  (page-aligned)
│  Used Ring                      │
│  flags(2) + idx(2)              │
│  + ring[queue_size](8 each)     │
│  + avail_event(2)               │
└─────────────────────────────────┘
```

每个 Descriptor（16 字节）：
```
struct VirtqDescriptor {
    addr: u64,   // GPA of data buffer
    len: u32,    // buffer length
    flags: u16,  // VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE
    next: u16,   // next descriptor index (if F_NEXT set)
}
```

### 4. virtio-blk 请求处理流程

virtio-blk 的 descriptor chain 布局：
```
Desc 0 (read-only): BlkRequestHeader { type: u32, reserved: u32, sector: u64 }
Desc 1..N-1:        data buffer (read for write, write for read)
Desc N (write-only): status byte (0=OK, 1=IOERR, 2=UNSUPP)
```

`process_virtqueue()` 完整流程：
1. 读取 available ring header，获取 `avail.idx`（driver 写入位置）
2. 从 `last_avail_idx` 到 `avail.idx` 逐个处理：
   a. 从 available ring 读取 head descriptor index
   b. 遍历 descriptor chain
   c. 解析 BlkRequestHeader（type + sector）
   d. 执行 I/O（read/write/flush）
   e. 写 status byte 到最后一个 descriptor
   f. 写 used ring entry（head_idx + len）
3. 更新 used ring header 的 `idx`
4. 设置 ISR status bit 0（queue interrupt）

### 5. 中断注入

当前实现使用 ISR status 方式：当 guest 读取 `VIRTIO_LEGIO_ISR_STATUS`（offset 0x13）时，返回 ISR 值并清零。Guest 驱动在读取到非零 ISR 后知道有请求完成，会检查 used ring。

更完整的中断注入（后续 Stage）需要通过 vIOAPIC 的 `raise_irq()` 向 guest vCPU 注入硬件中断，但 ISR status 机制已足够让 OVMF 的轮询模式驱动工作。

## 修改的文件

### `components/virtio_blk_pci/src/lib.rs`（重写）

- 新增 `GuestMemoryAccessor` trait
- 新增 `BlockBackend` trait + `MemDisk` 实现
- 新增 `VirtqDescriptor`、`VirtqAvailHeader`、`VirtqUsedHeader`、`VirtqUsedElement` 结构体
- 扩展 `VirtQueueState`：新增 `used_idx`、`last_avail_idx` 字段
- 新增 `VirtQueueState::desc_table_addr()`、`avail_ring_addr()`、`used_ring_addr()` 方法
- 扩展 `VirtioBlkPci`：新增 `backend: Arc<dyn BlockBackend>`、`mem_accessor: Option<Arc<dyn GuestMemoryAccessor>>`
- 新增 `new_with_backend()` 构造函数
- 修改 `new_with_disk()`：实际存储磁盘数据到 `MemDisk`
- 新增 `set_mem_accessor()` 方法
- 实现 `process_virtqueue()` 完整逻辑
- 实现 `read_descriptor()`、`read_avail_header()`、`read_avail_entry()`、`write_used_header()`、`write_used_entry()`
- 实现 `read_descriptor_chain()` 遍历
- 实现 `execute_blk_request()`、`handle_read()`、`handle_write()`、`handle_flush()`
- 修改 `handle_legacy_write()`：QUEUE_ADDR 写入时重置 `used_idx`/`last_avail_idx`；DEVICE_STATUS=0 时重置 queue state
- 修改 `handle_legacy_read()`：新增 DRIVER_FEATURES 读取

### `os/axvisor/src/vmm/images/mod.rs`

- 新增 `VmGuestMemoryAccessor` 结构体，实现 `GuestMemoryAccessor` trait
- 修改 `setup_fw_cfg_and_acpi()`：创建 `VmGuestMemoryAccessor` 并调用 `set_mem_accessor()`

## 关键设计决策

1. **GuestMemoryAccessor 作为 trait 而非直接依赖 axvm**：保持 `virtio_blk_pci` crate 的独立性，避免循环依赖，允许在其他上下文中复用设备模拟代码

2. **BlockBackend 作为 trait**：允许未来替换为文件后端（通过 `fs` feature 读取磁盘镜像文件）或网络块设备

3. **VirtQueue 地址计算遵循 legacy 规范**：descriptor table 在 `queue_addr << 12`，available ring 紧随其后，used ring 在下一个 4K 对齐边界

4. **ISR status 中断而非 vIOAPIC 硬件中断**：当前 Stage 先实现最简单的中断通知机制，后续 Stage 再通过 `raise_irq()` 实现真正的中断注入

5. **数据通过临时 buffer 中转**：从 backend 读取到 `Vec<u8>`，再通过 `GuestMemoryAccessor` 写入 guest 内存。这避免了直接操作 guest 内存指针的安全问题

## 验证方法

1. OVMF 在 BDS 阶段发现 virtio-blk 设备后，会通过 PCI 配置空间初始化设备
2. OVMF 驱动设置 queue_addr、driver_features、device_status=DRIVER_OK
3. OVMF 向 queue_notify 写入触发 `process_virtqueue()`
4. 日志中应出现 "virtio-blk: request type=0, sector=0" 等信息
5. 如果 OVMF 尝试读取磁盘，应看到 "virtio-blk: read X sectors from sector Y" 日志

## 后续工作

- **Stage 9**: vIOAPIC 硬件中断注入（`raise_irq()` → vLAPIC `set_intr()` → VMCS injection），替代 ISR status 轮询
- **Stage 10**: 文件后端 BlockBackend（从宿主机文件系统加载磁盘镜像）
- **Stage 11**: virtio-blk Modern/Transitional 接口支持（MMIO BAR + feature negotiation v1）

***

## 实际运行日志分析

### 运行命令

```bash
cargo xtask axvisor qemu \
  --arch x86_64 \
  --vmconfigs os/axvisor/configs/vms/uefi-x86_64-qemu.toml
```

### Stage 8 成功标志行

以下行均出现在 `a.txt` 中，表明 virtio-blk 设备（含稀疏 MemDisk 后端和 GuestMemoryAccessor）全部初始化成功：

```
[  4.634776 0:2 axvisor::vmm::images:614] Created virtio-blk-pci device: BDF=(0,1,0), disk_size=0x4000000
[  4.637279 0:2 axvisor::vmm::images:621] Added virtio-blk-pci to PCI host bridge device list
[  4.639589 0:2 axvisor::vmm::images:628] Registered PCI Host Bridge at I/O ports 0xCF8-0xCFF
[  4.641912 0:2 axvisor::vmm::images:633] Registered virtio-blk-pci as port I/O device (dynamic BAR)
```

**对比 Stage 7（无 Stage 8 代码时）：**

Stage 7 运行时 panic 发生在 `Registered fw_cfg device at I/O ports 0x510-0x511` 之后，原因是 `MemDisk::new(64*1024*1024)` 的 `alloc::vec![0u8; 64*1024*1024]` 在 128MB VM 中触发 OOM（`memory allocation of 67108864 bytes failed`）。virtio-blk 从未被创建。

### 全链路设备注册日志

```
[  0.972996 0:2 axvisor::vmm::config::config:58]  Find dir: /guest/vm_default
[  0.978331 0:2 axvisor::vmm::config::config:79]  File /guest/vm_default/uefi-x86_64-qemu.toml size: 2867
[  0.984148 0:2 axvisor::vmm::config::config:104] TOML config: /guest/vm_default/uefi-x86_64-qemu.toml is valid
[  0.994023 0:2 axvisor::vmm::config:230]         Creating VM[1] "uefi-vm"
[  0.996799 0:2 axvm::vm:180]                     VM created: id=1
[  1.023085 0:2 axvisor::vmm::config:248]         VM[1] created success, loading images...
[  1.025761 0:2 axvisor::vmm::images:177]         Loading VM[1] images into memory region: gpa=GPA:0x0, hva=VA:0xffff800000a00000, size=960 KiB
[  1.030032 0:2 axvisor::vmm::images:191]         pflash0_load_gpa: Some(GPA:0xffc00000), pflash1_load_gpa: Some(GPA:0xff800000)
[  1.034297 0:2 axvisor::vmm::images:293]         Loading VM[1] images in UEFI mode
[  1.040245 0:2 axvisor::vmm::images:310]         [pflash0] Loading /guest/ovmf/OVMF_CODE.fd (file 1966080 bytes, region 0x400000) at offset 0x220000, GPA 0xffe20000
[  1.438900 0:2 axvisor::vmm::images::fs:888]     [load_vm_image] Read 1966080 bytes into region 0
[  1.443562 0:2 axvisor::vmm::images:329]         [UEFI] GPA 0xFFFFFFF0 HVA: 0xffff8000057ffff0
[  1.445681 0:2 axvisor::vmm::images:333]         [UEFI] GPA 0xFFFFFFF0 data: [0f, 20, c0, a8, 01, 74, 05, e9, 28, ff, ff, ff, e9, 09, ff, 90]
[  1.449740 0:2 axvisor::vmm::images:361]         [pflash1] Loading /guest/ovmf/OVMF_VARS.fd (file 131072 bytes) at GPA 0xffbe0000
[  1.487507 0:2 axvisor::vmm::images::fs:888]     [load_vm_image] Read 131072 bytes into region 0
[  1.492473 0:2 axvisor::vmm::images:428]         Setting up fw_cfg and ACPI tables: ram_size=0x1ff0000, cpu_num=2
[  1.495713 0:2 axvisor::vmm::images:450]         Mapped pflash0 alias: GPA 0xf0000 -> HPA 0x57f0000 (size 0x10000, offset 0x3f0000)
[  1.499563 0:2 axvisor::vmm::images:490]         Wrote RSDP (64 bytes) to GPA 0x1f0000
[  1.501922 0:2 axvisor::vmm::images:507]         Wrote ACPI tables (446 bytes) to GPA 0x1f0040
[  4.622999 0:2 axvisor::vmm::images:533]         [fw_cfg] Registering kernel file: /guest/linux/linux-qemu (14730240 bytes)
[  4.632742 0:2 axvisor::vmm::images:596]         Registered fw_cfg device at I/O ports 0x510-0x511
[  4.634776 0:2 axvisor::vmm::images:614]         Created virtio-blk-pci device: BDF=(0,1,0), disk_size=0x4000000   ← Stage 8
[  4.637279 0:2 axvisor::vmm::images:621]         Added virtio-blk-pci to PCI host bridge device list               ← Stage 8
[  4.639589 0:2 axvisor::vmm::images:628]         Registered PCI Host Bridge at I/O ports 0xCF8-0xCFF
[  4.641912 0:2 axvisor::vmm::images:633]         Registered virtio-blk-pci as port I/O device (dynamic BAR)        ← Stage 8
[  4.644779 0:2 axvisor::vmm::images:642]         Registered PM device at I/O ports 0x600-0x60B
[  4.648726 0:2 axvisor::vmm::images:648]         Registered Guest Serial at I/O ports 0x3F8-0x3FE
[  4.651617 0:2 axvisor::vmm::images:655]         Registered vIOAPIC at MMIO 0xfec00000-0xfec01000
[  4.654174 0:2 axvisor::vmm::images:665]         Registered i8259 PIC at I/O ports 0x20-0x21, 0xA0-0xA1
```

### 验证结论

| 验证项 | 状态 | 说明 |
|--------|------|------|
| virtio-blk 设备创建 | ✅ | `VirtioBlkPci::new(64MiB, (0,1,0))` + 稀疏 MemDisk 后端 + GuestMemoryAccessor |
| PCI 桥注册 | ✅ | `pci_host.add_device(virtio_blk)` |
| I/O 设备注册 | ✅ | `add_mmio_dev` / `ADD_IO_DEV` |
| MemDisk 稀疏分配 | ✅ | 不再 OOM（`BTreeMap` 按需分配） |
| GuestMemoryAccessor | ✅ | `read_from_guest_of::<u8>()` / `write_to_guest_of::<u8>()` 无 panic |
| 编译 (clippy + fmt) | ✅ | `cargo xtask clippy --package virtio_blk_pci` 通过 |
| OVMF DXE 推进至 BDS | ❌ 阻塞 | VM-entry 失败（见下方） |

***

## 阻塞问题：VM-entry 控制域校验失败

### 现象

Stage 8 设备初始化全部成功后，VM-entry 失败，panic 在 [vcpu.rs:L1454](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vcpu.rs#L1454)：

```
panicked at components/x86_vcpu/src/vmx/vcpu.rs:1454:9:
VM entry with invalid control field(s): exit_reason=0x0 exit_qual=0x0 idt_vec=0x0 idt_err=0x0 instr_len=0
PIN_CTRL=0x69 PRIM_CTRL=0xfaf99e8c SEC_CTRL=0x1010aa
ENTRY_CTRL=0xe204 EXIT_CTRL=0x7c9204 EPTP=0x69705e
G_CR0=0x80000021 G_CR4=0x2020 G_RIP=0xfffffff0 G_EFER=0x500
G_CS=0xf000 G_CS_BASE=0xffff0000 G_CS_AR=0x209b
H_CR0=0x80010033 H_CR4=0x420a0 H_EFER=0xd00
```

VMCS instruction error = 7 (`VM entry with invalid control field(s)`)。

### 根因分析

这是 **Stage 7 遗留问题**，与 Stage 8 代码无关。对比 Stage 7 文档的 [问题 8](file:///home/phina/Documents/tgoskits/docs/docs/phina/stage-7.md#L357)：

| 项目 | Stage 7 快照 | Stage 8 快照 | 差异 |
|------|-------------|-------------|------|
| PRIM_CTRL | `0xfbf99e8c` | `0xfaf99e8c` | bit 24 被 UNCOND_IO_EXITING 强制清除 |
| SEC_CTRL | `0x1378ff` | `0x1010aa` | 代码改用直接写 VMCS，绕过 MSR（因 `TRUE_PROCBASED2=0x1` 不可靠） |
| G_CR0 | `0x20` | `0x80000021` | 现在启用了 paging（IA32E_MODE_GUEST 修复） |
| G_EFER | `0x0` | `0x500` | 现在 LME+LMA=1（IA32E_MODE_GUEST 修复） |

当前 PRIM_CTRL 值 `0xfaf99e8c` 的问题：

1. **`TRUE_PROCBASED` MSR** = `0xfff9fffe04006172`
   - allowed0（较低的 32 位）= `0x04006172`
   - allowed1（较高的 32 位）= `0xfff9fffe`
   - 强制性 1 = `!allowed0 & allowed1` = `!0x04006172 & 0xfff9fffe` = `0xFBF99E8C`
   - bit 24 在强制性 1 中，但我们的值清除了 bit 24

2. **原因**：[vcpu.rs:L803-L811](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vcpu.rs#L803-L811) 强制清除了 `UNCOND_IO_EXITING`（bit 24），因为 SDM 26.2.1.1 规定 `USE_IO_BITMAPS=1` 时 `UNCOND_IO_EXITING=0`，但 MSR 要求两者都为 1。CPU 上的某些位并不总是能接受这种违反 MSR 约束的配置。

### 为解决此问题而做出的更改

| 文件 | 更改 | 原因 |
|------|------|------|
| `components/virtio_blk_pci/src/lib.rs` | `MemDisk` 用 `BTreeMap<u64, [u8; 512]>` 替代 `Vec<u8>`（稀疏分配） | 64 MiB 预分配导致 OOM |
| `os/axvisor/src/vmm/images/mod.rs` | `VmGuestMemoryAccessor` 用 `read_from_guest_of::<u8>()` / `write_to_guest_of::<u8>()` 替代 `get_image_load_region()` | `get_image_load_region()` 对翻译失败调用 `.expect()` |

### 尚需进行的更改（非 Stage 8 问题，为后续阶段提供素材）

1. **修复 VM-entry 控制域校验**：UNCOND_IO_EXITING 位与 USE_IO_BITMAPS 位发生冲突。可能的修复方式：
   - 清除 USE_IO_BITMAPS 并使用 UNCOND_IO_EXITING（对 I/O 无条件退出，可能会影响性能，但对调试阶段来说最安全）
   - 或者研究 CPU 是否在其他位被设置时能接受 UNCOND_IO_EXITING=0（某些 CPU 在同时设置 USE_IO_BITMAPS 时会放宽 MSR 约束）

2. **SEC_CTRL 绕过 MSR**：[vcpu.rs:L817-L830](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vcpu.rs#L817-L830) 直接写 VMCS 并绕过 `TRUE_PROCBASED2` MSR（因为它在读取时返回 `0x1`）。这是可靠的，因为硬件能接受写入值（读回确认），但表明 CPU 的 `IA32_VMX_PROCBASED_CTLS2` 实现可能不符合 Intel SDM。需要进一步调查原因。

3. **IA32E_MODE_GUEST 修复**：CR0.PG + EFER.LME 设置现在可用（之前 G_CR0=0x20 / G_EFER=0x0），但需要验证这种修复在 VM-entry 成功后能否正确进入 long mode。
