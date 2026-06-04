## 

cargo xtask axvisor qemu \
  --config os/axvisor/configs/board/qemu-x86_64.toml \
  --vmconfigs os/axvisor/configs/vms/uefi-x86_64-qemu.toml \
  --qemu-config os/axvisor/configs/qemu/qemu-x86_64-uefi.toml

or

cargo xtask axvisor qemu --config os/axvisor/configs/board/qemu-x86_64.toml

---

已经实现的作业有

/home/phina/Documents/tgoskits/docs/docs/phina/stage-N.md

参考 
/home/phina/Documents/tgoskits/docs/docs/development/20260530.md

开发记录请写在 /home/phina/Documents/tgoskits/docs/docs/phina/stage-N.md

 你可以尝试自己运行验证（编译成功后十几秒就可以，因为虚拟机会卡在一处,有了输出要及时关闭，注意不要在你的沙箱操作(沙箱没有KVM)，直接在沙箱外执行，我的系统支持KVM）
cargo xtask axvisor qemu --config os/axvisor/configs/board/qemu-x86_64.toml>a.txt



