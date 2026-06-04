# Stage 7: SMP 多 vCPU 启动 (INIT/SIPI)

> 日期：2026-05-31
> 参考：[20260530.md](../development/20260530.md) Step 7

***

## 一、目标

1. 实现 vLAPIC ICR 写入检测 → INIT/SIPI 处理
2. 实现 AP vCPU 初始化（VMCS + EPT + 实模式启动状态）
3. 实现 vCPU 间同步（BSP 通过 INIT/SIPI 唤醒 AP）
4. 扩展 VM 配置文件支持多 vCPU
5. 更新 QEMU 启动参数支持 SMP

***

## 二、整体架构

```
OVMF BSP (vCPU0)
    │
    ├─ 读取 MADT 表（含 2 个 LAPIC 条目）
    ├─ 写入 vLAPIC ICR（INIT → SIPI → APIC ID=1）
    │
    ▼
vLAPIC: process_init_sipi()
    │  └─ 生成 PendingInitSipi 事件
    ▼
VmxVcpu::run()
    │  └─ 检测 take_pending_init_sipi()
    │  └─ 返回 AxVCpuExitReason::CpuUp { target_cpu, entry_point }
    ▼
vcpu_run() → CpuUp 处理分支
    │  └─ 查找 target_vcpu_id（通过 vcpu_affinities 映射）
    │  └─ vcpu_on(vm, target_vcpu_id, entry_point)
    │      └─ 设置 AP vCPU 入口地址
    │      └─ 分配 vCPU 任务
    │      └─ 加入 wait_queue
    ▼
AP vCPU (vCPU1) 启动
    └─ inner_run() → vmlaunch（实模式、CS_BASE=0）
```

***

## 三、实现细节

### 3.1 vLAPIC: INIT/SIPI 处理

**修改文件:** [components/x86_vlapic/src/vlapic.rs](file:///home/phina/Documents/tgoskits/components/x86_vlapic/src/vlapic.rs)

#### 3.1.1 PendingInitSipi 结构体

定义待处理的 INIT/SIPI 请求，存储发送给 vCPU 的 INIT/StartUp 信息：

```rust
#[derive(Debug, Clone, Copy)]
pub struct PendingInitSipi {
    pub target_cpu: u32,
    pub mode: APICDeliveryMode,
    pub vector: u32,
}
```

#### 3.1.2 process_init_sipi 方法

在 ICR 写入处理中，当 Delivery Mode 为 INIT 或 StartUp 时，调用此方法生成 pending 请求：

```rust
fn process_init_sipi(
    &mut self,
    vcpu_id: u32,
    mode: APICDeliveryMode,
    icr_low: InterruptCommandRegisterLowLocal,
) {
    let vector = icr_low.read(INTERRUPT_COMMAND_LOW::Vector);
    self.pending_init_sipi = Some(PendingInitSipi {
        target_cpu: vcpu_id,
        mode,
        vector,
    });
}
```

#### 3.1.3 take_pending_init_sipi 方法

提供接口让 VCPU 模块拉取待处理的 INIT/SIPI 请求：

```rust
pub fn take_pending_init_sipi(&mut self) -> Option<PendingInitSipi> {
    self.pending_init_sipi.take()
}
```

#### 3.1.4 导出接口

在 [lib.rs](file:///home/phina/Documents/tgoskits/components/x86_vlapic/src/lib.rs) 中：
- `pub use vlapic::PendingInitSipi;` — 导出结构体
- `pub fn take_pending_init_sipi(&self) -> Option<PendingInitSipi>` — 暴露给 VCPU

#### 3.1.5 APIC ID 初始化

在 `VirtualApicRegs::new()` 中将 vCPU ID 写入 APIC ID 寄存器（偏移 0x20）：

```rust
unsafe {
    let id_ptr = apic_frame.as_mut_ptr().cast::<u8>().add(0x20) as *mut u32;
    *id_ptr = vcpu_id as u32;
    let version_ptr = apic_frame.as_mut_ptr().cast::<u8>().add(0x30) as *mut u32;
    *version_ptr = 0x0105_0014;
}
```

### 3.2 VCPU: CpuUp 退出原因处理

**修改文件:** [components/x86_vcpu/src/vmx/vcpu.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vcpu.rs)

#### 3.2.1 run() 方法中的 INIT/SIPI 检查

在 `run()` 方法末尾，在处理普通 VM-Exit 之后，检查是否有 pending INIT/SIPI：

```rust
fn run(&mut self) -> AxResult<AxVCpuExitReason> {
    let result = match self.inner_run() {
        // ... 现有 VM-Exit 处理 ...
    };

    if let Some(sipi) = self.vlapic.take_pending_init_sipi() {
        let entry_point = if sipi.vector != 0 {
            GuestPhysAddr::from((sipi.vector as usize) << 12)
        } else {
            GuestPhysAddr::from(0x0)
        };
        return Ok(AxVCpuExitReason::CpuUp {
            target_cpu: sipi.target_cpu as u64,
            entry_point,
            arg: 0,
        });
    }

    result
}
```

#### 3.2.2 inner_run() 中 AP vCPU 启动优化

修改 `inner_run()` 的 VMCS 初始化逻辑，区分 BSP 和 AP 的启动入口：

- BSP (entry == 0xFFFFFFF0)：设置 `CS_BASE=0xFFFF0000, CS_SELECTOR=0xF000, RIP=0xFFF0`
- AP (entry != 0xFFFFFFF0)：设置 `CS_BASE=0, CS_SELECTOR=0, RIP=entry_point`

```rust
let rip_val = self.entry.unwrap().as_usize();
if rip_val == 0xFFFFFFF0 {
    VmcsGuestNW::RIP.write(0xFFF0).unwrap();
} else {
    VmcsGuestNW::RIP.write(rip_val).unwrap();
    VmcsGuestNW::CS_BASE.write(0).unwrap();
    VmcsGuest16::CS_SELECTOR.write(0).unwrap();
}
```

### 3.3 vcpus: CpuUp 事件处理与 vcpu_on

**修改文件:** [os/axvisor/src/vmm/vcpus.rs](file:///home/phina/Documents/tgoskits/os/axvisor/src/vmm/vcpus.rs)

#### 3.3.1 vcpu_on 函数

实现 AP vCPU 启动逻辑：

```rust
fn vcpu_on(vm: VMRef, vcpu_id: usize, entry_point: GuestPhysAddr, arg: usize) {
    let vcpu = vm.vcpu_list()[vcpu_id].clone();
    assert_eq!(vcpu.state(), VCpuState::Free);

    vcpu.set_entry(entry_point).expect("vcpu_on: set_entry failed");
    vcpu.set_gpr(0, arg);

    let vcpu_task = alloc_vcpu_task(&vm, vcpu);
    VM_VCPU_TASK_WAIT_QUEUE
        .get_mut(&vm.id()).unwrap()
        .add_vcpu_task(vcpu_task);
}
```

#### 3.3.2 CpuUp 处理分支

在 `vcpu_run()` 的 match 分支中添加 CpuUp 处理：

```rust
AxVCpuExitReason::CpuUp { target_cpu, entry_point, arg } => {
    let vcpu_mappings = vm.get_vcpu_affinities_pcpu_ids();
    let target_vcpu_id = vcpu_mappings
        .iter()
        .find_map(|(vcpu_id, _, phys_id)| {
            if *phys_id == target_cpu as usize { Some(*vcpu_id) }
            else { None }
        })
        .unwrap_or_else(|| panic!("Physical CPU ID {} not found", target_cpu));

    vcpu_on(vm.clone(), target_vcpu_id, entry_point, arg as _);
    vcpu.set_gpr(0, 0);  // 返回值设为 0 表示成功
}
```

### 3.4 ACPI MADT: 多 CPU 条目

**修改文件:** [components/acpi_tables/src/lib.rs](file:///home/phina/Documents/tgoskits/components/acpi_tables/src/lib.rs)

#### 3.4.1 LAPIC 条目 Flags 字段

每个 LAPIC 条目需要 8 字节（原实现少了 4 字节的 Flags 字段）：

```rust
for cpu_id in 0..self.config.cpu_num {
    builder.append_u8(0);                        // Type: Processor Local APIC
    builder.append_u8(lapic_entry_size as u8);   // Length: 8
    builder.append_u8(cpu_id as u8);             // ACPI Processor UID
    builder.append_u8(cpu_id as u8);             // APIC ID
    builder.append_u32(1);                       // Flags: Enabled
}
```

### 3.5 配置更新

#### 3.5.1 VM 配置文件

**文件:** [os/axvisor/configs/vms/uefi-x86_64-qemu.toml](file:///home/phina/Documents/tgoskits/os/axvisor/configs/vms/uefi-x86_64-qemu.toml)

```toml
[base]
cpu_num = 2
phys_cpu_sets = [1, 2]
```

- `cpu_num = 2`：VM 拥有 2 个 vCPU
- `phys_cpu_sets = [1, 2]`：vCPU0 绑定到物理 CPU0，vCPU1 绑定到物理 CPU1

#### 3.5.2 QEMU 启动参数

**文件:** [os/axvisor/configs/qemu/qemu-x86_64.toml](file:///home/phina/Documents/tgoskits/os/axvisor/configs/qemu/qemu-x86_64.toml)

```toml
args = ["-smp", "2", ...]
```

#### 3.5.3 SMP 参数动态应用

**文件:** [scripts/axbuild/src/axvisor/rootfs.rs](file:///home/phina/Documents/tgoskits/scripts/axbuild/src/axvisor/rootfs.rs)

在 QEMU 配置加载后动态应用 SMP 参数：

```rust
qemu_test::apply_smp_qemu_arg(&mut qemu, request.smp);
```

**文件:** [scripts/axbuild/src/test/qemu.rs](file:///home/phina/Documents/tgoskits/scripts/axbuild/src/test/qemu.rs)

```rust
pub(crate) fn apply_smp_qemu_arg(qemu: &mut QemuConfig, smp: Option<usize>) {
    let Some(cpu_num) = smp else { return; };
    QemuArgsMut::new(&mut qemu.args)
        .set_option_value("-smp", cpu_num.to_string());
}
```

### 3.6 Rootfs 镜像更新

运行时从 `/guest/vm_default/uefi-x86_64-qemu.toml` 读取 VM 配置，此文件位于 rootfs 镜像中。需要将修改后的配置文件更新到 rootfs：

```bash
mkdir -p /tmp/rootfs_mnt
sudo mount -o loop tmp/axbuild/rootfs/rootfs-x86_64-alpine.img /tmp/rootfs_mnt
sudo cp os/axvisor/configs/vms/uefi-x86_64-qemu.toml \
    /tmp/rootfs_mnt/guest/vm_default/uefi-x86_64-qemu.toml
sudo umount /tmp/rootfs_mnt
```

***

## 四、验证

### 4.1 编译验证

```bash
cargo xtask clippy --package x86_vlapic    # ✓ 通过
cargo xtask clippy --package x86_vcpu       # ✓ 通过
cargo xtask axvisor build --arch x86_64 -c uefi-x86_64-qemu  # ✓ 编译成功
```

### 4.2 运行验证

```bash
timeout 25 cargo xtask axvisor qemu --config os/axvisor/configs/board/qemu-x86_64.toml 2>&1
```

关键日志输出：

```
qemu-system-x86_64 ... -smp 2 ...                    # ✓ QEMU 以 2 核启动

Setting up fw_cfg and ACPI tables: ... cpu_num=2      # ✓ ACPI 表包含 2 个 CPU
[HV] created VmxVcpu(vmcs: PA:0x6692000)              # ✓ BSP vCPU 创建
[HV] created VmxVcpu(vmcs: PA:0x6697000)              # ✓ AP vCPU 创建
Initializing VM[1]'s 2 vcpus                          # ✓ 2 个 vCPU 初始化
Spawning task for VM[1] VCpu[0]                       # ✓ BSP 任务创建
```

### 4.3 验证结论

SMP 基础设施已正确就位：
- ✅ vLAPIC INIT/SIPI 处理链路
- ✅ CpuUp 退出原因生成与处理
- ✅ vcpu_on AP vCPU 启动逻辑
- ✅ ACPI MADT 多 LAPIC 条目
- ✅ VM 配置 cpu_num=2
- ✅ QEMU -smp 2

目前的阻塞点在于 OVMF 在 CPUID 枚举后陷入 EPT_VIOLATION (@ GPA=0x80000008) 死循环，导致无法推进到 IOAPIC/LAPIC 初始化阶段，因此 INIT/SIPI 端到端流程尚未触发。这是 Step 5（CPUID 子叶修复）的遗留问题，需在后续阶段修复。

***

## 五、问题与后续

### 5.1 已知问题

| 问题 | 影响 | 状态 |
|------|------|------|
| OVMF EPT_VIOLATION 死循环 (GPA=0x80000008) | 阻塞 INIT/SIPI 端到端验证 | Step 5 遗留 |
| ArceOS 内核 `smp=1` | AP vCPU 任务只能分时共享单核 | 可调整 build config |
| vCPU affinity 映射依赖 `phys_cpu_ids`/`phys_cpu_sets` | 需确保配置一致性 | 已处理 |

### 5.2 后续优化

1. **Step 5 修复**: 修复 CPUID 子叶死循环，打通 OVMF → Linux 链路
2. **vLAPIC EOI 广播**: 实现 EOI 向 vIOAPIC 的回写，支持 Level 触发中断
3. **MSI/MSI-X 支持**: 实现 PCI MSI 中断路由

***

## 六、VMX 控制域调试记录 (2026-06-02)

> 本章记录在解决 **"VM entry with invalid control field(s)"** (VmxInstructionError code 7) 问题过程中遇到的所有问题。当前进展：VM-entry 仍在 panic，但大部分控制域配置问题已定位。

### 6.1 问题总览

| # | 问题发现阶段 | 问题描述 | 状态 | 根因与处理方式 |
|---|------------|---------|------|--------------|
| 1 | 第 1 轮运行 | `SECONDARY_PROCBASED_EXEC_CONTROLS` 直接写 VMCS，未通过 MSR 能力检查 | ✅ 已解决 | 硬件通过 `IA32_VMX_PROCBASED_CTLS2` 强制 mandatory1=0x1378fe（含 VMCS_SHADOWING、VIRTUALIZE_X2APIC、ENABLE_PML 等），直接写入值与 MSR 不一致。**修复**：改用 `vmcs::set_control()`，通过 MSR allowed0/allowed1 计算合规值。 |
| 2 | 第 1 轮修复 | `set_control` 后手动设置 VIRTUALIZE_APIC，强制启用了硬件不支持的位 | ✅ 已解决 | 在 set_control 后面又写了一次 VMCS，覆盖了 MSR 能力约束。**修复**：移除手动设置代码，只保留 set_control。 |
| 3 | 第 1 轮运行 | EPT A/D flags 在硬件不支持时被启用 | ✅ 已解决 | EPTP 的 bit 6 (Enable Accessed/Dirty) 依赖 IA32_VMX_EPT_VPID_CAP bit 21，硬件不支援时设置此位会报错。**修复**：在 `set_ept_pointer()` 中检查 MSR，不支持则清除。 |
| 4 | 第 1 轮修复 | `VMENTRY_CONTROLS` 直接写 VMCS，未通过 MSR 能力检查 | ✅ 已解决 | 与问题 1 同类模式，`.`write()` 绕过 IA32_VMX_TRUE_ENTRY_CTLS 校验。**修复**：改用 `vmcs::set_control()`。 |
| 5 | 第 1 轮修复 | 编译错误：找不到 `VmcsReadOnlyNW` | ✅ 已解决 | 新增的 `vmx_entry_failed()` 诊断函数使用了此类型但未导入。**修复**：在 `vmcs` 模块引用中添加。 |
| 6 | 第 2 轮运行 | Guest CR4 未设 PAE，违反 ENTRY_CTRL 的 IA32E_MODE_GUEST 约束 | ✅ 已解决 | 硬件强制 ENTRY_CTRL.IA32E_MODE_GUEST=1，SDM 要求此时 CR4.PAE=1。原始 G_CR4=0x2000 (PAE=0)。**修复**：额外设置 `Cr4Flags::PHYSICAL_ADDRESS_EXTENSION`，G_CR4→0x2020。 |
| 7 | 第 2 轮运行 | VIRTUALIZE_X2APIC=1 但 VIRTUALIZE_APIC=0，违反 SDM 交叉依赖 | ✅ 已修复（待验证） | MSR 强制 mandatory1 包含 VIRTUALIZE_X2APIC=1，但不强制 VIRTUALIZE_APIC。SDM 要求两者必须同时为 1 或同时为 0。不能清除 X2APIC（MSR 禁止），只能主动设置 APIC=1。**修复**：在 setup 代码中检测并补设 VIRTUALIZE_APIC 位，同时配置 APIC_ACCESS_ADDR。最新 SEC_CTRL=0x1378ff（含 APIC 位）。 |
| 8 | 第 2 轮运行 | VM-entry 失败：原因仍为 "invalid control field(s)" | ❌ **未解决** | 即使设置了 VIRTUALIZE_APIC，VM-entry 仍然失败。当前所有可见控制域值均符合 MSR 约束（SEC_CTRL=0x1378ff、ENTRY_CTRL=0xe204、PIN_CTRL=0x69、PRIM_CTRL=0xfbf99e8c），且 G_CR4=0x2020 满足 PAE 要求。**可能原因**：还存在 VMCS 其他字段的合法性问题，如 VMCS shadowing (bit 14) 需要额外的 VMCS_LINK_POINTER 设置，或在 UNRESTRICTED_GUEST+IA32E_MODE_GUEST 组合下对 Guest 状态字段有额外约束。**需进一步诊断**：比对 Intel SDM 的 VM-entry 检查列表，逐条验证。 |

### 6.2 当前 VMCS 状态快照（最新运行）

```
PANIC: VM entry with invalid control field(s)
PIN_CTRL=0x69          PRIM_CTRL=0xfbf99e8c   SEC_CTRL=0x1378ff
ENTRY_CTRL=0xe204      EXIT_CTRL=0x7c9204     EPTP=0x69005e
G_CR0=0x20             G_CR4=0x2020           G_RIP=0xfffffff0
G_EFER=0x0             G_CS=0xf000            G_CS_BASE=0xffff0000
G_CS_AR=0x9b
H_CR0=0x80010033       H_CR4=0x420a0          H_EFER=0xd00
```

### 6.3 SEC_CTRL=0x1378ff 逐位分析

| 位 | 名称 | 当前值 | 合法性 | 备注 |
|----|------|--------|--------|------|
| 0 | VIRTUALIZE_APIC | 1 | ✓ | 手动补设，已满足 X2APIC 交叉依赖 |
| 1 | ENABLE_EPT | 1 | ✓ | |
| 2 | DTABLE_EXITING | 1 | ✓ | mandatory1 |
| 3 | ENABLE_RDTSCP | 1 | ✓ | mandatory1 |
| 4 | VIRTUALIZE_X2APIC | 1 | ✓ | mandatory1，依赖 VIRTUALIZE_APIC=1 ✓ |
| 5 | ENABLE_VPID | 1 | ✓ | |
| 6 | WBINVD_EXITING | 1 | ✓ | mandatory1 |
| 7 | UNRESTRICTED_GUEST | 1 | ✓ | |
| 12 | ENABLE_INVPCID | 1 | ✓ | |
| 14 | VMCS_SHADOWING | 1 | ⚠️ | mandatory1。设了 VMREAD/WRITE_BITMAP_ADDR，但 VMCS 阴影需要 VMCS_LINK_POINTER (guest 域 0x2800) 配置为 shadow VMCS region 的物理地址，当前 LINK_PTR=0xffffffffffffffff（表示不链接到 shadow VMCS）。**需确认 SDM 对 VMCS_SHADOWING=1 时 LINK_POINTER 的要求** |
| 17 | ENABLE_PML | 1 | ✓ | mandatory1。已配置 PML_ADDR |
| 20 | ENABLE_XSAVES_XRSTORS | 1 | ✓ | mandatory1。已配置 XSS_EXITING_BITMAP |

### 6.4 剩余未解决问题的深入分析

**问题 8 的潜在根因（按可能性排序）：**

1. **VMCS shadowing 不完整（可能性高）**
   - 硬件强制 VMCS_SHADOWING=1（bit 14），但 guest LINK_POINTER 设为 `0xffffffffffffffff`（表示"无 shadow VMCS"）。
   - 根据 Intel SDM Vol. 3C 25.6.2：当 VMCS_SHADOWING=1 时，VMREAD/VMWRITE 指令的行为发生变化，但 LINK_POINTER 在 VM-entry 时会被检查。
   - 需要查阅 SDM 关于 VMCS_SHADOWING=1 且 LINK_POINTER=all-1 时的精确行为。

2. **Guest 状态在 UNRESTRICTED_GUEST+IA32E_MODE_GUEST 组合下的额外约束（可能性中）**
   - IA32E_MODE_GUEST=1 要求 guest 处于 IA-32e 模式，但当前 guest 是实模式启动（CR0.PE=0, CR0.PG=0）。
   - UNRESTRICTED_GUEST=1 允许 guest 在实模式下运行，但与 IA32E_MODE_GUEST=1 的组合是否合法？
   - 如果硬件要求 IA32E_MODE_GUEST=1 时 guest 必须处于 IA-32e 模式（即使 UNRESTRICTED_GUEST=1），则需要设置 EFER.LME=1、CR4.PAE=1、并启用 paging。

3. **ENTRY_CTRL 的 LOAD_IA32_EFER 与 guest EFER 冲突（可能性低）**
   - ENTRY_CTRL 包含 LOAD_IA32_EFER=1，但当前 guest EFER=0。
   - ENTRY_CTRL 的 LOAD_IA32_EFER 位导致 VM-entry 时从 guest-state 域加载 IA32_EFER。如果 guest EFER 与 VM-entry 控制域要求的模式不一致，可能被拒绝。

### 6.5 调试期间修改的文件

| 文件 | 修改内容 | 关联问题 |
|------|----------|---------|
| [components/x86_vcpu/src/vmx/vcpu.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vcpu.rs) | 1. 改用 `set_control()` 配置 SEC_CTRL 和 ENTRY_CTRL（#1/#4）；2. 移除手动 VIRTUALIZE_APIC 设置（#2）；3. 添加 `VmcsReadOnlyNW` 导入（#5）；4. Guest CR4 添加 PAE（#6）；5. VIRTUALIZE_X2APIC→VIRTUALIZE_APIC 交叉依赖修复（#7）；6. 新增 VM-entry 失败诊断 dump；7. CR4_GUEST_HOST_MASK 排除 VMXE | #1-#7 |
| [components/x86_vcpu/src/vmx/vmcs.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vmcs.rs) | `set_ept_pointer()` 增加 EPT A/D flags 硬件支持检查（#3） | #3 |

***

## 七、修改文件汇总

| 文件 | 修改内容 |
|------|----------|
| [components/x86_vlapic/src/vlapic.rs](file:///home/phina/Documents/tgoskits/components/x86_vlapic/src/vlapic.rs) | 新增 PendingInitSipi、process_init_sipi、take_pending_init_sipi |
| [components/x86_vlapic/src/lib.rs](file:///home/phina/Documents/tgoskits/components/x86_vlapic/src/lib.rs) | 导出 PendingInitSipi、take_pending_init_sipi 方法 |
| [components/x86_vcpu/src/vmx/vcpu.rs](file:///home/phina/Documents/tgoskits/components/x86_vcpu/src/vmx/vcpu.rs) | run() 中检查 pending INIT/SIPI；inner_run() 区分 BSP/AP 启动 |
| [os/axvisor/src/vmm/vcpus.rs](file:///home/phina/Documents/tgoskits/os/axvisor/src/vmm/vcpus.rs) | CpuUp 处理分支、vcpu_on 函数 |
| [components/acpi_tables/src/lib.rs](file:///home/phina/Documents/tgoskits/components/acpi_tables/src/lib.rs) | MADT LAPIC 条目添加 4字节 Flags 字段 |
| [os/axvisor/configs/vms/uefi-x86_64-qemu.toml](file:///home/phina/Documents/tgoskits/os/axvisor/configs/vms/uefi-x86_64-qemu.toml) | cpu_num=2, phys_cpu_sets=[1,2] |
| [os/axvisor/configs/qemu/qemu-x86_64.toml](file:///home/phina/Documents/tgoskits/os/axvisor/configs/qemu/qemu-x86_64.toml) | -smp 2 |
| [scripts/axbuild/src/axvisor/rootfs.rs](file:///home/phina/Documents/tgoskits/scripts/axbuild/src/axvisor/rootfs.rs) | 动态应用 SMP 参数 |
| [scripts/axbuild/src/test/qemu.rs](file:///home/phina/Documents/tgoskits/scripts/axbuild/src/test/qemu.rs) | apply_smp_qemu_arg 函数 |