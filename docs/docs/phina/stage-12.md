# Stage 12: 代码结构整理

> 本文档从 `summary.md` 移入，包含 dev-uefix64 分支的冗余代码评估、代码结构整理计划和执行记录。

## 三、冗余代码与文件评估

### 3.1 仓库根目录垃圾文件（已删除）

以下文件均为调试过程中的临时产物，**不应进入仓库**：

| 文件 | 行数 | 性质 | 处置 |
|------|------|------|------|
| `a.txt` | 265 | 调试日志 | ✅ 已删除 |
| `issue.txt` | 276 | 调试日志 | ✅ 已删除 |
| `debug-pci-host-bridge-hang.md` | 30 | 调试笔记 | ✅ 已删除 |
| `qemu-x86_64` | 8 | 无扩展名 board 配置散落 | ✅ 已删除 |
| `qemu-x86_64-uefi` | 9 | 无扩展名 board 配置散落 | ✅ 已删除 |
| `uefi-x86_64-qemu` | 10 | 无扩展名 board 配置散落 | ✅ 已删除 |
| `x86-qemu-q35` | 9 | 无扩展名 board 配置散落 | ✅ 已删除 |
| `run_test.sh` | 14 | 临时测试脚本 | ✅ 已删除 |

### 3.2 重复配置文件（已删除）

`dev-uefix64` 引入了 **3 份重复**的 VM/QEMU 配置，正式版本在 `os/axvisor/configs/` 和 `os/axvisor/vms/`：

| 重复文件 | 正式文件 | 处置 |
|---------|---------|------|
| `configs/qemu/qemu-x86_64.toml` | `os/axvisor/configs/qemu/qemu-x86_64.toml` | ✅ 已删除 |
| `configs/vms/uefi-x86_64-qemu.toml` | `os/axvisor/vms/uefi-x86_64-qemu.toml` | ✅ 已删除 |
| `vms/uefi-x86_64-qemu.toml` | `os/axvisor/vms/uefi-x86_64-qemu.toml` | ✅ 已删除 |
| `os/axvisor/configs/board/qemu-x86_64.tomal` | `os/axvisor/configs/board/qemu-x86_64.toml`（`.tomal` 是拼写错误） | ✅ 已删除 |

### 3.3 散落的开发文档（已归档）

`docs/docs/development/` 下有 4 份 UEFI 开发文档，与 `docs/docs/phina/stage-*.md` 内容高度重叠，已归档到 `docs/docs/phina/archive/`：

| 文件 | 行数 | 与 phina/stage-*.md 关系 | 处置 |
|------|------|------------------------|------|
| `20260527.md` | 115 | 早期分析，已被 stage-1/2 覆盖 | ✅ 已归档 |
| `20260530.md` | 455 | 早期分析，已被 stage-3/4 覆盖 | ✅ 已归档 |
| `axvisor-x86_64-uefi-analysis.md` | 937 | UEFI 架构分析，部分被 stage-* 覆盖 | ✅ 已归档 |
| `axvisor-x86_64-uefi-week1.md` | 1346 | 周报，已被 stage-* 覆盖 | ✅ 已归档 |

### 3.4 代码冗余评估

#### 3.4.1 `dev` 分支无重复实现

经 `git diff dev..dev-uefix64` 验证，9 个新组件 crate（`acpi_tables`、`fw_cfg`、`i8254_pit`、`i8259_pic`、`mc146818_cmos`、`pci_host`、`pm_timer`、`virtio_blk_pci`、`x86_vioapic`）在 `dev` 分支中**完全不存在**，不存在"dev 已实现 + dev-uefix64 重复实现"的情况。所有组件均为净新增。

#### 3.4.2 `x86_vcpu/vmx/vcpu.rs` 膨胀（1602 → 4542 行，+184%）

单文件 4542 行，混合了：VMCS 设置、VM-entry/exit 处理、CPUID 模拟、CR/MSR 拦截、EPT violation、中断注入、SMP INIT/SIPI、VM-entry 失败诊断 dump、防御性 workaround。**已拆分**（见第五节执行记录）。

#### 3.4.3 `os/axvisor/src/vmm/images/mod.rs` 膨胀（577 → 1543 行，+167%）

单文件 1543 行，混合了：BIOS 镜像加载、UEFI pflash 加载、fw_cfg 设置、ACPI 表构建入口、virtio-blk 创建、全部设备注册、VmGuestMemoryAccessor。**建议拆分**（暂缓）。

#### 3.4.4 S11 防御性 workaround（待评估移除）

`diag.rs` 中的三段 workaround（CS Unusable / IA32_PAT / 段 AR 保留位）在 S11 中明确标注"非根因，保留作安全网"。由于 CR3 NOFLUSH 修复后 VM-entry 0 失败，这些 workaround 从未被触发。**建议**：保留 CS Unusable 清理（SDM 合规），移除 PAT 和段 AR workaround（增加噪音、掩盖真实问题）。

#### 3.4.5 S9 被 S10 推翻的 CPUID 子叶过滤（已自然消失）

S9 给 leaf 0x1 加的 `ecx>0` 返回全零，S10 已移除。当前代码无此冗余。

---

## 四、代码结构整理计划

### 4.1 整理原则

1. **不改变运行时行为**：整理后 `cargo xtask clippy` 全通过，UEFI 启动流程不变。
2. **先删垃圾，再拆文件，最后评估 workaround**：分阶段进行，每阶段验证编译。
3. **保留所有 stage-*.md 文档**：这些是历史记录，不删除。

### 4.2 阶段 1：删除垃圾文件与重复配置（低风险）

**操作**：
- 删除根目录 8 个垃圾文件（§3.1）。
- 删除 3 个重复配置 + 1 个 typo 文件（§3.2）。
- 将 `docs/docs/development/` 下 4 份散落文档移动到 `docs/docs/phina/archive/`（保留历史，不混入主目录）。

**验证**：`cargo xtask clippy --package axvisor` 通过。

### 4.3 阶段 2：拆分 `vcpu.rs`（中风险）

将 `components/x86_vcpu/src/vmx/vcpu.rs`（4542 行）按职责拆分为模块：

```
components/x86_vcpu/src/vmx/
├── vcpu.rs          # VmxVcpu 结构体定义、run()、setup()、reset()
├── vmcs_setup.rs    # VMCS 控制域设置、set_control、CR0/CR4/EFER 初始化
├── exit_handler.rs  # VM-exit reason 分发、各 reason handler
├── cpuid.rs         # CPUID 模拟（handle_cpuid + 各 leaf 分支）
├── cr_msr.rs        # CR/MSR 拦截处理（handle_cr、handle_msr、set_cr）
├── ept.rs           # EPT violation 处理、MMIO 转发
├── interrupt.rs     # 中断注入、inject_pending_events、EOI
├── smp.rs           # SMP INIT/SIPI、CpuUp
└── diag.rs          # VM-entry 失败诊断 dump、防御性 workaround
```

**验证**：`cargo xtask clippy --package x86_vcpu` 全 feature 通过。

### 4.4 阶段 3：拆分 `images/mod.rs`（中风险）

将 `os/axvisor/src/vmm/images/mod.rs`（1543 行）按加载模式拆分：

```
os/axvisor/src/vmm/images/
├── mod.rs           # 公共接口、load_vm_image 入口
├── bios.rs          # trampoline/BIOS 模式加载
├── uefi.rs          # UEFI 模式：pflash 加载、reset vector 设置
├── fw_cfg_acpi.rs   # fw_cfg 设备创建、ACPI 表构建入口、E820
├── devices.rs       # virtio-blk-pci 创建、PCI Host Bridge、全部设备注册
└── guest_mem.rs     # VmGuestMemoryAccessor 实现
```

**验证**：`cargo xtask clippy --package axvisor` 通过。

### 4.5 阶段 4：评估移除 S11 防御性 workaround（低风险）

在阶段 2 拆分后，`diag.rs` 中保留：
- ✅ 保留：VM-entry 失败诊断 dump（调试价值高）。
- ✅ 保留：CS Unusable bit 清理（SDM 合规，CS 必须 usable）。
- ⚠️ 评估移除：IA32_PAT 无效项修复——如果 guest 从未写入无效 PAT，移除后无影响；若移除后 VM-entry 失败，则恢复。
- ⚠️ 评估移除：段 AR 保留位清理——同上。

**验证**：移除后 `cargo xtask axvisor qemu` 启动测试，确认 0 次 VM-entry 失败。

### 4.6 阶段 5：清理调试日志（低风险）

搜索并降级/移除以下调试日志：
- DSDT AML 字节 dump（`acpi_tables/src/lib.rs`）。
- vCPU 重入检测日志（`vcpu.rs`）。
- VMCS-DUMP 日志（`diag.rs`）——已注释掉。
- IO-CODE / IO-fw_cfg / IO-ALL / EPT_VIOLATION 日志（`vcpu.rs`）——已注释掉。
- `log = "Debug"` 配置降级为 `"Info"`（仅保留 UEFI 启动关键里程碑日志）。

---

## 五、整理执行记录

### 阶段 1 执行记录

**状态**：✅ 完成（2026-06-21）

**操作**：
- 删除根目录 8 个垃圾文件：`a.txt`、`issue.txt`、`debug-pci-host-bridge-hang.md`、`qemu-x86_64`、`qemu-x86_64-uefi`、`uefi-x86_64-qemu`、`x86-qemu-q35`、`run_test.sh`。
- 删除 3 个重复配置：`configs/qemu/qemu-x86_64.toml`、`configs/vms/uefi-x86_64-qemu.toml`、`vms/uefi-x86_64-qemu.toml`。
- 删除 1 个 typo 文件：`os/axvisor/configs/board/qemu-x86_64.tomal`。
- 删除空目录：`configs/qemu/`、`configs/vms/`、`configs/`、`vms/`。
- 将 4 份散落开发文档归档到 `docs/docs/phina/archive/`：`20260527.md`、`20260530.md`、`axvisor-x86_64-uefi-analysis.md`、`axvisor-x86_64-uefi-week1.md`。

**验证**：`cargo xtask clippy --package x86_vcpu --package acpi_tables --package virtio_blk_pci --package x86_vioapic --package x86_vlapic --package pci_host` → 6 package(s), 9 check(s), 全部通过。

### 阶段 2 执行记录

**状态**：✅ 完成（2026-06-21）

**操作**：将 `components/x86_vcpu/src/vmx/vcpu.rs` 从 4542 行拆分为 9 个模块。

| 模块文件 | 行数 | 职责 |
|---------|------|------|
| `vcpu.rs` | 1633 | VmxVcpu 结构体定义、run()/setup()/reset()、寄存器访问、vmx_launch/resume/exit、Drop/Debug impl、AxArchVCpu impl |
| `vmcs_setup.rs` | 950 | VMCS 控制域设置、setup_io_bitmap/msr_bitmap/vmcs、fixup_ia32e_guest_cr_and_efer |
| `ept.rs` | 532 | EPT violation 处理、MMIO 转发、GVA→GPA→HPA 翻译、指令解码 |
| `cr_msr.rs` | 406 | CR/MSR 拦截、set_cr、handle_cr、APIC MSR access、TSC deadline |
| `diag.rs` | 391 | VM-entry 失败诊断 dump、CS/PAT/段 AR 防御性 workaround |
| `exit_handler.rs` | 377 | VM-exit reason 分发、NMI_WINDOW/VMX_PREEMPTION_TIMER/HLT/XSETBV handler |
| `cpuid.rs` | 297 | CPUID 模拟（leaf 0x1/0x4/0xB/0x1F/0x80000008 等） |
| `interrupt.rs` | 141 | inject_pending_events、中断注入链 |
| `smp.rs` | 44 | take_smp_init_sipi |

**可见性修复**（3 处）：
- `cpuid.rs` 添加 `use bit_field::BitField;`（`set_bit` 方法）
- `PendingEvent` 三个字段改为 `pub(super)`（interrupt.rs 访问）
- `vmx_exit` 改为 `pub(super) unsafe extern "C" fn`（vmcs_setup.rs 取函数指针）

**验证**：`cargo xtask clippy --package x86_vcpu` → 4 feature（base/svm/tracing/vmx）全通过，0 失败。`cargo fmt --package x86_vcpu` 通过。

### 阶段 3 执行记录

**状态**：⏸ 暂缓（用户决定本轮只拆 vcpu.rs，images/mod.rs 留待后续）

### 阶段 4 执行记录

**状态**：⏸ 暂缓（需 QEMU 运行时验证，本轮不执行）

### 阶段 5 执行记录

**状态**：✅ 部分完成（2026-06-21）

**操作**：注释掉以下过多日志：
- `diag.rs` `dump_vmcs_state()` 全部 ~25 条 `debug!` 调用（VMCS-DUMP 满屏问题的主要源头）。
- `vcpu.rs` IO-CODE 代码 dump 块（`info!` + 周围变量计算）。
- `vcpu.rs` IO-fw_cfg 端口访问日志（`debug!`）。
- `vcpu.rs` IO-ALL 非 PM-TIMER 端口访问日志（`debug!`）。
- `vcpu.rs` EPT_VIOLATION 日志（`info!`）。

**验证**：`cargo xtask clippy --package x86_vcpu` → 4 feature 全通过。`cargo fmt` 通过。
