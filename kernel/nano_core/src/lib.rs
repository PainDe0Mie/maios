#![no_std]
#![no_main]
#![feature(let_chains)]
#![feature(naked_functions)]

extern crate panic_entry;

// === IMPORTATIONS REQUISES ===
use core::ops::DerefMut;
use captain::MulticoreBringupInfo;
use memory::VirtualAddress;
use mod_mgmt::parse_nano_core::NanoCoreItems;
use serial_port_basic::{take_serial_port, SerialPortAddress};
use early_printer::println;
use kernel_config::memory::KERNEL_OFFSET;

mod build_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

cfg_if::cfg_if! {
    if #[cfg(feature = "uefi")] {
        mod uefi;
    } else if #[cfg(feature = "bios")] {
        mod bios;
    } else {
        compile_error!("either the 'bios' or 'uefi' feature must be enabled");
    }
}

#[macro_export]
macro_rules! try_exit {
    ($expr:expr) => {
        $expr.unwrap_or_else(|e| $crate::shutdown(format_args!("{e}")))
    };
}

/// Éteint l'OS en cas d'erreur critique au tout début du démarrage
pub fn shutdown(msg: core::fmt::Arguments) -> ! {
    println!("Mai is shutting down, msg: {}", msg);
    log::error!("Mai is shutting down, msg: {}", msg);
    panic!("{}", msg);
}

// === FONCTION PRINCIPALE ===
#[cfg_attr(target_arch = "aarch64", allow(unused_variables))]
pub fn nano_core<B>(
    boot_info: B,
    double_fault_stack_top: VirtualAddress,
    kernel_stack_start: VirtualAddress,
) -> Result<(), &'static str>
where
    B: boot_info::BootInformation
{
    // Étape 1 : On désactive les interruptions matérielles pour démarrer tranquillement
    irq_safety::disable_interrupts();
    #[cfg(target_arch = "aarch64")]
    irq_safety::disable_fast_interrupts();

    println!("nano_core(): Entered early setup. Interrupts disabled.");

    // Étape 2 : Configuration initiale des logs via le port série
    #[cfg(target_arch = "x86_64")]
    let logger_ports = [take_serial_port(SerialPortAddress::COM1)];

    #[cfg(target_arch = "aarch64")]
    let logger_ports: [[serial_port_basic::SerialPort; 0]; 0] = [];

    logger::early_init(None, IntoIterator::into_iter(logger_ports).flatten());
    log::info!("initialized early logger");
    println!("nano_core(): initialized early logger.");

    // Étape 3 : Initialisation des exceptions de base (pour capturer les gros crashs tôt)
    #[cfg(target_arch = "x86_64")] {
        exceptions_early::init(Some(double_fault_stack_top));
        println!("nano_core(): initialized early IDT with exception handlers.");
    }

    // Étape 4 : Activation de l'écran si le bootloader a préparé la mémoire vidéo
    if let Some(ref fb_info) = boot_info.framebuffer_info() && fb_info.is_mapped() {
        early_printer::init(fb_info, None).unwrap_or_else(|_e|
            log::error!("Failed to init early_printer; proceeding with init. Error: {:?}", _e)
        );
    }

    let rsdp_address = boot_info.rsdp();
    
    // Étape 5 : Mise en place de toute la gestion de la mémoire (tas, pile, pagination)
    let (
        kernel_mmi_ref,
        text_mapped_pages,
        rodata_mapped_pages,
        data_mapped_pages,
        stack,
        bootloader_modules,
        identity_mapped_pages
    ) = memory_initialization::init_memory_management(boot_info, kernel_stack_start)?;

    #[cfg(target_arch = "aarch64")] {
        let logger_ports = [take_serial_port(SerialPortAddress::COM1)];
        logger::early_init(None, IntoIterator::into_iter(logger_ports).flatten());
        log::info!("initialized early logger with aarch64 serial ports.");
        println!("nano_core(): initialized early logger with aarch64 serial ports.");
    }

    println!("nano_core(): initialized memory subsystem.");
    println!("nano_core(): bootloader-provided RSDP address: {:X?}", rsdp_address);

    log::info!("\n    \
        ===================== Mai build info: =====================\n    \
        CUSTOM CFGs: {} \n    \
        ===============================================================",
        build_info::CUSTOM_CFG_STR,
    );

    // Étape 6 : Initialisation du stockage d'état global
    state_store::init();
    log::trace!("state_store initialized.");
    println!("nano_core(): initialized state store.");

    // Étape 7 : Préparation du système qui gère les différents modules de code
    let default_namespace = mod_mgmt::init(bootloader_modules, kernel_mmi_ref.lock().deref_mut())?;
    println!("nano_core(): initialized crate namespace subsystem.");

    println!("nano_core(): parsing nano_core crate, please wait ...");
    let (nano_core_crate_ref, multicore_info) = match mod_mgmt::parse_nano_core::parse_nano_core(
        default_namespace,
        text_mapped_pages.into_inner(),
        rodata_mapped_pages.into_inner(),
        data_mapped_pages.into_inner(),
        false,
    ) {
        Ok(NanoCoreItems { nano_core_crate_ref, init_symbol_values, num_new_symbols }) => {
            println!("nano_core(): finished parsing the nano_core crate, {} new symbols.", num_new_symbols);

            #[cfg(target_arch = "x86_64")]
            let multicore_info = {
                let ap_realmode_begin = init_symbol_values
                    .get("ap_start_realmode")
                    .and_then(|v| VirtualAddress::new(*v + KERNEL_OFFSET))
                    .ok_or("Missing/invalid symbol expected from assembly code \"ap_start_realmode\"")?;
                let ap_realmode_end = init_symbol_values
                    .get("ap_start_realmode_end")
                    .and_then(|v| VirtualAddress::new(*v + KERNEL_OFFSET))
                    .ok_or("Missing/invalid symbol expected from assembly code \"ap_start_realmode_end\"")?;

                let ap_gdt = nano_core_crate_ref.lock_as_ref()
                    .sections
                    .values()
                    .find(|sec| &*sec.name == "GDT_AP")
                    .map(|ap_gdt_sec| ap_gdt_sec.virt_addr)
                    .ok_or("Missing/invalid symbol expected from data section \"GDT_AP\"")
                    .and_then(|vaddr| memory::translate(vaddr)
                        .ok_or("Failed to translate \"GDT_AP\"")
                    )
                    .and_then(|paddr| VirtualAddress::new(paddr.value())
                        .ok_or("\"GDT_AP\" physical address was not a valid identity virtual address")
                    )?;
                
                MulticoreBringupInfo {
                    ap_start_realmode_begin: ap_realmode_begin,
                    ap_start_realmode_end: ap_realmode_end,
                    ap_gdt,
                }
            };

            #[cfg(target_arch = "aarch64")]
            let multicore_info = MulticoreBringupInfo { };

            (nano_core_crate_ref, multicore_info)
        }
        Err((msg, _mapped_pages_array)) => return Err(msg),
    };

    #[cfg(loadable)] {
        // Espace pour les hooks si loadable
    }
    core::mem::drop(nano_core_crate_ref);
    
    #[cfg(loadable)] {
        use mod_mgmt::CrateNamespace;
        println!("nano_core(): loading the \"captain\" crate...");
        let (captain_file, _ns) = CrateNamespace::get_crate_object_file_starting_with(default_namespace, "captain-").ok_or("couldn't find the singular \"captain\" crate object file")?;
        let (_captain_crate, _num_captain_syms) = default_namespace.load_crate(&captain_file, None, &kernel_mmi_ref, false)?;
        
        println!("nano_core(): loading the panic handling crate(s)...");
        let (panic_wrapper_file, _ns) = CrateNamespace::get_crate_object_file_starting_with(default_namespace, "panic_wrapper-").ok_or("couldn't find the singular \"panic_wrapper\" crate object file")?;
        let (_pw_crate, _num_pw_syms) = default_namespace.load_crate(&panic_wrapper_file, None, &kernel_mmi_ref, false)?;

        early_tls::insert(default_namespace.get_tls_initializer_data());
    }

    // Étape 8 : On passe officiellement le contrôle au "Captain"
    println!("nano_core(): invoking the captain...");
    let drop_after_init = captain::DropAfterInit {
        identity_mappings: identity_mapped_pages,
    };
    
    #[cfg(not(loadable))] {
        captain::init(kernel_mmi_ref, stack, drop_after_init, multicore_info, rsdp_address)?;
    }
    
    #[cfg(loadable)] {
        use captain::DropAfterInit;
        use memory::{MmiRef, PhysicalAddress};
        use no_drop::NoDrop;
        use stack::Stack;

        let section = default_namespace
            .get_symbol_starting_with("captain::init::")
            .upgrade()
            .ok_or("no single symbol matching \"captain::init\"")?;
        log::info!("The nano_core (in loadable mode) is invoking the captain init function: {:?}", section);

        type CaptainInitFunc = fn(MmiRef, NoDrop<Stack>, DropAfterInit, MulticoreBringupInfo, Option<PhysicalAddress>) -> Result<(), &'static str>;
        let func: &CaptainInitFunc = unsafe { section.as_func() }?;

        func(kernel_mmi_ref, stack, drop_after_init, multicore_info, rsdp_address)?;
    }

    println!("Loading the captain should have taken us into an infinite loop, oopsi.");
    Err("captain::init returned unexpectedly... it should be an infinite loop (diverging function)")
}

// === EXTERN SYMBOLS & LIBM HACK ===
// Ces blocs sont indispensables pour que l'éditeur de liens (linker) trouve les variables d'assemblage
// et que les fonctions mathématiques basiques fonctionnent en mode "no_std".

#[allow(dead_code)]
extern {
    static initial_bsp_stack_guard_page: usize;
    static initial_bsp_stack_bottom: usize;
    static initial_bsp_stack_top: usize;
    static ap_start_realmode: usize;
    static ap_start_realmode_end: usize;
}

mod libm;

mod stack_smash_protection;
