# Debug Session: pci-host-bridge-hang

## Status: [OPEN]

## Problem Description
VM 启动后卡住，没有更多输出。PCI Host Bridge 已注册到 I/O ports 0xCF8-0xCFF，但 OVMF 可能无法正确扫描 PCI 总线。

## Hypotheses
1. **H1**: PCI 主桥没有正确响应配置空间读取请求（返回值格式错误）
2. **H2**: OVMF 在等待某个特定的 PCI 设备响应（如 VGA 设备 00:02.0）
3. **H3**: 端口 I/O 地址范围匹配有问题，导致请求没有被正确路由
4. **H4**: PCI 配置空间的 Vendor ID/Device ID 值不正确，OVMF 无法识别
5. **H5**: handle_read 返回的 `0xFFFFFFFF` 被错误处理

## Evidence Collection Plan
- 添加详细的插桩日志，记录所有 PCI 配置空间的读写请求
- 记录 Bus/Device/Function 编号，确认 OVMF 正在扫描哪些设备
- 记录返回值，确认是否正确返回

## Timeline
- [Step 1] 创建调试文件
- [Step 2] 添加插桩日志
- [ ] 用户运行并收集日志
- [ ] 分析日志并确定根因
- [ ] 实施修复
- [ ] 验证修复
- [ ] 清理

## Notes
- 日志级别需要设置为 trace 才能看到 PCI 相关日志