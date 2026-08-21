pub const DENORMAL_OFFSET: f32 = 1e-15;

#[inline(always)]
pub fn flush_denormal(sample: f32) -> f32 {
    let bits = sample.to_bits();
    let is_subnormal_or_zero = (bits & 0x7F80_0000) == 0;
    let mask = (is_subnormal_or_zero as u32).wrapping_sub(1);
    f32::from_bits(bits & mask)
}

/// Flush denormals for f64 samples.
///
/// Subnormal f64 values (exp field == 0) can stall the FPU on many CPUs.
/// This helper zeroes them in a branchless way, mirroring the f32 version.
#[inline(always)]
pub fn flush_denormal_f64(sample: f64) -> f64 {
    let bits = sample.to_bits();
    // Exponent occupies bits 52..62 (11 bits). If all zero, the value is
    // subnormal (or zero) and should be flushed.
    let is_subnormal_or_zero = (bits & 0x7FF0_0000_0000_0000) == 0;
    let mask = (is_subnormal_or_zero as u64).wrapping_sub(1);
    f64::from_bits(bits & mask)
}

#[inline]
pub fn enable_flush_zero_denormals_on_current_thread() -> bool {
    #[cfg(not(debug_assertions))]
    {
        #[cfg(all(target_arch = "x86_64", target_feature = "sse"))]
        {
            unsafe {
                let mut mxcsr: u32 = 0;
                core::arch::asm!(
                    "stmxcsr [{addr}]",
                    addr = in(reg) &mxcsr,
                    options(nostack),
                );
                mxcsr |= 0x8040;
                core::arch::asm!(
                    "ldmxcsr [{addr}]",
                    addr = in(reg) &mxcsr,
                    options(nostack),
                );
            }
            true
        }
        #[cfg(all(target_arch = "x86", target_feature = "sse"))]
        {
            unsafe {
                let mut mxcsr: u32 = 0;
                core::arch::asm!(
                    "stmxcsr [{addr}]",
                    addr = in(reg) &mxcsr,
                    options(nostack),
                );
                mxcsr |= 0x8040;
                core::arch::asm!(
                    "ldmxcsr [{addr}]",
                    addr = in(reg) &mxcsr,
                    options(nostack),
                );
            }
            true
        }
        #[cfg(target_arch = "aarch64")]
        {
            unsafe {
                let fpcr: u64;
                core::arch::asm!("mrs {0}, fpcr", out(reg) fpcr);
                let new_fpcr = fpcr | (1u64 << 24);
                core::arch::asm!("msr fpcr, {0}", in(reg) new_fpcr);
            }
            true
        }
        #[cfg(not(any(
            all(target_arch = "x86_64", target_feature = "sse"),
            all(target_arch = "x86", target_feature = "sse"),
            target_arch = "aarch64"
        )))]
        {
            false
        }
    }
    #[cfg(debug_assertions)]
    {
        false
    }
}
