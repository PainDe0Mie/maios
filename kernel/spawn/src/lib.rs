//! This crate offers routines for spawning new tasks
//! and convenient builder patterns for customizing new tasks.

#![allow(clippy::type_complexity)]
#![no_std]
#![feature(stmt_expr_attributes)]
#![feature(naked_functions)]

extern crate alloc;

use core::{marker::PhantomData, mem, ops::Deref, sync::atomic::{fence, Ordering}};
use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use log::{error, info, debug};
use cpu::CpuId;
use debugit::debugit;
use spin::Mutex;
use memory::{get_kernel_mmi_ref, MmiRef};
use stack::Stack;
use mod_mgmt::{CrateNamespace, SectionType, TlsDataImage, SECTION_HASH_DELIMITER};
use environment::Environment;
use path::{Path, PathBuf};
use fs_node::FileOrDir;
use preemption::{hold_preemption, PreemptionGuard};
use no_drop::NoDrop;
use task::{Task, TaskRef, JoinableTaskRef, ExitableTaskRef, KillReason};

// FailureCleanupFunction is a per-task type-parameterised fn pointer.
// We define it locally to avoid circular dependencies with the task crate.
type FailureCleanupFunction = fn(ExitableTaskRef, KillReason) -> !;

#[cfg(simd_personality)]
use task::SimdExt;


/// Initializes tasking for this CPU.
///
/// NOTE (mai_os): `namespace` and `env` are now required parameters because the
/// bootstrap task needs them to be fully initialised. Pass the initial kernel
/// namespace and a default environment from nano_core.
pub fn init(
    kernel_mmi_ref: MmiRef,
    cpu_id: CpuId,
    stack: NoDrop<Stack>,
    namespace: Arc<CrateNamespace>,
    env: Arc<Mutex<Environment>>,
) -> Result<BootstrapTaskRef, &'static str> {
    let (joinable_bootstrap_task, exitable_bootstrap_task) =
        task::bootstrap_task(cpu_id, stack, kernel_mmi_ref, namespace, env)?;
    BOOTSTRAP_TASKS.lock().push(joinable_bootstrap_task);

    let idle_task = new_task_builder(idle_task_entry, cpu_id)
        .name(format!("idle_task_cpu_{cpu_id}"))
        .idle(cpu_id)
        .spawn_restartable(None)?.0;

    // Register the idle task in MKS (per-CPU idle run queue).
    task::scheduler::register_idle_task(cpu_id, idle_task.clone());
    // Enqueue the bootstrap task on this CPU so it gets scheduled.
    task::scheduler::add_task_to(cpu_id, exitable_bootstrap_task.0.clone());

    Ok(BootstrapTaskRef {
        cpu_id,
        exitable_taskref: exitable_bootstrap_task,
    })
}

/// The set of bootstrap tasks that are created using `task::bootstrap_task()`.
/// These require special cleanup; see [`cleanup_bootstrap_tasks()`].
static BOOTSTRAP_TASKS: Mutex<Vec<JoinableTaskRef>> = Mutex::new(Vec::new());

/// Spawns a dedicated task to cleanup all bootstrap tasks.
pub fn cleanup_bootstrap_tasks(num_tasks: u32) -> Result<(), &'static str> {
    new_task_builder(
        |total_tasks: u32| {
            let mut num_tasks_cleaned = 0;
            while num_tasks_cleaned < total_tasks {
                if let Some(task) = BOOTSTRAP_TASKS.lock().pop() {
                    match task.join() {
                        Ok(_exit_val) => num_tasks_cleaned += 1,
                        Err(_e) => panic!(
                            "BUG: failed to join bootstrap task {:?}, error: {:?}",
                            task, _e,
                        ),
                    }
                }
            }
            info!("Cleaned up all {} bootstrap tasks.", total_tasks);
            *BOOTSTRAP_TASKS.lock() = Vec::new();
            unsafe { early_tls::drop() };
        },
        num_tasks,
    )
    .name(String::from("bootstrap_task_cleanup"))
    .spawn()?;

    Ok(())
}

/// A wrapper around a `TaskRef` for bootstrapped tasks.
#[derive(Debug)]
pub struct BootstrapTaskRef {
    #[allow(dead_code)]
    cpu_id: CpuId,
    exitable_taskref: ExitableTaskRef,
}
impl Deref for BootstrapTaskRef {
    type Target = TaskRef;
    fn deref(&self) -> &TaskRef {
        self.exitable_taskref.deref()
    }
}
impl BootstrapTaskRef {
    pub fn finish(self) {
        drop(self);
    }
}
impl Drop for BootstrapTaskRef {
    fn drop(&mut self) {
        remove_current_task_from_runqueue(&self.exitable_taskref);
        self.exitable_taskref.mark_as_exited(Box::new(()))
            .expect("BUG: bootstrap task was unable to mark itself as exited");
    }
}


/// Creates a builder for a new `Task`.
pub fn new_task_builder<F, A, R>(
    func: F,
    argument: A
) -> TaskBuilder<F, A, R>
    where A: Send + 'static,
          R: Send + 'static,
          F: FnOnce(A) -> R,
{
    TaskBuilder::new(func, argument)
}


const ENTRY_POINT_SECTION_NAME: &str = "main";
type MainFuncArg = Vec<String>;
type MainFuncRet = isize;
type MainFunc = fn(MainFuncArg) -> MainFuncRet;

/// Creates a builder for a new application `Task`.
pub fn new_application_task_builder(
    crate_object_file: &Path,
    new_namespace: Option<Arc<CrateNamespace>>,
) -> Result<TaskBuilder<MainFunc, MainFuncArg, MainFuncRet>, &'static str> {
    let namespace = new_namespace
        .or_else(|| task::with_current_task(|t| t.namespace.clone()).ok())
        .ok_or("spawn::new_application_task_builder(): couldn't get current task")?;

    let crate_object_file = match crate_object_file.get(namespace.dir())
        .or_else(|| PathBuf::from(format!("{}.o", &crate_object_file)).get(namespace.dir()))
    {
        Some(FileOrDir::File(f)) => f,
        _ => return Err("Couldn't find specified file path for new application crate"),
    };

    let app_crate_ref = {
        let kernel_mmi_ref = get_kernel_mmi_ref().ok_or("couldn't get_kernel_mmi_ref")?;
        CrateNamespace::load_crate_as_application(&namespace, &crate_object_file, kernel_mmi_ref, false)?
    };

    let main_func_sec_opt = {
        let app_crate = app_crate_ref.lock_as_ref();
        let expected_main_section_name = format!("{}{}{}", app_crate.crate_name_as_prefix(), ENTRY_POINT_SECTION_NAME, SECTION_HASH_DELIMITER);
        app_crate.find_section(|sec|
            sec.typ == SectionType::Text && sec.name_without_hash() == expected_main_section_name
        ).cloned()
    };
    let main_func_sec = main_func_sec_opt.ok_or("spawn::new_application_task_builder(): couldn't find \"main\" function")?;
    let main_func = unsafe { main_func_sec.as_func::<MainFunc>() }?;

    let mut tb = TaskBuilder::new(*main_func, MainFuncArg::default())
        .name(app_crate_ref.lock_as_ref().crate_name.to_string());

    tb.post_build_function = Some(Box::new(
        move |new_task| {
            new_task.app_crate = Some(Arc::new(app_crate_ref));
            new_task.namespace = namespace;
            Ok(None)
        }
    ));

    Ok(tb)
}

/// A struct that offers a builder pattern to create and customize new `Task`s.
#[must_use = "a `TaskBuilder` does nothing until `spawn()` is invoked on it"]
pub struct TaskBuilder<F, A, R> {
    func: F,
    argument: A,
    _return_type: PhantomData<R>,
    name: Option<String>,
    stack: Option<Stack>,
    parent: Option<TaskRef>,
    pin_on_cpu: Option<CpuId>,
    blocked: bool,
    idle: bool,
    post_build_function: Option<Box<
        dyn FnOnce(&mut Task) -> Result<Option<FailureCleanupFunction>, &'static str>
    >>,
    env: Option<Arc<Mutex<Environment>>>,

    #[cfg(simd_personality)]
    simd: SimdExt,
}

impl<F, A, R> TaskBuilder<F, A, R>
    where A: Send + 'static,
          R: Send + 'static,
          F: FnOnce(A) -> R,
{
    fn new(func: F, argument: A) -> TaskBuilder<F, A, R> {
        TaskBuilder {
            argument,
            func,
            _return_type: PhantomData,
            name: None,
            stack: None,
            parent: None,
            pin_on_cpu: None,
            blocked: false,
            idle: false,
            post_build_function: None,
            env: None,

            #[cfg(simd_personality)]
            simd: SimdExt::None,
        }
    }

    pub fn name(mut self, name: String) -> TaskBuilder<F, A, R> {
        self.name = Some(name);
        self
    }

    pub fn argument(mut self, argument: A) -> TaskBuilder<F, A, R> {
        self.argument = argument;
        self
    }

    pub fn stack(mut self, stack: Stack) -> TaskBuilder<F, A, R> {
        self.stack = Some(stack);
        self
    }

    pub fn parent(mut self, parent_task: TaskRef) -> TaskBuilder<F, A, R> {
        self.parent = Some(parent_task);
        self
    }

    pub fn pin_on_cpu(mut self, cpu_id: CpuId) -> TaskBuilder<F, A, R> {
        self.pin_on_cpu = Some(cpu_id);
        self
    }

    #[cfg(simd_personality)]
    pub fn simd(mut self, extension: SimdExt) -> TaskBuilder<F, A, R> {
        self.simd = extension;
        self
    }

    pub fn env(mut self, env: Arc<Mutex<Environment>>) -> TaskBuilder<F, A, R> {
        self.env = Some(env);
        self
    }

    pub fn block(mut self) -> TaskBuilder<F, A, R> {
        self.blocked = true;
        self
    }

    /// Spawns the new task.
    #[inline(never)]
    pub fn spawn(self) -> Result<JoinableTaskRef, &'static str> {
        // Inherit states from current task (or from explicitly set parent).
        let current = self.parent
            .clone()
            .or_else(|| task::get_my_current_task())
            .ok_or("spawn: couldn't get current task")?;

        let stack = match self.stack {
            Some(s) => s,
            None => {
                let kernel_mmi_ref = memory::get_kernel_mmi_ref()
                    .ok_or("spawn: couldn't get kernel MMI to allocate stack")?;
                let mut kernel_mmi = kernel_mmi_ref.lock();
                stack::alloc_stack(
                    16, // 16 pages = 64 KiB, taille standard pour une stack kernel
                    &mut kernel_mmi.page_table,
                ).ok_or("spawn: failed to allocate stack for new task")?
            }
        };

        let task_id = task::get_next_task_id();
        let tls = TlsDataImage::new();

        let mut new_task = Task::new(
            task_id,
            String::new(), // name set below
            stack,
            current.mmi.clone(),
            current.namespace.clone(),
            self.env.unwrap_or_else(|| current.env.clone()),
            current.app_crate.clone(),
            tls,
        );

        // Override to Initing so setup_context_trampoline can verify state.
        use task_struct::RunState as RS;
        new_task.runstate.store(RS::Initing);

        // Set name (defaults to the type name of F if not provided).
        new_task.name = self.name.unwrap_or_else(|| String::from(core::any::type_name::<F>()));

        // Set pinned CPU if requested.
        if let Some(cpu) = self.pin_on_cpu {
            new_task.pinned_core.store(Option::<CpuId>::Some(cpu).into());
        }

        #[cfg(simd_personality)] {
            new_task.simd = self.simd;
        }

        setup_context_trampoline(&mut new_task, task_wrapper::<F, A, R>)?;

        // Store the entry function and argument at the bottom of the new task's stack.
        let bottom_of_stack: &mut usize = new_task.kstack.as_type_mut(0)?;
        let box_ptr = Box::into_raw(Box::new(TaskFuncArg::<F, A, R> {
            arg:  self.argument,
            func: self.func,
            _ret: PhantomData,
        }));
        *bottom_of_stack = box_ptr as usize;

        // Mark as idle task if requested.
        if self.idle {
            new_task.is_an_idle_task = true;
            new_task.sched.policy = task_struct::SchedClass::Idle;
        }

        // Call the post-build hook if provided.
        let _failure_cleanup_function = match self.post_build_function {
            Some(pb_func) => pb_func(&mut new_task)?,
            None => None,
        };

        // Transition out of Initing state.
        if self.blocked {
            new_task.block_initing_task()
                .map_err(|_| "BUG: newly-spawned blocked task was not in the Initing runstate")?;
        } else {
            new_task.make_inited_task_runnable()
                .map_err(|_| "BUG: newly-spawned task was not in the Initing runstate")?;
        }
        info!("Task {} set to Runnable", new_task.id);

        // Wrap in Arc, register in TASK_LIST, and produce a JoinableTaskRef.
        let task_ref = TaskRef::from_task(new_task);
        let joinable = JoinableTaskRef(task_ref.clone());

        fence(Ordering::Release);

        info!("spawn: adding task {} to scheduler", task_ref.id);

        if !self.idle {
            if let Some(cpu) = self.pin_on_cpu {
                task::scheduler::add_task_to(cpu, task_ref);
            } else {
                task::scheduler::add_task(task_ref);
            }
        }

        Ok(joinable)
    }
}

/// Additional `TaskBuilder` impl for restartable tasks (F and A must be Clone).
impl<F, A, R> TaskBuilder<F, A, R>
    where A: Send + Sync + Clone + 'static,
          R: Send + 'static,
          F: FnOnce(A) -> R + Send + Sync + Clone + 'static,
{
    pub fn idle(mut self, cpu_id: CpuId) -> TaskBuilder<F, A, R> {
        self.idle = true;
        self.pin_on_cpu(cpu_id)
    }

    #[inline(never)]
    pub fn spawn_restartable(
        mut self,
        restart_with_arg: Option<A>
    ) -> Result<JoinableTaskRef, &'static str> {
        let restart_info = task_struct::RestartInfo {
            argument: Box::new(restart_with_arg.unwrap_or_else(|| self.argument.clone())),
            func: Box::new(self.func.clone()),
        };

        self.post_build_function = Some(Box::new(
            move |new_task| {
                new_task.restart_info = Some(restart_info);
                setup_context_trampoline(new_task, task_wrapper_restartable::<F, A, R>)?;
                Ok(Some(task_restartable_cleanup_failure::<F, A, R>))
            }
        ));

        self.spawn()
    }
}


/// A wrapper around a task's function and argument.
#[derive(Debug)]
struct TaskFuncArg<F, A, R> {
    func: F,
    arg:  A,
    _ret: PhantomData<*const R>,
}


/// Sets up the given new task's kernel stack to jump to `entry_point_function`
/// when first switched to.
#[doc(hidden)]
pub fn setup_context_trampoline(
    new_task: &mut Task,
    entry_point_function: fn() -> !
) -> Result<(), &'static str> {
    use task_struct::RunState as RS;
    
    if new_task.runstate.load() != RS::Initing {
        return Err("`setup_context_trampoline()` can only be invoked on `Initing` tasks");
    }
    
    // Utilisation d'une macro propre pour gérer les différents types de contexte SIMD
    // sans dupliquer la logique complexe des pointeurs.
    macro_rules! init_context {
        ($ContextType:ty) => ({
            let new_task_id = new_task.id;
            
            // 1. On récupère le sommet de la pile
            let stack_top_vaddr = new_task.kstack.top_unusable().value(); 
            
            // 2. Calculs des tailles pour respecter l'ABI x86-64.
            // Lors d'un appel de fonction, le CPU empile l'adresse de retour (8 octets).
            // L'ABI exige que la pile soit alignée sur 16 octets AVANT cet appel (call),
            // ce qui signifie qu'elle est désalignée de 8 octets au moment d'entrer dans la fonction.
            let context_size = mem::size_of::<$ContextType>();
            let return_address_size = mem::size_of::<usize>();
            
            // On calcule l'adresse où stocker le contexte pour laisser la place à la fausse "return address"
            let context_vaddr = stack_top_vaddr - context_size - return_address_size;

            // VÉRIFICATION DE SÉCURITÉ : On s'assure que la base de notre "faux appel" 
            // est bien alignée sur 16 octets, comme l'exige le hardware.
            debug_assert!(
                (stack_top_vaddr - return_address_size) % 16 == 0, 
                "FATAL: Stack pointer misalignment detected for task {}!", new_task_id
            );

            // 3. Écriture du contexte en mémoire (Safety: on a calculé l'adresse dans la pile de la tâche)
            let context_dest = unsafe { &mut *(context_vaddr as *mut $ContextType) };
            
            let mut new_context = <$ContextType>::new(entry_point_function as usize);
            new_context.set_first_register(new_task_id);
            *context_dest = new_context;
            
            // 4. On sauvegarde le vrai pointeur de pile final
            new_task.saved_sp = context_dest as *const _ as usize;
            
            // Log de traçabilité (trace plutôt que info pour ne pas polluer la console)
            log::trace!(
                "Tâche {} setup : StackTop={:#X}, ContextVAddr={:#X}, saved_sp={:#X}", 
                new_task_id, stack_top_vaddr, context_vaddr, new_task.saved_sp
            );
        });
    }

    #[cfg(simd_personality)] {
        match new_task.simd {
            SimdExt::AVX  => { init_context!(context_switch::ContextAVX); }
            SimdExt::SSE  => { init_context!(context_switch::ContextSSE); }
            SimdExt::None => { init_context!(context_switch::ContextRegular); }
        }
    }

    #[cfg(not(simd_personality))] {
        init_context!(context_switch::Context);
    }

    Ok(())
}

/// Internal routine shared by `task_wrapper` and `task_wrapper_restartable`.
fn task_wrapper_internal<F, A, R>(
    current_task_id: usize,
) -> (Result<R, task::KillReason>, ExitableTaskRef)
where
    A: Send + 'static,
    R: Send + 'static,
    F: FnOnce(A) -> R,
{
    let task_entry_func;
    let task_arg;
    let recovered_preemption_guard;
    let exitable_taskref;

    {
        // Register this task as the currently-running task for this CPU.
        exitable_taskref = task::init_current_task(current_task_id, None)
            .unwrap_or_else(|_|
                panic!("BUG: task_wrapper: couldn't init task {} as the current task", current_task_id)
            );

        recovered_preemption_guard = exitable_taskref.post_context_switch_action();

        // Recover the entry function and argument from the bottom of the stack.
        let task_func_arg = exitable_taskref.with_kstack(|kstack| {
            kstack.as_type(0).map(|tfa_box_raw_ptr: &usize| {
                let tfa_boxed = unsafe { Box::from_raw((*tfa_box_raw_ptr) as *mut TaskFuncArg<F, A, R>) };
                *tfa_boxed
            })
        }).expect("BUG: task_wrapper: couldn't access task's function/argument at bottom of stack");

        task_entry_func = task_func_arg.func;
        task_arg        = task_func_arg.arg;

        #[cfg(not(rq_eval))]
        debug!("task_wrapper [1]: \"{}\" about to call task entry func {:?} {{{}}} with arg {:?}",
            &**exitable_taskref, debugit!(task_entry_func), core::any::type_name::<F>(), debugit!(task_arg)
        );
    }

    drop(recovered_preemption_guard);
    unsafe { preemption::force_enable_preemption(); }

    fence(Ordering::Release);

    #[cfg(target_arch = "x86_64")]
    let result = catch_unwind::catch_unwind_with_arg(task_entry_func, task_arg);

    #[cfg(not(target_arch = "x86_64"))]
    let result = Ok(task_entry_func(task_arg));

    (result, exitable_taskref)
}

/// Entry point for all new `Task`s.
fn task_wrapper<F, A, R>() -> !
where
    A: Send + 'static,
    R: Send + 'static,
    F: FnOnce(A) -> R,
{
    let current_task_id = context_switch::read_first_register();
    info!("task_wrapper: task {} started", current_task_id);
    let (result, exitable_task_ref) = task_wrapper_internal::<F, A, R>(current_task_id);

    match result {
        Ok(exit_value)   => task_cleanup_success::<F, A, R>(exitable_task_ref, exit_value),
        Err(kill_reason) => task_cleanup_failure::<F, A, R>(exitable_task_ref, kill_reason),
    }
}

/// Entry point for restartable tasks.
fn task_wrapper_restartable<F, A, R>() -> !
where
    A: Send + Sync + Clone + 'static,
    R: Send + 'static,
    F: FnOnce(A) -> R + Send + Sync + Clone + 'static,
{
    let current_task_id = context_switch::read_first_register();
    info!("task_wrapper_restartable: task {} started", current_task_id);
    let (result, exitable_task_ref) = task_wrapper_internal::<F, A, R>(current_task_id);

    match result {
        Ok(exit_value)   => task_restartable_cleanup_success::<F, A, R>(exitable_task_ref, exit_value),
        Err(kill_reason) => task_restartable_cleanup_failure::<F, A, R>(exitable_task_ref, kill_reason),
    }
}


#[inline(always)]
fn task_cleanup_success_internal<R>(current_task: ExitableTaskRef, exit_value: R) -> (PreemptionGuard, ExitableTaskRef)
    where R: Send + 'static,
{
    let preemption_guard = hold_preemption();

    #[cfg(not(rq_eval))]
    debug!("task_cleanup_success: {:?} successfully exited with return value {:?}", current_task.name, debugit!(exit_value));
    if current_task.mark_as_exited(Box::new(exit_value)).is_err() {
        error!("task_cleanup_success: {:?} task could not set exit value, because task had already exited.", current_task.name);
    }

    (preemption_guard, current_task)
}

fn task_cleanup_success<F, A, R>(current_task: ExitableTaskRef, exit_value: R) -> !
    where A: Send + 'static,
          R: Send + 'static,
          F: FnOnce(A) -> R,
{
    let (preemption_guard, current_task) = task_cleanup_success_internal(current_task, exit_value);
    task_cleanup_final::<F, A, R>(preemption_guard, current_task)
}

fn task_restartable_cleanup_success<F, A, R>(current_task: ExitableTaskRef, exit_value: R) -> !
    where A: Send + Sync + Clone + 'static,
          R: Send + 'static,
          F: FnOnce(A) -> R + Send + Sync + Clone + 'static,
{
    let (preemption_guard, current_task) = task_cleanup_success_internal(current_task, exit_value);
    task_restartable_cleanup_final::<F, A, R>(preemption_guard, current_task)
}


#[inline(always)]
fn task_cleanup_failure_internal(current_task: ExitableTaskRef, kill_reason: task::KillReason) -> (PreemptionGuard, ExitableTaskRef) {
    let preemption_guard = hold_preemption();

    debug!("task_cleanup_failure: {:?} panicked with {:?}", current_task.name, kill_reason);

    if current_task.mark_as_killed(kill_reason).is_err() {
        error!("task_cleanup_failure: {:?} task could not set kill reason, because task had already exited.", current_task.name);
    }

    (preemption_guard, current_task)
}

fn task_cleanup_failure<F, A, R>(current_task: ExitableTaskRef, kill_reason: task::KillReason) -> !
    where A: Send + 'static,
          R: Send + 'static,
          F: FnOnce(A) -> R,
{
    let (preemption_guard, current_task) = task_cleanup_failure_internal(current_task, kill_reason);
    task_cleanup_final::<F, A, R>(preemption_guard, current_task)
}

fn task_restartable_cleanup_failure<F, A, R>(current_task: ExitableTaskRef, kill_reason: task::KillReason) -> !
    where A: Send + Sync + Clone + 'static,
          R: Send + 'static,
          F: FnOnce(A) -> R + Send + Sync + Clone + 'static,
{
    let (preemption_guard, current_task) = task_cleanup_failure_internal(current_task, kill_reason);
    task_restartable_cleanup_final::<F, A, R>(preemption_guard, current_task)
}


#[inline(always)]
fn task_cleanup_final_internal(current_task: &ExitableTaskRef) {
    task::scheduler::remove_task_from_current(current_task);
    for tls_dtor in thread_local_macro::take_current_tls_destructors().into_iter() {
        unsafe {
            (tls_dtor.dtor)(tls_dtor.object_ptr);
        }
    }
    current_task.reap_if_orphaned();
    fence(Ordering::Acquire)
}


#[allow(clippy::extra_unused_type_parameters)]
fn task_cleanup_final<F, A, R>(preemption_guard: PreemptionGuard, current_task: ExitableTaskRef) -> !
    where A: Send + 'static,
          R: Send + 'static,
          F: FnOnce(A) -> R,
{
    task_cleanup_final_internal(&current_task);
    drop(current_task);
    drop(preemption_guard);
    loop {
        scheduler::schedule();
        log::warn!("BUG: task_cleanup_final(): task was rescheduled after being dead!");
        if let Some(curr) = task::get_my_current_task() {
            task::scheduler::remove_task(&curr);
        }
    }
}

fn task_restartable_cleanup_final<F, A, R>(preemption_guard: PreemptionGuard, current_task: ExitableTaskRef) -> !
where
    A: Send + Sync + Clone + 'static,
    R: Send + 'static,
    F: FnOnce(A) -> R + Send + Sync + Clone + 'static,
{
    {
        #[cfg(use_crate_replacement)]
        let mut se = fault_crate_swap::SwapRanges::default();

        #[cfg(use_crate_replacement)] {
            if let Some(crate_to_swap) = fault_crate_swap::get_crate_to_swap() {
                let version = fault_crate_swap::self_swap_handler(&crate_to_swap);
                match version {
                    Ok(v) => { se = v }
                    Err(err) => { debug!(" Crate swapping failed {:?}", err) }
                }
            }
        }

        let restartable_info = current_task.with_restart_info(|restart_info_opt| {
            restart_info_opt.map(|restart_info| {
                #[cfg(use_crate_replacement)] {
                    let func_ptr = &restart_info.func as *const _ as usize;
                    let arg_ptr = &restart_info.argument as *const _ as usize;
                    if fault_crate_swap::constant_offset_fix(&se, func_ptr, func_ptr + 16).is_ok()
                        && fault_crate_swap::constant_offset_fix(&se, arg_ptr, arg_ptr + 8).is_ok() {
                        debug!("Function and argument addresses corrected");
                    }
                }

                let func: &F = restart_info.func.downcast_ref().expect("BUG: failed to downcast restartable task's function");
                let arg : &A = restart_info.argument.downcast_ref().expect("BUG: failed to downcast restartable task's argument");
                (func.clone(), arg.clone())
            })
        });

        if let Some((func, arg)) = restartable_info {
            let mut new_task = new_task_builder(func, arg)
                .name(current_task.name.clone());
            if let Some(cpu) = current_task.pinned_cpu() {
                new_task = new_task.pin_on_cpu(cpu);
            }
            new_task.spawn_restartable(None)
                .expect("Failed to respawn the restartable task");
        } else {
            error!("BUG: Restartable task has no restart information available");
        }
    }

    task_cleanup_final_internal(&current_task);
    drop(current_task);
    drop(preemption_guard);
    loop {
        scheduler::schedule();
        log::warn!("BUG: task_restartable_cleanup_final(): task was rescheduled after being dead!");
        if let Some(curr) = task::get_my_current_task() {
            task::scheduler::remove_task(&curr);
        }
    }
}

/// Helper function to remove a task from its runqueue.
fn remove_current_task_from_runqueue(current_task: &ExitableTaskRef) {
    task::scheduler::remove_task(current_task);
}

/// A basic idle task that loops endlessly.
#[inline(never)]
fn idle_task_entry(_cpu_id: CpuId) {
    info!("Entered idle task loop on core {}: {:?}", cpu::current_cpu(), task::get_my_current_task());
    loop {
        // Si quelqu'un d'autre est runnable, switcher vers lui
        if !task::scheduler::schedule() {
            // Personne d'autre → dormir jusqu'à la prochaine interruption
            // Le timer APIC va nous réveiller et déclencher une préemption
            unsafe { core::arch::asm!("hlt", options(nomem, nostack)); }
        }
    }
}
