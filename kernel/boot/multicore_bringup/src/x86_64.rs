use core::{
    arch::asm,
    convert::TryInto,
    mem::size_of,
    ops::DerefMut,
    sync::atomic::{AtomicU64, Ordering},
};
use spin::Mutex;
use volatile::Volatile;
use zerocopy::FromBytes;
use memory::{VirtualAddress, PhysicalAddress, MappedPages, PteFlags, MmiRef};
use kernel_config::{memory::{PAGE_SIZE, PAGE_SHIFT, KERNEL_STACK_SIZE_IN_PAGES}, display::FRAMEBUFFER_MAX_RESOLUTION};
use apic::{LocalApic, get_lapics, current_cpu, has_x2apic, bootstrap_cpu, cpu_count};
use ap_start::{kstart_ap, AP_READY_FLAG};
use madt::{Madt, MadtEntry, find_nmi_entry_for_processor};
use core::hint::spin_loop;
use log::{error, warn, info, trace, debug};

/// The physical address that an AP jumps to when it first is booted by the BSP.
/// For x2apic systems, this must be at 0x10000 or higher! 
const AP_STARTUP: usize = 0x10000; 

/// The physical address of the memory area for AP startup data passed from the BSP in long mode (Rust) code.
/// Located one page below the AP_STARTUP code entry point, at 0xF000.
const TRAMPOLINE: usize = AP_STARTUP - PAGE_SIZE;

/// The offset from the `TRAMPOLINE` address to where the AP startup code will write `GraphicInfo`.
const GRAPHIC_INFO_OFFSET_FROM_TRAMPOLINE: usize = 0x100;

/// Graphic mode information that will be updated after `handle_ap_cores()` is invoked. 
static GRAPHIC_INFO: Mutex<Option<GraphicInfo>> = Mutex::new(None);

/// Returns information about the currently-active graphical framebuffer.
///
/// This will return `None` if `handle_ap_cores()` has not yet been invoked
/// (which is the function that obtains the graphic info in the first place),
/// or if the obtained graphic info is invalid.
pub fn get_graphic_info() -> Option<GraphicInfo> {
    GRAPHIC_INFO.lock().filter(GraphicInfo::is_valid)
}

/// A structure to access information about the graphical framebuffer mode
/// that was discovered and chosen in the AP's real-mode initialization sequence.
/// 
/// # Struct format
/// The layout of fields in this struct must be kept in sync with the code in 
/// `ap_realmode.asm` that writes to this structure.
#[derive(FromBytes, Clone, Copy, Debug)]
#[repr(packed)]
pub struct GraphicInfo {
    /// The visible width of the screen, in pixels.
    width: u16,
    /// The visible height of the screen, in pixels.
    height: u16,
    /// The physical address of the primary framebuffer memory.
    physical_address: u32,
    /// The `mode` that the VGA is currently operating in.
    ///
    /// This is a bitfield that Theseus doesn't currently use.
    _mode: u16,
    /// The attribute bitfield that describes the VGA mode's capabilities.
    ///
    /// This is a bitfield that Theseus doesn't currently use.
    _attributes: u16,
    /// The total size of the graphic VGA memory in 64 KiB chunks.
    total_memory_size_64_kib_chunks: u16,
    /// The number of bytes in each row or line of the framebuffer's memory.
    /// This is similar to the "stride" of a framebuffer, but is expressed
    /// in units of bytes rather than in units of pixels.
    bytes_per_scanline: u16,
    /// The size of each pixel, in number of bits.
    bits_per_pixel: u8,
    /// The size of a pixel's red component, in number of bits.
    red_mask_size: u8,
    /// The bit position of the least significant byte of a pixel's red component.
    red_field_position: u8,
    /// The size of a pixel's green component, in number of bits.
    green_mask_size: u8,
    /// The bit position of the least significant byte of a pixel's green component.
    green_field_position: u8,
    /// The size of a pixel's blue component, in number of bits.
    blue_mask_size: u8,
    /// The bit position of the least significant byte of a pixel's blue component.
    blue_field_position: u8,
}

impl GraphicInfo {
    /// Checks this `GraphicInfo` to ensure it is valid.
    ///
    /// Currently, its width, height, and physical address all must be non-zero.
    fn is_valid(&self) -> bool {
        self.width != 0 && self.height != 0 && self.physical_address != 0
    }

    /// Returns the visible width of the screen, in pixels.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Returns the visible height of the screen, in pixels.
    pub fn height(&self) -> u16 {
        self.height
    }

    /// Returns the physical address of the primary framebuffer memory.
    pub fn physical_address(&self) -> u32 {
        self.physical_address
    }

    /// Returns the total size in bytes of the VGA graphics memory.
    ///
    /// This memory contains the framebuffer as well as any extra visible
    /// displayable memory, which can be used for any graphics purposes,
    /// e.g., a backbuffer for double buffering (aka page flipping).
    pub fn total_memory_size_in_bytes(&self) -> u32 {
        (self.total_memory_size_64_kib_chunks as u32) << 16
    }

    /// The number of bytes in each row or line of the framebuffer's memory.
    ///
    /// This is similar to the "stride" of a framebuffer, but is expressed
    /// in units of bytes rather than in units of pixels.
    pub fn bytes_per_scanline(&self) -> u16 {
        self.bytes_per_scanline
    }

    /// The size of each pixel, in number of bits, *not* bytes.
    pub fn bits_per_pixel(&self) -> u8 {
        self.bits_per_pixel
    }

    /// The size of a pixel's Red value, in number of bits.
    pub fn red_size(&self) -> u8 {
        self.red_mask_size
    }

    /// The position of the least significant bit of a pixel's Red value.
    pub fn red_position(&self) -> u8 {
        self.red_field_position
    }

    /// The size of a pixel's Green value, in number of bits.
    pub fn green_size(&self) -> u8 {
        self.green_mask_size
    }

    /// The position of the least significant bit of a pixel's Green value.
    pub fn green_position(&self) -> u8 {
        self.green_field_position
    }
    
    /// The size of a pixel's Blue value, in number of bits.
    pub fn blue_size(&self) -> u8 {
        self.blue_mask_size
    }

    /// The position of the least significant bit of a pixel's Blue value.
    pub fn blue_position(&self) -> u8 {
        self.blue_field_position
    }
}

/// Information needed to bring up APs (secondary CPUs) on x86_64.
pub enum MulticoreBringupInfo {
    /// Historique (Multiboot2, UEFI maison) : le BSP réveille chaque cœur par INIT-SIPI, et le
    /// cœur passe du mode réel 16 bits au mode long via le code `ap_realmode.asm`.
    Trampoline {
        /// The starting virtual addresses of the `ap_start` realmode (16-bit) code.
        ap_start_realmode_begin: VirtualAddress,
        /// The ending virtual addresses (exclusive) of the `ap_start` realmode (16-bit) code.
        ap_start_realmode_end: VirtualAddress,
        /// The address of the GDT set up for each AP (secondary CPU)
        ap_gdt: VirtualAddress,
    },
    /// Protocole Limine : le chargeur a déjà démarré tous les cœurs, en mode long, et les garde
    /// parqués sur leur `goto_address`. Il suffit d'y écrire notre point d'entrée : ni mode
    /// réel, ni trampoline sous 1 Mio, ni attentes PIT de 10 ms par cœur.
    Limine {
        /// Les cœurs secondaires (le BSP n'y figure pas).
        aps: &'static [LimineAp],
    },
}

/// Un cœur secondaire parqué par Limine.
#[derive(Clone, Copy, Debug)]
pub struct LimineAp {
    /// Identifiant ACPI du processeur (celui des entrées NMI de la MADT).
    pub processor_id: u32,
    pub lapic_id: u32,
    /// Adresse physique de sa `struct limine_mp_info`.
    pub mp_info_phys: usize,
}

/// Décalage de `goto_address` dans `struct limine_mp_info`
/// (`processor_id: u32, lapic_id: u32, reserved: u64, goto_address, extra_argument`).
const LIMINE_GOTO_OFFSET: usize = 16;

/// Ce que le BSP prépare pour le cœur qu'il libère (un à la fois). Statique de l'image du noyau :
/// projetée à la même adresse par les tables de Limine (que le cœur utilise en arrivant) et par
/// les nôtres.
#[repr(C)]
struct LimineApBoot {
    cr3: AtomicU64,
    stack_top: AtomicU64,
    entry: AtomicU64,
    processor_id: AtomicU64,
    apic_id: AtomicU64,
    stack_bottom: AtomicU64,
    nmi_lint: AtomicU64,
    nmi_flags: AtomicU64,
    /// Mis à 1 par le cœur quand il a tout lu.
    taken: AtomicU64,
}

static LIMINE_AP_BOOT: LimineApBoot = LimineApBoot {
    cr3: AtomicU64::new(0),
    stack_top: AtomicU64::new(0),
    entry: AtomicU64::new(0),
    processor_id: AtomicU64::new(0),
    apic_id: AtomicU64::new(0),
    stack_bottom: AtomicU64::new(0),
    nmi_lint: AtomicU64::new(0),
    nmi_flags: AtomicU64::new(0),
    taken: AtomicU64::new(0),
};

/// GDT minimale des cœurs pendant leur entrée (remplacée ensuite par `interrupts::init_ap`) :
/// celle de Limine vit dans sa mémoire, qui n'est plus projetée une fois CR3 changé.
///
/// Elle est en lecture seule : le bit « accessed » (40) est donc déjà posé, sinon le processeur
/// l'écrirait en chargeant le segment et ferait une faute de page (puis une triple faute, faute
/// d'IDT à ce stade).
#[repr(C, align(16))]
struct ApGdt([u64; 3]);
static LIMINE_AP_GDT: ApGdt = ApGdt([
    0,
    (1 << 40) | (1 << 41) | (1 << 43) | (1 << 44) | (1 << 47) | (1 << 53), // code 64 bits
    (1 << 40) | (1 << 41) | (1 << 44) | (1 << 47),                         // données
]);
#[repr(C, packed)]
struct GdtPtr {
    limit: u16,
    base: &'static ApGdt,
}
static LIMINE_AP_GDT_PTR: GdtPtr = GdtPtr { limit: (3 * 8 - 1) as u16, base: &LIMINE_AP_GDT };

/// Génère le point d'entrée des cœurs libérés par Limine ; `$avx` : réglages AVX éventuels
/// (le gabarit `asm!` n'accepte que des littéraux).
macro_rules! limine_ap_entry_fn {
    ($($avx:literal),*) => {
        /// Point d'entrée d'un cœur libéré par Limine (`RDI` = sa `limine_mp_info`, ignorée).
        ///
        /// Le cœur arrive en mode long sur les tables de pages de Limine. Il passe sur les
        /// nôtres, qui projettent le noyau aux mêmes adresses, prend la pile allouée par le BSP,
        /// charge une GDT à nous, règle les mêmes bits que le BSP (SSE, NX, `syscall`, PGE),
        /// puis appelle `kstart_ap`.
        #[naked]
        unsafe extern "C" fn limine_ap_entry() -> ! {
            asm!(
                "cli",
                "cld",
                "mov rax, [rip + {boot}]",        // cr3
                "mov cr3, rax",
                "mov rsp, [rip + {boot} + 8]",    // stack_top
                "lgdt [rip + {gdt_ptr}]",
                "push 0x08",
                "lea rax, [rip + 2f]",
                "push rax",
                "retfq",
                "2:",
                "xor eax, eax",
                "mov ss, ax",
                "mov ds, ax",
                "mov es, ax",
                "mov fs, ax",
                "mov gs, ax",
                // CR0 : coprocesseur surveillé, pas émulé, écriture protégée.
                "mov rax, cr0",
                "and rax, ~(1 << 2)",
                "or rax, (1 << 1) | (1 << 16)",
                "mov cr0, rax",
                // CR4 : PGE, OSFXSR, OSXMMEXCPT.
                "mov rax, cr4",
                "or rax, (1 << 7) | (1 << 9) | (1 << 10)",
                "mov cr4, rax",
                $($avx,)*
                // EFER : NXE et SCE (LME est déjà là).
                "mov ecx, 0xC0000080",
                "rdmsr",
                "or eax, (1 << 11) | (1 << 0)",
                "wrmsr",
                // TSC_AUX invalide jusqu'à l'initialisation du cœur ; bases FS/GS nulles.
                "mov ecx, 0xC0000103",
                "mov eax, 0xFFFFFFFF",
                "xor edx, edx",
                "wrmsr",
                "xor eax, eax",
                "mov ecx, 0xC0000100",
                "wrmsr",
                "mov ecx, 0xC0000101",
                "wrmsr",
                "mov ecx, 0xC0000102",
                "wrmsr",
                // Arguments de `kstart_ap`.
                "mov rdi, [rip + {boot} + 24]",   // processor_id
                "mov rsi, [rip + {boot} + 32]",   // apic_id
                "mov rdx, [rip + {boot} + 40]",   // bas de pile
                "mov rcx, [rip + {boot} + 8]",    // haut de pile
                "mov r8,  [rip + {boot} + 48]",   // nmi_lint
                "mov r9,  [rip + {boot} + 56]",   // nmi_flags
                "mov rax, [rip + {boot} + 16]",   // entry
                "mov qword ptr [rip + {boot} + 64], 1", // taken : le BSP peut préparer le suivant
                "and rsp, ~0xF",
                "call rax",
                "3:",
                "cli",
                "hlt",
                "jmp 3b",
                boot = sym LIMINE_AP_BOOT,
                gdt_ptr = sym LIMINE_AP_GDT_PTR,
                options(noreturn)
            )
        }
    };
}

#[cfg(target_feature = "avx")]
limine_ap_entry_fn!(
    "mov rax, cr4",
    "or rax, 1 << 18",   // OSXSAVE
    "mov cr4, rax",
    "xor ecx, ecx",
    "xgetbv",
    "or eax, 0b111",     // x87, SSE, AVX
    "xor ecx, ecx",
    "xsetbv"
);
#[cfg(not(target_feature = "avx"))]
limine_ap_entry_fn!();


/// Starts up and sets up AP cores based on system information from ACPI
/// (specifically the MADT (APIC) table).
/// 
/// # Arguments: 
/// * `kernel_mmi_ref`: A reference to the MMI structure with the kernel's page table.
/// * `max_framebuffer_resolution`: the maximum resolution `(width, height)` of the graphical framebuffer
///    that an AP should request from the BIOS when it boots up in 16-bit real mode.
///    If `None`, there will be no maximum.
pub fn handle_ap_cores(
    kernel_mmi_ref: &MmiRef,
    multicore_info: MulticoreBringupInfo,
) -> Result<u32, &'static str> {
    let (ap_start_realmode_begin, ap_start_realmode_end, ap_gdt) = match multicore_info {
        MulticoreBringupInfo::Limine { aps } => return handle_limine_aps(kernel_mmi_ref, aps),
        MulticoreBringupInfo::Trampoline { ap_start_realmode_begin, ap_start_realmode_end, ap_gdt } =>
            (ap_start_realmode_begin, ap_start_realmode_end, ap_gdt),
    };
    let ap_startup_size_in_bytes = ap_start_realmode_end.value() - ap_start_realmode_begin.value();

    let page_table_phys_addr: PhysicalAddress;
    let mut trampoline_mapped_pages: MappedPages; // must be held throughout APs being booted up
    let mut ap_startup_mapped_pages: MappedPages; // must be held throughout APs being booted up
    {
        let mut kernel_mmi = kernel_mmi_ref.lock();
        let page_table = &mut kernel_mmi.page_table;
        // first, double check that the ap_start_realmode address is mapped and valid
        page_table.translate(ap_start_realmode_begin).ok_or("handle_ap_cores(): couldn't translate ap_start_realmode address")?;

        // Map trampoline frame and the ap_startup code to the AP_STARTUP frame.
        // These frames MUST be identity mapped because they're accessed in AP boot up code,
        // which has no page tables because it operates in 16-bit real mode.
        let trampoline_frame  = memory::allocate_frames_at(PhysicalAddress::new_canonical(TRAMPOLINE), 1)
            .map_err(|_e| "handle_ap_cores(): failed to allocate trampoline frame")?;
        let trampoline_page   = memory::allocate_pages_at(VirtualAddress::new_canonical(TRAMPOLINE), trampoline_frame.size_in_frames())
            .map_err(|_e| "handle_ap_cores(): failed to allocate trampoline page")?;
        let ap_startup_frames = memory::allocate_frames_by_bytes_at(PhysicalAddress::new_canonical(AP_STARTUP), ap_startup_size_in_bytes)
            .map_err(|_e| "handle_ap_cores(): failed to allocate AP startup frames")?;
        let ap_startup_pages  = memory::allocate_pages_at(VirtualAddress::new_canonical(AP_STARTUP), ap_startup_frames.size_in_frames())
            .map_err(|_e| "handle_ap_cores(): failed to allocate AP startup pages")?;
        
        let flags = PteFlags::new().valid(true).writable(true);
        trampoline_mapped_pages = page_table.map_allocated_pages_to(
            trampoline_page, 
            trampoline_frame, 
            flags,
        )?;
        ap_startup_mapped_pages = page_table.map_allocated_pages_to(
            ap_startup_pages,
            ap_startup_frames,
            flags,
        )?;
        page_table_phys_addr = page_table.physical_address();
    }

    let all_lapics = get_lapics();
    let this_cpu = current_cpu();

    // Copy the AP startup code (from the kernel's text section pages) into the AP_STARTUP physical address entry point.
    {
        // First, get the kernel's text pages, which is the MappedPages object that contains the vaddr `ap_start_realmode_begin`.
        let kernel_text_pages_ref = mod_mgmt::get_initial_kernel_namespace()
            .ok_or("BUG: couldn't get the initial kernel CrateNamespace")
            .and_then(|namespace| namespace.get_crate("nano_core").ok_or("BUG: couldn't get the 'nano_core' crate"))
            .and_then(|nano_core_crate| nano_core_crate.lock_as_ref().text_pages.clone().ok_or("BUG: nano_core crate had no text pages"))?;
        let kernel_text_pages = kernel_text_pages_ref.0.lock();
        // Second, perform the actual copy.
        let source_slice: &[u8] = kernel_text_pages.offset_of_address(ap_start_realmode_begin)
            .ok_or("BUG: the 'ap_start_realmode_begin' virtual address was not covered by the kernel's text pages")
            .and_then(|offset| kernel_text_pages.as_slice(offset, ap_startup_size_in_bytes))?;
        let dest_slice: &mut [u8] = ap_startup_mapped_pages.as_slice_mut(0, ap_startup_size_in_bytes)?;
        dest_slice.copy_from_slice(source_slice);
    }
    // Now, the AP startup code is at the PhysicalAddress `AP_STARTUP`.

    let mut ap_count = 0; // the number of AP cores we have successfully booted.
    let mut is_last_ap = false;
    let ap_trampoline_data: &mut ApTrampolineData = trampoline_mapped_pages.as_type_mut(0)?;
    // Here, we set up the data items that will be accessible to the APs when they boot up.
    // We only set the values of fields that are the same for ALL APs here;
    // values that change for each AP are set individually in `bring_up_ap()` below.
    let (max_width, max_height) = FRAMEBUFFER_MAX_RESOLUTION;
    ap_trampoline_data.ap_max_fb_width.write(max_width);
    ap_trampoline_data.ap_max_fb_height.write(max_height);
    ap_trampoline_data.ap_gdt.write(ap_gdt.value().try_into().map_err(|_| "AP_GDT physical address larger than u32::MAX")?);

    let acpi_tables = acpi::get_acpi_tables().lock();
    let madt = Madt::get(&acpi_tables)
        .ok_or("Couldn't find the MADT APIC table. Has the ACPI subsystem been initialized yet?")?;
    let madt_iter = madt.iter();

    // Count the total number of CPUs first so we know when we're bringing up the last AP.
    let total_cpus_expected: u32 = {
        let mut total = 0;
        for madt_entry in madt_iter.clone() {
            let flags = match madt_entry {
                MadtEntry::LocalApic(entry)   => entry.flags,
                MadtEntry::LocalX2Apic(entry) => entry.flags,
                _ => continue,
            };
            if flags & 0x1 == 0x1 {
                total += 1;
            }
        }
        total
    };

    // Now, bring up each AP sequentially.
    for madt_entry in madt_iter.clone() {
        let (processor_id, apic_id, flags) = match madt_entry {
            MadtEntry::LocalApic(entry) => (entry.processor as u32, entry.apic_id as u32, entry.flags),
            MadtEntry::LocalX2Apic(entry) => (entry.processor, entry.x2apic_id, entry.flags),
            _ => continue,
        };

        // we already handled the BSP in a different function
        if this_cpu.value() == apic_id {
            continue;
        }

        if flags & 0x1 != 0x1 {
            warn!("Processor {} apic_id {} is disabled by the hardware, cannot initialize or use it.", 
                processor_id, apic_id);
            continue;
        }

        // start up this AP, and have it create a new LocalApic for itself. 
        // This must be done by each core itself, and not called repeatedly by the BSP on behalf of other cores.
        let bsp_lapic_ref = bootstrap_cpu()
            .and_then(|bsp_id| all_lapics.get(&bsp_id))
            .ok_or("Couldn't get BSP's LocalApic!")?;
        let mut bsp_lapic = bsp_lapic_ref.write();
        let ap_stack = stack::alloc_stack(
            KERNEL_STACK_SIZE_IN_PAGES,
            &mut kernel_mmi_ref.lock().page_table,
        ).ok_or("could not allocate AP stack!")?;

        let (nmi_lint, nmi_flags) = find_nmi_entry_for_processor(processor_id, madt_iter.clone());

        // Subtracr 2:  -1 for the BSP, and another -1 for the last AP.
        is_last_ap = ap_count == total_cpus_expected.saturating_sub(2);

        bring_up_ap(
            bsp_lapic.deref_mut(), 
            processor_id,
            apic_id,
            ap_trampoline_data,
            page_table_phys_addr, 
            ap_stack, 
            nmi_lint,
            nmi_flags,
            is_last_ap,
        );
        ap_count += 1;
    }

    // Retrieve the graphic mode information written during the AP bootup sequence in `ap_realmode.asm`.
    if ap_count != 0 && is_last_ap {
        let graphic_info = trampoline_mapped_pages
            .as_type::<GraphicInfo>(GRAPHIC_INFO_OFFSET_FROM_TRAMPOLINE)?;
        info!("Obtained graphic info from real mode: {:?}", graphic_info);
        *GRAPHIC_INFO.lock() = Some(*graphic_info);
    }
    
    // Wait for all CPUs to finish booting and init
    info!("handle_ap_cores(): BSP is waiting for APs to boot...");
    let expected_cpus = ap_count + 1;
    let mut num_known_cpus = cpu_count();
    let mut iter = 0;
    while num_known_cpus < expected_cpus {
        spin_loop();
        num_known_cpus = cpu_count();
        if iter == 100000 {
            trace!("BSP is waiting for APs to boot ({} of {})", num_known_cpus, expected_cpus);
            iter = 0;
        }
        iter += 1;
    }
    
    Ok(ap_count)  
}


/// Libère les cœurs parqués par Limine, un à un.
fn handle_limine_aps(kernel_mmi_ref: &MmiRef, aps: &'static [LimineAp]) -> Result<u32, &'static str> {
    let page_table_phys_addr = kernel_mmi_ref.lock().page_table.physical_address();
    let acpi_tables = acpi::get_acpi_tables().lock();
    let madt = Madt::get(&acpi_tables)
        .ok_or("Couldn't find the MADT APIC table. Has the ACPI subsystem been initialized yet?")?;

    let mut ap_count = 0;
    for ap in aps {
        let ap_stack = stack::alloc_stack(KERNEL_STACK_SIZE_IN_PAGES, &mut kernel_mmi_ref.lock().page_table)
            .ok_or("could not allocate AP stack!")?;
        let (nmi_lint, nmi_flags) = find_nmi_entry_for_processor(ap.processor_id, madt.iter());

        let boot = &LIMINE_AP_BOOT;
        boot.cr3.store(page_table_phys_addr.value() as u64, Ordering::Relaxed);
        boot.stack_top.store(ap_stack.top_unusable().value() as u64, Ordering::Relaxed);
        boot.stack_bottom.store(ap_stack.bottom().value() as u64, Ordering::Relaxed);
        boot.entry.store(kstart_ap as usize as u64, Ordering::Relaxed);
        boot.processor_id.store(ap.processor_id as u64, Ordering::Relaxed);
        boot.apic_id.store(ap.lapic_id as u64, Ordering::Relaxed);
        boot.nmi_lint.store(nmi_lint as u64, Ordering::Relaxed);
        boot.nmi_flags.store(nmi_flags as u64, Ordering::Relaxed);
        boot.taken.store(0, Ordering::Relaxed);
        AP_READY_FLAG.store(false, Ordering::SeqCst);
        ap_start::insert_ap_stack(ap.lapic_id, ap_stack);

        // `goto_address` vit dans la mémoire du chargeur : on projette sa page le temps de
        // l'écriture (publication avec sémantique release, comme l'exige le protocole).
        {
            let page_offset = ap.mp_info_phys & (PAGE_SIZE - 1);
            let frame_paddr = PhysicalAddress::new_canonical(ap.mp_info_phys - page_offset);
            let frames = memory::allocate_frames_at(frame_paddr, 1)
                .map_err(|_| "limine : page de limine_mp_info indisponible")?;
            let pages = memory::allocate_pages(1).ok_or("limine : plus de pages virtuelles")?;
            let mp = kernel_mmi_ref.lock().page_table.map_allocated_pages_to(
                pages, frames, PteFlags::new().valid(true).writable(true),
            )?;
            let goto_vaddr = mp.start_address().value() + page_offset + LIMINE_GOTO_OFFSET;
            // SAFETY: la page est projetée en écriture juste au-dessus et reste vivante (`mp`)
            // pendant l'écriture ; le champ est aligné sur 8 octets dans `limine_mp_info`.
            let goto = unsafe { &*(goto_vaddr as *const AtomicU64) };
            goto.store(limine_ap_entry as usize as u64, Ordering::SeqCst);
        }

        while boot.taken.load(Ordering::Acquire) == 0 || !AP_READY_FLAG.load(Ordering::SeqCst) {
            spin_loop();
        }
        info!(" AP {} (proc {}) est dans le code Rust.", ap.lapic_id, ap.processor_id);
        ap_count += 1;
    }

    let expected_cpus = ap_count + 1;
    while cpu_count() < expected_cpus {
        spin_loop();
    }
    Ok(ap_count)
}

/// The data items used when an AP core is booting up in real mode.
///
/// # Important Layout Note
/// The order of the members in this struct must exactly match how they are used
/// and specified in the AP bootup code (at the top of `defines.asm`).
#[derive(Debug, FromBytes)]
#[repr(C)]
struct ApTrampolineData {
    /// A flag that indicates whether the new AP is ready. 
    /// The Rust setup code sets it to 0, and the AP boot code sets it to 1.
    ap_ready:          Volatile<u64>,
    /// The processor ID of the new AP that is being brought up.
    ap_processor_id:   Volatile<u32>,
    _padding0:         [u8; 4],
    /// The CPU ID of the new AP that is being brought up.
    ap_cpu_id:         Volatile<u32>,
    _padding1:         [u8; 4],
    /// The physical address of the top-level P4 page table root (value of CR3).
    ap_page_table:     Volatile<PhysicalAddress>,
    /// The starting virtual address (bottom) of the stack that was allocated for the new AP.
    ap_stack_start:    Volatile<VirtualAddress>,
    /// The ending virtual address (top) of the stack that was allocated for the new AP.
    ap_stack_end:      Volatile<VirtualAddress>,
    /// The virtual address of the Rust entry point that the new AP should jump to after booting up.
    ap_code:           Volatile<VirtualAddress>,
    /// The NMI LINT (Non-Maskable Interrupt Local Interrupt) value for the new AP.
    ap_nmi_lint:       Volatile<u8>,
    _padding2:         [u8; 7],
    /// The NMI (Non-Maskable Interrupt) flags value for the new AP.
    ap_nmi_flags:      Volatile<u16>,
    _padding3:         [u8; 6],
    /// The maximum width in pixels of the graphical framebuffer that an AP should request
    /// when changing graphical framebuffer modes in its 16-bit real-mode code. 
    ap_max_fb_width:   Volatile<u16>,
    _padding4:         [u8; 6],
    /// The maximum height in pixels of the graphical framebuffer that an AP should request
    /// when changing graphical framebuffer modes in its 16-bit real-mode code. 
    ap_max_fb_height:  Volatile<u16>,
    _padding5:         [u8; 6],
    /// The location of the GDT_AP symbol in physical memory.
    ap_gdt:            Volatile<u32>,
    _padding6:         [u8; 4],
    /// A flag indicating whether this is the last AP being brought up:
    /// * A value of `0` indicates this is NOT the last AP to be initialized,
    ///   so no graphics mode switching should occur.
    /// * A value of `1` indicates this is the last AP to be initialized,
    ///   and therefore it should perform the graphics mode switch.
    ap_is_last_ap:     Volatile<u8>,
    _padding7:         [u8; 7],
}
const _: () = assert!(size_of::<ApTrampolineData>() == 13 * size_of::<u64>());


/// Called by the BSP to initialize the given `new_lapic` using IPIs.
#[allow(clippy::too_many_arguments)]
fn bring_up_ap(
    bsp_lapic: &mut LocalApic,
    new_apic_processor_id: u32,
    new_apic_id: u32,
    ap_trampoline_data: &mut ApTrampolineData,
    page_table_paddr: PhysicalAddress, 
    ap_stack: stack::Stack,
    nmi_lint: u8, 
    nmi_flags: u16,
    is_last_ap: bool,
) {
    ap_trampoline_data.ap_ready.write(0);
    ap_trampoline_data.ap_processor_id.write(new_apic_processor_id);
    ap_trampoline_data.ap_cpu_id.write(new_apic_id);
    ap_trampoline_data.ap_page_table.write(page_table_paddr);
    ap_trampoline_data.ap_stack_start.write(ap_stack.bottom());
    ap_trampoline_data.ap_stack_end.write(ap_stack.top_unusable());
    ap_trampoline_data.ap_code.write(VirtualAddress::new_canonical(kstart_ap as usize));
    ap_trampoline_data.ap_nmi_lint.write(nmi_lint);
    ap_trampoline_data.ap_nmi_flags.write(nmi_flags);
    ap_trampoline_data.ap_is_last_ap.write(if is_last_ap { 1 } else { 0 });
    AP_READY_FLAG.store(false, Ordering::SeqCst);

    // Give ownership of the stack we created for this AP to the `ap_start` crate, 
    // in which the AP will take ownership of it once it boots up.
    ap_start::insert_ap_stack(new_apic_id, ap_stack); 

    info!("Bringing up AP, proc: {} apic_id: {}", new_apic_processor_id, new_apic_id);
    
    bsp_lapic.clear_error();
    let esr = bsp_lapic.error();
    debug!(" initial esr = {:#X}", esr);

    // Send INIT IPI
    {
        // 0x500 means INIT Delivery Mode, 0x4000 means Assert (not de-assert), 0x8000 means level triggers
        let mut icr = /*0x8000 |*/ 0x4000 | 0x500; 
        if has_x2apic() {
            icr |= (new_apic_id as u64) << 32;
        } else {
            icr |= ( new_apic_id as u64) << 56; // destination apic id 
        }
        // icr |= 1 << 11; // (1 << 11) is logical address mode, 0 is physical. Doesn't work with physical addressing mode!
        debug!(" INIT IPI... icr: {:#X}", icr);
        bsp_lapic.set_icr(icr);
    }

    debug!("waiting 10 ms...");
    pit_clock_basic::pit_wait(10000).unwrap_or_else(|_e| { error!("bring_up_ap(): failed to pit_wait 10 ms. Error: {:?}", _e); });
    debug!("done waiting.");

    // // Send DEASSERT INIT IPI
    // {
    //     // 0x500 means INIT Delivery Mode, 0x8000 means level triggers
    //     let mut icr = 0x8000 | 0x500; 
    //     if has_x2apic() {
    //         icr |= (new_apic_id as u64) << 32;
    //     } else {
    //         icr |= ( new_apic_id as u64) << 56; // destination apic id 
    //     }
    //     // icr |= 1 << 11; // (1 << 11) is logical address mode, 0 is physical. Doesn't work with physical addressing mode!
    //     debug!(" DEASSERT IPI... icr: {:#X}", icr);
    //     bsp_lapic.set_icr(icr);
    // }

    bsp_lapic.clear_error();
    let esr = bsp_lapic.error();
    debug!(" pre-SIPI esr = {:#X}", esr);

    // Send START IPI
    {
        //Start at 0x1000:0000 => 0x10000. We copied the ap_start_realmode code into AP_STARTUP earlier, in handle_apic_entry()
        let ap_segment = (AP_STARTUP >> PAGE_SHIFT) & 0xFF; // the frame number where we want the AP to start executing from boot
        let mut icr = /*0x8000 |*/ 0x4000 | 0x600 | ap_segment as u64; //0x600 means Startup IPI

        if has_x2apic() {
            icr |= (new_apic_id as u64) << 32;
        } else {
            icr |= (new_apic_id as u64) << 56;
        }
        // icr |= 1 << 11; // (1 << 11) is logical address mode, 0 is physical. Doesn't work with physical addressing mode!
        debug!(" SIPI... icr: {:#X}", icr);
        bsp_lapic.set_icr(icr);
    }

    pit_clock_basic::pit_wait(300).unwrap_or_else(|_e| { error!("bring_up_ap(): failed to pit_wait 300 us. Error {:?}", _e); });
    pit_clock_basic::pit_wait(200).unwrap_or_else(|_e| { error!("bring_up_ap(): failed to pit_wait 200 us. Error {:?}", _e); });

    bsp_lapic.clear_error();
    let esr = bsp_lapic.error();
    debug!(" post-SIPI esr = {:#X}", esr);
    // TODO: we may need to send a second START IPI on real hardware???

    // Wait for trampoline ready
    debug!(" Wait...");
    while ap_trampoline_data.ap_ready.read() == 0 {
        spin_loop();
    }
    debug!(" Trampoline...");
    while ! AP_READY_FLAG.load(Ordering::SeqCst) {
        spin_loop();
    }
    info!(" AP {} is in Rust code. Ready!", new_apic_id);
}
