#![no_std]

use log::info;
use time::{Instant, Period};

pub struct Tsc;

impl time::ClockSource for Tsc {
    type ClockType = time::Monotonic;

    fn now() -> Instant {
        Instant::new(tsc_value())
    }
}

/// Returns the frequency of the TSC for the system, currently measured using
/// the PIT clock for calibration.
pub fn get_tsc_period() -> Option<Period> {
    const PIT_WAIT_MICROSECONDS: u32 = 10_000;
    const PIT_WAIT_FEMTOSECONDS: u64 = PIT_WAIT_MICROSECONDS as u64 * 1_000_000_000;

    let start = tsc_value();
    pit_clock_basic::pit_wait(PIT_WAIT_MICROSECONDS).ok()?;
    let end = tsc_value();

    let increments = end.checked_sub(start)?;
    let tsc_period = Period::new(PIT_WAIT_FEMTOSECONDS / increments);

    info!("TSC period calculated by PIT is: {tsc_period}");

    Some(tsc_period)
}

#[doc(hidden)]
pub fn tsc_value() -> u64 {
    let mut _aux = 0;
    // SAFETY: Reading the TSC value is a platform-specific intrinsic that has no
    // side effects or dangerous behavior, and is supported on all modern x86_64
    // hardware.
    unsafe { core::arch::x86_64::__rdtscp(&mut _aux) }
}

/// Synchronisation des TSC des cœurs au démarrage (comme `arch/x86/kernel/tsc_sync.c` de Linux).
///
/// Sous un hyperviseur (ou sur certaines cartes), le TSC de chaque cœur peut partir d'une
/// origine différente : l'horloge monotone du noyau (le TSC) n'est alors plus la même d'un cœur à
/// l'autre, et un minuteur armé sur un cœur peut expirer en avance quand l'interruption est
/// traitée sur un autre (décalage mesuré sous KVM : environ 20 ms). Le vDSO, qui lit le TSC du
/// cœur où tourne le processus, en hérite aussi.
///
/// Mesure par aller-retour, à la manière de NTP : le cœur principal note `t1`, le cœur qui
/// démarre répond avec son `t2`, le principal note `t3` à la réception ; le décalage est
/// `t2 - (t1 + t3) / 2`, pris sur l'échange au plus court aller-retour (le moins perturbé).
/// Correction par `IA32_TSC_ADJUST` (sans toucher au compteur lui-même), sinon par écriture
/// d'`IA32_TSC`.
pub mod sync {
    use core::arch::x86_64::{__cpuid_count, _rdtsc};
    use core::sync::atomic::{fence, AtomicI64, AtomicU32, AtomicU64, Ordering};

    const ROUNDS: u32 = 128;
    const DONE: u32 = u32::MAX;
    const IA32_TSC: u32 = 0x10;
    const IA32_TSC_ADJUST: u32 = 0x3b;

    static SEQ: AtomicU32 = AtomicU32::new(0);
    static T2: AtomicU64 = AtomicU64::new(0);
    static OFFSET: AtomicI64 = AtomicI64::new(0);

    #[inline]
    fn tsc() -> u64 {
        // SAFETY : lecture du TSC ; les barrières empêchent de l'avancer ou de la retarder par
        // rapport aux accès voisins.
        unsafe {
            core::arch::x86_64::_mm_lfence();
            let t = _rdtsc();
            core::arch::x86_64::_mm_lfence();
            t
        }
    }

    fn has_tsc_adjust() -> bool {
        // SAFETY : CPUID est toujours disponible en mode long.
        unsafe { core::arch::x86_64::__cpuid(0).eax >= 7 && __cpuid_count(7, 0).ebx & (1 << 1) != 0 }
    }

    fn rdmsr(msr: u32) -> u64 {
        let (lo, hi): (u32, u32);
        // SAFETY : MSR architectural, lu en anneau 0.
        unsafe { core::arch::asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack)) };
        ((hi as u64) << 32) | lo as u64
    }

    unsafe fn wrmsr(msr: u32, v: u64) {
        core::arch::asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32, options(nostack));
    }

    /// Côté cœur principal : mesure le décalage du cœur qui exécute [`ap_sync`] en même temps,
    /// le lui transmet, et le renvoie (ticks, positif si le cœur secondaire est en avance), avec
    /// l'aller-retour minimal.
    pub fn bsp_measure() -> (i64, u64) {
        let mut best = (0i64, u64::MAX);
        for r in 0..ROUNDS {
            let t1 = tsc();
            SEQ.store(2 * r + 1, Ordering::Release);
            while SEQ.load(Ordering::Acquire) != 2 * r + 2 {
                core::hint::spin_loop();
            }
            let t3 = tsc();
            let t2 = T2.load(Ordering::Acquire);
            let rtt = t3.wrapping_sub(t1);
            if rtt < best.1 {
                let mid = t1 as i128 + (rtt / 2) as i128;
                best = ((t2 as i128 - mid) as i64, rtt);
            }
        }
        OFFSET.store(best.0, Ordering::Release);
        fence(Ordering::SeqCst);
        SEQ.store(DONE, Ordering::Release);
        best
    }

    /// Côté cœur qui démarre (interruptions coupées) : répond aux mesures, puis corrige son TSC.
    pub fn ap_sync() {
        for r in 0..ROUNDS {
            while SEQ.load(Ordering::Acquire) != 2 * r + 1 {
                core::hint::spin_loop();
            }
            T2.store(tsc(), Ordering::Release);
            SEQ.store(2 * r + 2, Ordering::Release);
        }
        while SEQ.load(Ordering::Acquire) != DONE {
            core::hint::spin_loop();
        }
        let off = OFFSET.load(Ordering::Acquire);
        // En dessous de quelques centaines de cycles, c'est le bruit de la mesure.
        if off.unsigned_abs() < 512 {
            return;
        }
        // SAFETY : MSR architecturaux du TSC, écrits en anneau 0 sur ce cœur.
        unsafe {
            if has_tsc_adjust() {
                let adj = rdmsr(IA32_TSC_ADJUST) as i64;
                wrmsr(IA32_TSC_ADJUST, adj.wrapping_sub(off) as u64);
            } else {
                wrmsr(IA32_TSC, (tsc() as i64).wrapping_sub(off) as u64);
            }
        }
    }
}
