//! Extinction (état S5) et redémarrage de la machine.
//!
//! - Extinction : le paquet `_S5_` de la DSDT donne `SLP_TYPa`/`SLP_TYPb` ; on les écrit dans
//!   `PM1a_CNT`/`PM1b_CNT` avec `SLP_EN` (bit 13), après avoir passé la machine en mode ACPI
//!   (`SCI_EN`) par le port SMI si le microprogramme ne l'a pas fait. Suite de Linux
//!   (`acpi_hw_legacy_sleep`) : `SLP_TYP` d'abord, `SLP_EN` ensuite, sans toucher aux autres bits.
//! - Redémarrage : registre `RESET_REG` de la FADT (s'il est annoncé, espace d'E/S ou
//!   configuration PCI du bus 0), puis le registre de contrôle de réinitialisation 0xCF9
//!   (réinitialisation « chaude » puis « dure »), puis l'impulsion du contrôleur 8042 (0xFE).
//!
//! La DSDT n'est pas interprétée : seul le motif `Name (_S5_, Package { … })` est lu, comme le
//! font les petits noyaux. Les valeurs sont relevées à l'initialisation, hors de tout chemin
//! critique ; `poweroff`/`reboot` ne prennent aucun verrou.

use log::{info, warn};
use spin::Once;

/// Bit `SCI_EN` de `PM1_CNT` : la machine est en mode ACPI.
const SCI_EN: u16 = 1 << 0;
/// Bit `SLP_EN` de `PM1_CNT` : entre dans l'état de veille choisi par `SLP_TYP`.
const SLP_EN: u16 = 1 << 13;
/// Champ `SLP_TYP` de `PM1_CNT` (bits 10 à 12).
const SLP_TYP_MASK: u16 = 0b111 << 10;
/// `RESET_REG_SUP` (bit 10 des drapeaux de la FADT).
const RESET_REG_SUP: u32 = 1 << 10;
/// `HW_REDUCED_ACPI` (bit 20) : pas de `PM1_CNT`, registres de veille à part (non géré).
const HW_REDUCED_ACPI: u32 = 1 << 20;

/// Registre de réinitialisation annoncé par la FADT.
#[derive(Clone, Copy, Debug)]
enum ResetReg {
    /// Port d'E/S.
    Io(u16),
    /// Registre de configuration PCI du bus 0 : (périphérique, fonction, décalage).
    PciConfig(u8, u8, u8),
}

/// Ce qu'il faut pour éteindre et redémarrer, relevé dans la FADT et la DSDT.
#[derive(Clone, Copy, Debug)]
struct PowerInfo {
    pm1a_cnt: u16,
    pm1b_cnt: u16,
    /// `(SLP_TYPa, SLP_TYPb)` de l'état S5, si la DSDT en donne un.
    s5: Option<(u16, u16)>,
    smi_cmd: u16,
    acpi_enable: u8,
    reset: Option<(ResetReg, u8)>,
}

static POWER: Once<PowerInfo> = Once::new();

/// Relève ce qu'il faut dans la FADT et la DSDT (`dsdt` : octets de la table entière).
pub(crate) fn init(fadt: &fadt::Fadt, dsdt: Option<&[u8]>) {
    let flags = fadt.flags;
    let port = |gas: sdt::GenericAddressStructure, legacy: u32| -> u16 {
        let (space, addr) = (gas.address_space, gas.phys_addr);
        if space == 1 && addr != 0 && addr <= u16::MAX as u64 {
            addr as u16
        } else if legacy <= u16::MAX as u32 {
            legacy as u16
        } else {
            0
        }
    };
    let pm1a_cnt = port(fadt.x_pm1a_control_block, fadt.pm1a_control_block);
    let pm1b_cnt = port(fadt.x_pm1b_control_block, fadt.pm1b_control_block);
    let s5 = if flags & HW_REDUCED_ACPI != 0 { None } else { dsdt.and_then(find_s5) };

    let reset = (flags & RESET_REG_SUP != 0)
        .then(|| fadt.reset_reg)
        .and_then(|r| {
            let addr = r.phys_addr;
            match r.address_space {
                1 if addr <= u16::MAX as u64 && addr != 0 => Some(ResetReg::Io(addr as u16)),
                // Configuration PCI : périphérique bits 32–47, fonction 16–31, décalage 0–15.
                2 => Some(ResetReg::PciConfig((addr >> 32) as u8 & 0x1F, (addr >> 16) as u8 & 0x7, addr as u8)),
                _ => None,
            }
        })
        .map(|r| (r, fadt.reset_value));

    let info = PowerInfo {
        pm1a_cnt,
        pm1b_cnt,
        s5,
        smi_cmd: fadt.smi_command_port as u16,
        acpi_enable: fadt.acpi_enable,
        reset,
    };
    info!("ACPI : alimentation {info:?}");
    if info.s5.is_none() {
        warn!("ACPI : pas de paquet _S5_ dans la DSDT, extinction impossible");
    }
    POWER.call_once(|| info);
}

/// Cherche `Name (_S5_, Package (n) { SLP_TYPa, SLP_TYPb, … })` dans le code AML de la DSDT.
///
/// Forme attendue : `NameOp` (0x08), éventuellement `\` (0x5C), `_S5_`, `PackageOp` (0x12),
/// `PkgLength` (1 à 4 octets), `NumElements`, puis deux entiers (`ZeroOp`, `OneOp`, `OnesOp`,
/// `BytePrefix`, `WordPrefix`, `DWordPrefix`, `QWordPrefix`).
pub(crate) fn find_s5(aml: &[u8]) -> Option<(u16, u16)> {
    let body = aml.get(36..)?; // après l'en-tête SDT
    let mut from = 0;
    while let Some(k) = find(&body[from..], b"_S5_") {
        let at = from + k;
        from = at + 4;
        let named = (at >= 1 && body[at - 1] == 0x08) || (at >= 2 && body[at - 1] == 0x5C && body[at - 2] == 0x08);
        if !named {
            continue;
        }
        let mut p = at + 4;
        if body.get(p) != Some(&0x12) {
            continue;
        }
        p += 1;
        let lead = *body.get(p)?;
        p += 1 + (lead >> 6) as usize; // octets supplémentaires de PkgLength
        p += 1; // NumElements
        let (a, n) = integer(body.get(p..)?)?;
        let (b, _) = integer(body.get(p + n..)?)?;
        return Some(((a & 7) as u16, (b & 7) as u16));
    }
    None
}

/// Entier AML en tête de `s` : `(valeur, octets lus)`.
fn integer(s: &[u8]) -> Option<(u64, usize)> {
    let le = |n: usize| -> Option<(u64, usize)> {
        let b = s.get(1..1 + n)?;
        Some((b.iter().rev().fold(0u64, |v, &x| v << 8 | x as u64), 1 + n))
    };
    match *s.first()? {
        0x00 => Some((0, 1)),
        0x01 => Some((1, 1)),
        0xFF => Some((u64::MAX, 1)),
        0x0A => le(1),
        0x0B => le(2),
        0x0C => le(4),
        0x0E => le(8),
        _ => None,
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Éteint la machine (état S5). Ne revient que si l'extinction a échoué.
pub fn poweroff() -> Result<(), &'static str> {
    let info = POWER.get().ok_or("ACPI non initialisé")?;
    let (typ_a, typ_b) = info.s5.ok_or("pas de paquet _S5_ dans la DSDT")?;
    if info.pm1a_cnt == 0 {
        return Err("pas de registre PM1a_CNT");
    }
    warn!("ACPI : extinction de la machine");
    unsafe {
        // Mode ACPI : sans SCI_EN, le matériel ignore SLP_EN.
        if port_io::inw(info.pm1a_cnt) & SCI_EN == 0 && info.smi_cmd != 0 && info.acpi_enable != 0 {
            port_io::outb(info.acpi_enable, info.smi_cmd);
            for _ in 0..3_000_000 {
                if port_io::inw(info.pm1a_cnt) & SCI_EN != 0 {
                    break;
                }
            }
        }
        core::arch::asm!("cli", options(nomem, nostack));
        // Comme Linux : SLP_TYP dans les deux registres, puis SLP_EN.
        let a = port_io::inw(info.pm1a_cnt) & !(SLP_TYP_MASK | SLP_EN) | (typ_a << 10);
        port_io::outw(a, info.pm1a_cnt);
        let b = (info.pm1b_cnt != 0).then(|| {
            let b = port_io::inw(info.pm1b_cnt) & !(SLP_TYP_MASK | SLP_EN) | (typ_b << 10);
            port_io::outw(b, info.pm1b_cnt);
            b
        });
        port_io::outw(a | SLP_EN, info.pm1a_cnt);
        if let Some(b) = b {
            port_io::outw(b | SLP_EN, info.pm1b_cnt);
        }
        // La machine s'éteint après quelques microsecondes ; sinon, l'échec est rendu.
        for _ in 0..3_000_000 {
            core::hint::spin_loop();
            let _ = port_io::inb(0x80);
        }
        core::arch::asm!("sti", options(nomem, nostack));
    }
    Err("la machine ne s'est pas éteinte")
}

/// Redémarre la machine. Ne revient pas.
pub fn reboot() -> ! {
    warn!("ACPI : redémarrage de la machine");
    let delay = || {
        for _ in 0..200_000 {
            unsafe { let _ = port_io::inb(0x80); }
        }
    };
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
        if let Some((reg, value)) = POWER.get().and_then(|i| i.reset) {
            match reg {
                ResetReg::Io(port) => port_io::outb(value, port),
                ResetReg::PciConfig(dev, func, off) => {
                    let addr = 0x8000_0000u32 | (dev as u32) << 11 | (func as u32) << 8 | (off as u32 & 0xFC);
                    port_io::outl(addr, 0xCF8);
                    port_io::outb(value, 0xCFC + (off as u16 & 3));
                }
            }
            delay();
        }
        // Registre de contrôle de réinitialisation (PIIX, ICH, Q35) : chaude, puis dure.
        port_io::outb(0x02, 0xCF9);
        delay();
        port_io::outb(0x06, 0xCF9);
        delay();
        // Contrôleur clavier 8042 : impulsion sur la ligne de réinitialisation.
        for _ in 0..0x10000 {
            if port_io::inb(0x64) & 0x02 == 0 {
                break;
            }
        }
        port_io::outb(0xFE, 0x64);
        delay();
        // Dernier recours : triple faute (IDT vide puis interruption).
        let idt: [u8; 10] = [0; 10];
        core::arch::asm!("lidt [{0}]", "int3", in(reg) idt.as_ptr(), options(nostack));
    }
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}
