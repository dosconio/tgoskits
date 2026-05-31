/// Boot mode for x86_64 guest VMs.
///
/// Determines the initial vCPU state when the VM is started.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum X86BootMode {
    /// Traditional BIOS/trampoline boot: vCPU starts in real mode at a low address.
    #[default]
    Trampoline,
    /// UEFI boot: vCPU starts in real mode at the x86 reset vector (0xFFFFFFF0),
    /// with EPT mapping the reset vector to the pflash0 firmware region.
    Uefi,
}

/// Configuration for setting up an x86_64 VCpu.
#[derive(Debug, Default, Clone, Copy)]
pub struct X86VCpuSetupConfig {
    /// Boot mode for the guest VM.
    pub boot_mode: X86BootMode,
    /// Size of guest RAM in bytes.
    pub ram_size: usize,
}
