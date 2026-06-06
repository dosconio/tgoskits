# Stage 10: VM-entry 成功 → OVMF SEC 阶段启动 → 内存映射不足

## 目标

修复 Stage 9 遗留的 VM-entry 控制域校验失败问题，使 OVMF 能够真正开始执行，并推进到 SEC 阶段。

## 已解决的问题

### 1. VM-entry 控制域校验失败（Stage 7/8/9 遗留）

Stage 9 结束时，vCPU 因 VM-entry 控制域校验失败无法进入运行态。本阶段通过一系列修复使 VM-entry 成功：

#### 1a. `set_control` 函数 `mandatory1` 计算错误

[vmcs.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vmcs.rs#L707) 中 `set_control` 函数的 `mandatory1` 原来包含灵活默认位（`mandatory1 = !allowed1 | allowed0`），导致 PIN_CTRL 中 VIRTUAL_NMI 等位被强制设置，触发 NMI_WINDOW VM-exit 无限循环。

修复：`mandatory1 = allowed0`（仅包含真正的强制 1 位），灵活位通过 `set` 参数显式控制。

#### 1b. CR0 初始值计算时序问题

`fixup_ia32e_guest_cr_and_efer` 在读取 SEC_CTRL 判断 UNRESTRICTED_GUEST 支持时，SEC_CTRL 尚未写入（值为 0），导致 CR0 被强制加入 PE+PG 位。

修复：改为直接读取 MSR `IA32_VMX_PROCBASED_CTLS2` 检测 UNRESTRICTED_GUEST 支持。

#### 1c. CR0_GUEST_HOST_MASK 包含 PE/PG 位

当 UNRESTRICTED_GUEST=1 时，CR0.PE 和 CR0.PG 不应在 mask 中，否则 guest 写入 CR0 时 PE/PG 被 host 值覆盖，导致 INVALID_GUEST_STATE。

修复：当 UNRESTRICTED_GUEST=1 时，从 CR0_GUEST_HOST_MASK 中排除 PE（bit 0）和 PG（bit 31）。

#### 1d. TR unusable 导致 INVALID_GUEST_STATE

Intel SDM 要求 TR 必须为 usable（AR type byte 中 bit 16 = 0）。原来 TR AR = 0x1008b（bit 16 = 1，unusable）。

修复：TR AR 改为 0x8b。

#### 1e. NMI_WINDOW VM-exit 无限循环

PIN_CTRL 中 VIRTUAL_NMI 被设置后，CPU 在 guest 开启 NMI window 时产生 VM-exit，但 handler 没有正确处理。

修复：`mandatory1 = allowed0` 后 VIRTUAL_NMI 不再被强制设置；NMI_WINDOW handler 改为 no-op（清除 NMI_WINDOW_EXITING 位）。

#### 1f. RIP 初始值 0xFFFFFFF0 → 0xFFF0

x86 reset vector 的线性地址是 0xFFFFFFF0，但在 VMX 中 RIP 存储的是段内偏移量。当 CS.base = 0xFFFF0000 时，RIP 应为 0xFFF0（线性地址 = CS.base + RIP = 0xFFFF0000 + 0xFFF0 = 0xFFFFFFF0）。

修复：UEFI 模式下 RIP 设为 0xFFF0。

#### 1g. vmx_run 中覆盖 RIP

`vmx_run` 中用 `self.entry.unwrap().as_usize()`（= 0xFFFFFFF0）覆盖了 setup 中正确设置的 RIP = 0xFFF0。

修复：移除 vmx_run 中的 RIP 覆盖代码。

### 2. CR3 写入/读取未处理

OVMF 在 SEC 阶段设置页表时需要读写 CR3，但 `handle_cr` 只处理 CR0 和 CR4。

修复：
- 写入：将 CR3 加入 `if cr == 0 || cr == 4 || cr == 3` 条件
- 读取：添加 `access_type = 1`（move from cr）分支

### 3. CPUID leaf 0x1 返回全零

原代码在 `ecx > 0` 时返回全零，但 CPUID leaf 0x1 不使用 sub-leaf，ECX 输入值应被忽略。

修复：移除 sub-leaf 检查，始终用 ECX=0 调用 host CPUID，同时隐藏 VMX/TSC_DEADLINE/MONITOR 位，设置 HYPERVISOR 位。

### 4. EPT violation at GPA=0xC7FF01F（PCI MMIO 范围不足）

OVMF 分配的 PCI BAR 在 0xC000_0000 以上，但 PCI MMIO EPT violation 处理器只覆盖 0x8000_0000..0xC000_0000。

修复：将 PCI MMIO EPT violation 处理范围扩展到 0x8000_0000..0xFEC0_0000。

### 5. OVMF 卡在 TSC 校准循环

OVMF 在 SEC 阶段设置 APIC 定时器 ICR=0xFFFFFFFF，然后轮询 CCR（当前计数寄存器）来校准 TSC。虚拟 APIC 定时器通过 host 定时器回调实现，写入 ICR 时注册 host 定时器，但 CCR 读取时需要正确计算剩余计数。

此问题在本阶段后期自然解决——随着其他修复的推进，APIC 定时器的 CCR 递减逻辑开始正常工作，OVMF 成功通过 TSC 校准。

## 新遇到的问题

### OVMF #PF 异常：内存映射不足

OVMF 通过 TSC 校准后，在读取 IA32_APIC_BASE MSR（ECX=0x1B）时触发 #PF：

```
!!!!IA32ExceptionType-0E(#PF-Page-Fault)CPUApicID-00000000!!!!
ExceptionData-00000000  I:0R:0U:0W:0P:0PK:0SS:0SGX:0
EIP-03E5A0A3, CS-00000010, EFLAGS-00010093
EAX-00000000, ECX-0000001B, EDX-00000000, EBX-5AA55AA5
CR0-00000023, CR2-00000000, CR3-00000000, CR4-00002640
```

根因分析：
1. EPT violation at GPA=0x5AA55AA9（超出映射 RAM 范围）
2. VM 配置的 RAM 只有 32MB（0x0010_0000..0x01FF_FFFF），ram_end = 0x1FF0000（后改为 0x3EF0000）
3. OVMF SEC 阶段在建立页表前扫描内存，访问了超出映射范围的地址
4. EPT violation 处理器对超出 ram_end 的 GPA 注入 #PF，但此时 guest 尚未启用分页（CR0.PG=0），#PF handler 无法正确处理

尝试的修复：
1. 将 RAM 区域从 31 MiB 扩展到 62 MiB（0x3E0_0000）——仍不够，OVMF 访问 0x5AA55AA9
2. 将 RAM 区域扩展到 126 MiB（0x7E0_0000）——TLSF 分配器无法分配如此大的连续内存
3. QEMU 内存从 128M 增加到 256M——TLSF 仍然无法分配 126 MiB 连续内存

**当前状态：OVMF 已成功启动并执行到 SEC 阶段，但因 VM 内存映射不足导致 #PF 异常。**

## 配置变更

### VM 内存配置

[uefi-x86_64-qemu.toml](file:///home/phina/Documents/tgoskits/os/axvisor/configs/vms/uefi-x86_64-qemu.toml) memory_regions 从 31 MiB 扩展到 62 MiB：

```toml
# 修改前（Stage 9）
[0x0010_0000, 0x1F0_0000, 0x7, 0],   # RAM 0x100000-0x1FFFFFF (31 MiB) R|W|X

# 修改后（Stage 10）
[0x0010_0000, 0x3E0_0000, 0x7, 0],   # RAM 0x100000-0x3EFFFFF (62 MiB) R|W|X
```

注意：内存区域大小必须是 2 MiB（huge page）的整数倍，否则 `Layout::from_size_align` 会 panic。0x7F0_0000 = 127 MiB 不是 2 MiB 的整数倍，0x7E0_0000 = 126 MiB 是。

### QEMU 内存配置

[qemu-x86_64.toml](file:///home/phina/Documents/tgoskits/os/axvisor/configs/qemu/qemu-x86_64.toml) 从 128M 增加到 256M：

```toml
# 修改前
"-m", "128M",

# 修改后
"-m", "256M",
```

### 磁盘镜像

磁盘镜像 `/home/phina/Documents/tgoskits/tmp/axbuild/rootfs/uefi-rootfs.img` 需要同步更新 VM 配置文件：

```bash
mkdir -p /tmp/uefi-rootfs
sudo mount -o loop uefi-rootfs.img /tmp/uefi-rootfs
sudo cp os/axvisor/configs/vms/uefi-x86_64-qemu.toml /tmp/uefi-rootfs/guest/vm_default/
sudo umount /tmp/uefi-rootfs
```

## 运行命令

```bash
cargo xtask axvisor qemu \
  --config os/axvisor/configs/board/qemu-x86_64.toml \
  --vmconfigs os/axvisor/configs/vms/uefi-x86_64-qemu.toml \
  --rootfs /home/phina/Documents/tgoskits/tmp/axbuild/rootfs/uefi-rootfs.img
```

## OVMF 串口输出

OVMF 成功输出串口信息（首次看到 GUEST 串口输出）：

```
!!!!IA32ExceptionType-0E(#PF-Page-Fault)CPUApicID-00000000!!!!
ExceptionData-00000000I:0R:0U:0W:0P:0PK:0SS:0SGX:0
EIP-03E5A0A3,CS-00000010,EFLAGS-00010093
EAX-00000000,ECX-0000001B,EDX-00000000,EBX-5AA55AA5
ESP-0151EF68,EBP-0151EF80,ESI-5AA55AA5,EDI-0151EFA4
DS-00000008,ES-00000008,FS-00000008,GS-00000008,SS-00000008
CR0-00000023,CR2-00000000,CR3-00000000,CR4-00002640
...
```

## 涉及的文件

| 文件 | 变更 |
|------|------|
| [x86_vcpu/src/vmx/vmcs.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vmcs.rs#L707) | `set_control` 的 `mandatory1 = allowed0` |
| [x86_vcpu/src/vmx/vcpu.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vcpu.rs#L759-L762) | UEFI 模式 RIP=0xFFF0 |
| [x86_vcpu/src/vmx/vcpu.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vcpu.rs#L281-L282) | 移除 vmx_run 中的 RIP 覆盖 |
| [x86_vcpu/src/vmx/vcpu.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vcpu.rs#L2354) | CR3 写入支持 |
| [x86_vcpu/src/vmx/vcpu.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vcpu.rs#L2398-L2404) | CR3 读取支持（move from cr） |
| [x86_vcpu/src/vmx/vcpu.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vcpu.rs#L2440-L2448) | CPUID leaf 0x1 修复 |
| [x86_vcpu/src/vmx/vcpu.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vcpu.rs#L1881-L1886) | PCI MMIO 范围扩展到 0xFEC0_0000 |
| [x86_vcpu/src/vmx/vcpu.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vcpu.rs#L711-L731) | UNRESTRICTED_GUEST 时 CR0_MASK 排除 PE/PG |
| [uefi-x86_64-qemu.toml](file:///home/phina/Documents/tgoskits/os/axvisor/configs/vms/uefi-x86_64-qemu.toml#L41-L46) | RAM 扩展到 62 MiB |
| [qemu-x86_64.toml](file:///home/phina/Documents/tgoskits/os/axvisor/configs/qemu/qemu-x86_64.toml#L15) | QEMU 内存 128M → 256M |

## 后续工作

- **Stage 11**：解决 VM 内存映射不足的问题。可能的方向：
  1. 修改 EPT violation 处理：对 RAM 范围外但低于 4GB 的地址，不再注入 #PF，而是映射为 zero-page 或忽略访问
  2. 增加 TLSF 分配器可分配的连续内存大小（可能需要调整 axvisor 自身的内存分配策略）
  3. 让 VM 配置的 RAM 大小与 QEMU `-m` 参数匹配（128M 或 256M），需要 axvisor 能分配足够的连续内存
  4. 考虑使用 MAP_IDENTICAL 方式映射 guest 内存，避免通过 TLSF 分配
