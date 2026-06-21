# dev-uefix64 阶段总结与代码结构整理

本文档汇总 `docs/docs/phina/stage-*.md`（Stage 1 ~ Stage 12）的全部工作，**交叉验证**每个问题的真实解决状态（不盲信单个 stage 文档的"已解决"声明），并列出 `dev-uefix64` 分支相对 `dev` 分支的净增功能、冗余代码评估、以及代码结构整理计划。

> 分支基线：`dev`（`5b41966df`）→ `dev-uefix64`（`1984171f8`），共 12 个 commit（s1, s2, stage3, stage4, s5, s6, s8, s8+, s9, s10, s11, s12）。
> 净变更：113 文件，+20346 / −723 行；其中 9 个全新组件 crate，`x86_vcpu/vmx/vcpu.rs` 1602 → 4542 行。

---

## 一、问题与解决状态交叉验证表

下表按时间顺序列出每个 stage 声明解决的问题，并标注**后续 stage 是否复现/推翻**。状态含义：

- ✅ 真正解决：后续 stage 未再复现。
- ⚠️ 部分解决：声明解决但后续发现新侧问题，或仅编译通过、运行时未验证。
- ❌ 未解决：该 stage 结束时仍未解决（被后续 stage 解决或继续遗留）。
- 🔁 回归/推翻：后续 stage 推翻了"已解决"结论。

| # | Stage 声明 | 声明状态 | 交叉验证后真实状态 | 证据 |
|---|-----------|---------|-------------------|------|
| 1 | S1: 双启动模式（trampoline/uefi）+ pflash 配置 + vCPU UEFI 初始状态 | ✅ | ✅ | 后续 stage 未再质疑启动模式配置 |
| 2 | S1: vCPU reset vector RIP=0xFFFFFFF0 | ✅ | 🔁 **被 S10 推翻** | S10 §1f 指出 RIP 应为段内偏移 0xFFF0（CS.base=0xFFFF0000），0xFFFFFFF0 是线性地址；S10 修正为 0xFFF0 |
| 3 | S2: fw_cfg 设备 + ACPI 静态表（RSDP/XSDT/FADT/MADT/MCFG） | ✅ | ⚠️ **ACPI 表后续多次返工** | S7 给 MADT LAPIC 条目补 4 字节 Flags；S12 重写 DSDT（PkgLength/EISA ID/_CRS 三处 bug） |
| 4 | S2: 动态设备注册 + string I/O 处理 | ✅ | ✅ | 后续 stage 沿用 |
| 5 | S3: PCI 设备模拟（virtio-blk-pci）+ legacy I/O BAR + intercept_all I/O bitmap | ✅ | ⚠️ **virtio-blk 仅空壳** | S3 的 virtio-blk 无实际 I/O 能力；S8 重写为完整 VirtQueue I/O |
| 6 | S3: APIC timer mask 修复 | ✅ | ✅ | — |
| 7 | S4: vIOAPIC + EPT_VIOLATION→MMIO 转发 + 中断注入链 + GLOBAL_VIOAPIC 单例 | ✅ | ⚠️ **运行时未验证** | S4 自述"⏸ 被 CPUID 阻塞"，IOAPIC MMIO 从未端到端测试；直到 S10 VM-entry 成功后才真正运行 |
| 8 | S5: CPUID subleaf 修复（0x1/0x4/0xB/0x1F/0x80000008） | ✅ | 🔁 **被 S9/S10 修正** | S9 给 leaf 0x1 加 `ecx>0` 子叶过滤；S10 又**移除**该过滤（leaf 0x1 不使用 subleaf，ECX 应被忽略）——S9 的修复被 S10 推翻 |
| 9 | S5: fw_cfg 大端修复 + E820 表 + EXCEPTION_NMI 处理 | ✅ | ✅ | — |
| 10 | S5: Linux 内核启动 | ✅ | ❌ **未真正启动** | S5 声明 Linux 启动，但 S6/S7/S8 均显示 OVMF 仍卡在早期阶段；真正 Linux 启动在 S11 |
| 11 | S6: i8259 PIC 模拟 | ✅ | ✅ | — |
| 12 | S6: EPT violation #PF 注入（out-of-RAM 地址） | ✅ | 🔁 **被 S10 推翻** | S10 §5 指出：guest 未启用分页时注入 #PF 无法处理，导致 OVMF #PF 死循环；S10 改为扩展 RAM 映射范围 |
| 13 | S7: SMP INIT/SIPI + vLAPIC ICR + CpuUp exit reason | ✅ | ⚠️ **基础设施就绪但端到端未验证** | S7 自述"OVMF EPT_VIOLATION 死循环"阻塞测试；S11 实测 `smpboot: SMP disabled`，仅 1 CPU |
| 14 | S7: VMCS 控制域调试（8 轮迭代） | ⚠️ 问题 8 未解决 | ❌ **未解决** | S7 §6.2 快照显示 VM-entry 仍失败；S8 §"阻塞问题"确认"S7 遗留问题"；S9 确认"Stage 7/8 遗留"；**S10 才真正解决**（6 个子修复） |
| 15 | S8: VirtQueue 完整 I/O + GuestMemoryAccessor + BlockBackend + MemDisk | ✅ | ⚠️ **编译通过但运行时被阻塞** | S8 自述"VM-entry 失败阻塞"，virtio-blk I/O 从未在 OVMF 中触发；S12 才看到 `virtio_blk virtio0: [vda] 131072 512-byte` |
| 16 | S8: MemDisk 64MiB 预分配 OOM → BTreeMap 稀疏分配 | ✅ | ✅ | — |
| 17 | S9: vLAPIC EOI 广播（替换 `unimplemented!()`） | ✅ | ⚠️ **运行时未验证** | S9 自述"VM-entry 失败阻塞，无法运行时验证"；逻辑正确性靠语义对齐推断 |
| 18 | S9: CPUID leaf 0x1 子叶过滤（`ecx>0` 返回全零） | ✅ | 🔁 **被 S10 推翻** | S10 §3 移除此过滤，因 leaf 0x1 不使用 subleaf |
| 19 | S10: VM-entry 成功（6 个子修复：mandatory1/CR0 时序/CR0_MASK/TR AR/NMI_WINDOW/RIP） | ✅ | ✅ | S11 确认 0 次 VM-entry 失败 |
| 20 | S10: CR3 读写支持 | ✅ | ✅ | — |
| 21 | S10: PCI MMIO 范围扩展 0xC0000000 → 0xFEC00000 | ✅ | ✅ | — |
| 22 | S10: OVMF #PF（内存映射不足） | ❌ | ❌ **S10 未解决** | S10 自述"当前状态：OVMF 已成功启动但因 VM 内存映射不足导致 #PF"；**S11 通过扩展 RAM + CR3 NOFLUSH 修复间接解决** |
| 23 | S11: CR3 NOFLUSH bit 63 屏蔽（根因修复） | ✅ | ✅ | S11 确认 0 次 VM-entry 失败，Linux 完整启动 |
| 24 | S11: 防御性 workaround（CS Unusable / IA32_PAT / 段 AR 保留位） | ✅ 保留作安全网 | ⚠️ **非根因，待评估移除** | S11 自述"非根因，修复后 #1 仍然失败，直到 CR3 NOFLUSH 修复才彻底解决" |
| 25 | S12: AML PkgLength 4-bit 掩码修复 | ✅ | ✅ | — |
| 26 | S12: EISA ID 大端字节序修复 | ✅ | ✅ | — |
| 27 | S12: _CRS 资源描述符类型码修复（0x86→0x88, 0x85→0x87） | ✅ | ✅ | S12 确认 `ACPI: PCI Root Bridge [PCI0]` + _CRS 解析成功 |
| 28 | S12: 未知 MSR panic → 返回 0/Ok + advance_rip(2) | ✅ | ✅ | S12 确认 Linux 启动到 NFS/keyring 阶段 |

---

## 二、dev-uefix64 实现的功能清单

**一句话总结**：实现了 UEFI x64 启动（OVMF 固件加载）、fw_cfg 设备、ACPI 表（RSDP/XSDT/FADT/MADT/MCFG/DSDT）、PCI Host Bridge、virtio-blk-pci（完整 VirtQueue I/O）、vIOAPIC、vLAPIC EOI 广播、i8259 PIC、i8254 PIT、PM Timer、RTC、VMX VM-entry 合规化、CPUID 模拟、EPT violation 处理、MSR exit 处理、SMP INIT/SIPI 基础设施、CR3 NOFLUSH 修复。

以下为 `dev-uefix64` 相对 `dev` 的**净增**功能（`dev` 分支无这些组件/功能）：

### A. 启动与固件支持

1. **UEFI 启动模式**：`boot_mode = "uefi"` 配置，支持 OVMF_CODE.fd / OVMF_VARS.fd 加载到 pflash GPA 区域（[axvmconfig/src/lib.rs](file:///home/phina/Documents/tgoskits/components/axvmconfig/src/lib.rs)）。
2. **vCPU UEFI 初始状态**：reset vector 0xFFFFFFF0（线性地址），CS.base=0xFFFF0000，RIP=0xFFF0（段内偏移，S10 修正）。
3. **pflash 别名映射**：pflash0 顶部 64KB 别名到 GPA 0xF0000，兼容实模式 reset vector 访问。
4. **CR3 读写拦截**：支持 guest MOV to/from CR3（S10），写入时屏蔽 bit 63（NOFLUSH，S11 根因修复）。

### B. 固件配置与 ACPI 表（新组件 `acpi_tables`）

5. **fw_cfg 设备**（新组件 `fw_cfg`）：QEMU fw_cfg 接口模拟，支持通过 fw_cfg 传递内核镜像、内核命令行、E820 表、ACPI 表、initrd。
6. **ACPI 静态表构建**（新组件 `acpi_tables`）：RSDP、XSDT、FADT、MADT（含 LAPIC + IOAPIC + LAPIC Flags）、MCFG。
7. **ACPI DSDT 动态构建**：PCI Root Bridge (_HID=PNP0A08, _CID=PNP0A03)、_CRS 资源声明（WordBusNumber/WordIO/DWordMemory）、_PRT 中断路由表（S12 修复 PkgLength/EISA ID/_CRS 三处 bug）。
8. **E820 内存映射表**：通过 fw_cfg 传递给 Linux，声明 RAM + device reserved 区域。

### C. PCI 与块设备（新组件 `pci_host` + `virtio_blk_pci`）

9. **PCI Host Bridge 模拟**（新组件 `pci_host`）：配置空间 0xCF8/0xCFC 访问，多设备枚举，BAR 动态分配。
10. **virtio-blk-pci 设备**（新组件 `virtio_blk_pci`）：Legacy 模式 PCI 设备，完整 VirtQueue I/O（descriptor chain 遍历、available ring 读取、used ring 写回、ISR status 中断）。
11. **GuestMemoryAccessor trait**：GPA→HVA 翻译抽象，避免 `virtio_blk_pci` 直接依赖 `axvm`。
12. **BlockBackend trait + MemDisk**：稀疏磁盘后端（BTreeMap<u64, [u8;512]>），避免 64MiB 预分配 OOM。

### D. 中断控制器（新组件 `x86_vioapic` + 修改 `x86_vlapic`）

13. **vIOAPIC 模拟**（新组件 `x86_vioapic`）：24 个 RTE，level/edge 触发，Remote IRR 管理，`raise_irq()` + `eoi()` + `take_pending_irqs()`，`GLOBAL_VIOAPIC` 全局单例。
14. **vLAPIC EOI 广播**：`process_eoi()` 替换 `unimplemented!()`，通过 `GLOBAL_VIOAPIC.eoi()` 清除 Remote IRR（S9）。
15. **vLAPIC ICR 处理 + SMP INIT/SIPI**：`PendingInitSipi` 机制，`process_init_sipi()`，`take_pending_init_sipi()`，`CpuUp` exit reason（S7，基础设施就绪但运行时 SMP 未激活）。
16. **i8259 PIC 模拟**（新组件 `i8259_pic`）：主从 8259，ICW1-4/OCW1-3，IRR/ISR/IMR，虚拟 wire mode。
17. **i8254 PIT 模拟**（新组件 `i8254_pit`）：3 通道定时器，IRQ 0 时钟中断源。
18. **PM Timer 模拟**（新组件 `pm_timer`）：ACPI PM Timer（I/O 0x600-0x60B），32 位递减计数器。
19. **mc146818 RTC 模拟**（新组件 `mc146818_cmos`）：CMOS RTC，日期时间寄存器。

### E. VMX/vCPU 扩展（修改 `x86_vcpu`）

20. **VM-entry 控制域合规化**（S10，6 个子修复）：`set_control` mandatory1 计算、CR0 初始值时序、CR0_GUEST_HOST_MASK 排除 PE/PG（UNRESTRICTED_GUEST）、TR AR usable、NMI_WINDOW no-op、RIP 段内偏移。
21. **CPUID 模拟**：leaf 0x1（隐藏 VMX/TSC_DEADLINE/MONITOR，设置 HYPERVISOR）、0x4（缓存参数 subleaf）、0xB/0x1F（拓扑 subleaf）、0x80000008（物理地址位数）。
22. **EPT violation 处理**：RAM 范围外地址映射扩展（0x80000000-0xFEC00000 PCI MMIO），PCI BAR 动态映射。
23. **MSR exit 处理**：RDMSR/WRMSR exit 传播给 VMM 时 `advance_rip(2)`，未知 MSR 返回 0/Ok（S12）。
24. **VM-entry 失败诊断 dump**：CR3/FS_BASE/GS_BASE/IA32_PAT/PERF_GLOBAL_CTRL/PDPTE/TR/LDTR 全字段 dump（S11）。
25. **防御性 workaround**（S11，待评估移除）：CS Unusable bit 清理、IA32_PAT 无效项修复、段 AR 保留位清理。

### F. Axvisor VMM 扩展（修改 `os/axvisor`）

26. **UEFI 镜像加载流程**（`vmm/images/mod.rs` +966 行）：pflash0/pflash1 加载、fw_cfg + ACPI 表设置、virtio-blk-pci 创建、PCI Host Bridge 注册、全部虚拟设备注册链。
27. **VmGuestMemoryAccessor**：封装 `read_from_guest_of::<u8>()` / `write_to_guest_of::<u8>()`，替代 `.expect()` panic。
28. **vCPU 运行循环扩展**（`vmm/vcpus.rs`）：CpuUp 处理分支、`vcpu_on()` 函数、VM-entry 失败重试逻辑。
29. **SMP 配置支持**：`cpu_num`、`phys_cpu_sets` 配置项，`-smp` QEMU 参数动态应用。

### G. 配置文件

30. **UEFI VM 配置**：`os/axvisor/vms/uefi-x86_64-qemu.toml`（memory_regions、pflash0/1、cpu_num、kernel_path）。
31. **UEFI QEMU 配置**：`os/axvisor/configs/qemu/qemu-x86_64-uefi.toml`（pflash 参数、OVMF 路径）。
32. **board 配置扩展**：`os/axvisor/configs/board/qemu-x86_64.toml` 增加 `page-alloc-64g` feature。

---

> **冗余代码评估、代码结构整理计划和执行记录已移至 [stage-12.md](file:///home/phina/Documents/tgoskits/docs/docs/phina/stage-12.md)。**

---

## 三、当前阻塞与后续工作

| 项目 | 状态 | 说明 |
|------|------|------|
| Linux 启动到 init/用户空间 | ⏳ 进行中 | S12 已挂载 VFS，需增加测试时长观察 /init 执行 |
| SMP 多 CPU | ❌ 未工作 | S7 基础设施就绪，S11 实测 `SMP disabled`，需调试 AP 唤醒 |
| FADT GAS Register Bit Width 警告 | ⚠️ 待修复 | `sdt.rs` 的 `append_gas` 设置 Register Bit Width 为 0 |
| PMU 事件不可用警告 | ⚠️ 非致命 | CPUID 模拟未报告完整 PMU 事件，仅影响 perf 子系统 |
| 代码结构整理 | ⏳ 部分完成 | 阶段1（删垃圾）✅、阶段2（拆 vcpu.rs）✅、阶段5（注释日志）✅；阶段3（拆 images/mod.rs）⏸暂缓。详见 [stage-12.md](file:///home/phina/Documents/tgoskits/docs/docs/phina/stage-12.md) |
