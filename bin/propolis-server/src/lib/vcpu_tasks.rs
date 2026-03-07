// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tasks for vCPU backing threads and controls for them.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use propolis::{
    accessors::MemAccessor,
    bhyve_api,
    common::GuestAddr,
    exits::{self, InstEmul, SuspendDetail, VmExitKind},
    vcpu::Vcpu,
    VmEntry,
};
use slog::{debug, error, info};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum VcpuTaskError {
    #[error("Failed to spawn a vCPU backing thread: {0}")]
    BackingThreadSpawnFailed(std::io::Error),
    #[error("CPU bindings did not match vCPUs: {bindings} bindings for {vcpus} vCPUs")]
    CpuBindingMismatch { bindings: usize, vcpus: usize },
}

pub struct VcpuTasks {
    tasks: Vec<(propolis::tasks::TaskCtrl, std::thread::JoinHandle<()>)>,
    generation: Arc<AtomicUsize>,
}

#[cfg_attr(test, mockall::automock)]
pub(crate) trait VcpuTaskController: Send + Sync + 'static {
    fn new_generation(&self);
    fn pause_all(&mut self);
    fn resume_all(&mut self);
    fn exit_all(&mut self);
}

/// Attempt to emulate a string I/O instruction (INS/OUTS with optional REP
/// prefix). Returns `Some(VmEntry::Run)` if the instruction was recognized
/// and handled, or `None` to fall through to the existing unhandled-exit path.
fn emulate_string_io(
    vcpu: &Vcpu,
    acc_mem: &MemAccessor,
    inst: &InstEmul,
    exit_rip: u64,
    log: &slog::Logger,
) -> Option<VmEntry> {
    let bytes = inst.bytes();
    if bytes.is_empty() {
        return None;
    }

    // Parse prefix bytes
    let mut cursor: usize = 0;
    let mut has_rep = false;
    let mut size_override = false;

    // Scan prefixes (REP and operand-size override can appear in any order)
    loop {
        if cursor >= bytes.len() {
            return None;
        }
        match bytes[cursor] {
            0xF3 => {
                has_rep = true;
                cursor += 1;
            }
            0x66 => {
                size_override = true;
                cursor += 1;
            }
            _ => break,
        }
    }

    if cursor >= bytes.len() {
        return None;
    }

    // Decode opcode
    let opcode = bytes[cursor];
    let (is_out, byte_size): (bool, usize) = match opcode {
        0x6C => (false, 1),                                     // INSB
        0x6D => (false, if size_override { 2 } else { 4 }),     // INSD / INSW
        0x6E => (true, 1),                                      // OUTSB
        0x6F => (true, if size_override { 2 } else { 4 }),      // OUTSD / OUTSW
        _ => return None,
    };

    let inst_len = (cursor + 1) as u64;

    use bhyve_api::vm_reg_name;

    // Read registers
    let port = vcpu.get_reg(vm_reg_name::VM_REG_GUEST_RDX).ok()? as u16;
    let rflags = vcpu.get_reg(vm_reg_name::VM_REG_GUEST_RFLAGS).ok()?;
    let direction_flag = (rflags & (1 << 10)) != 0; // DF bit

    let addr_reg = if is_out {
        vm_reg_name::VM_REG_GUEST_RSI
    } else {
        vm_reg_name::VM_REG_GUEST_RDI
    };
    let mut addr = vcpu.get_reg(addr_reg).ok()?;

    let count = if has_rep {
        let c = vcpu.get_reg(vm_reg_name::VM_REG_GUEST_RCX).ok()?;
        if c == 0 {
            // REP with count=0: skip the instruction entirely
            vcpu.set_reg(
                vm_reg_name::VM_REG_GUEST_RIP,
                exit_rip + inst_len,
            )
            .ok()?;
            return Some(VmEntry::Run);
        }
        c
    } else {
        1u64
    };

    let op_name = if is_out { "OUTS" } else { "INS" };
    info!(
        log,
        "emulating string I/O: {}{} port={:#x} count={} size={} addr={:#x}",
        if has_rep { "REP " } else { "" },
        op_name,
        port,
        count,
        byte_size,
        addr;
        "vcpu" => vcpu.id
    );

    // Acquire guest memory
    let mem = acc_mem.access()?;

    let mut remaining = count;
    while remaining > 0 {
        if is_out {
            // OUTS: read from guest memory, write to port
            let val: u32 = match byte_size {
                1 => {
                    let v: propolis::common::GuestData<u8> =
                        mem.read::<u8>(GuestAddr(addr))?;
                    *v as u32
                }
                2 => {
                    let v: propolis::common::GuestData<u16> =
                        mem.read::<u16>(GuestAddr(addr))?;
                    *v as u32
                }
                4 => {
                    let v: propolis::common::GuestData<u32> =
                        mem.read::<u32>(GuestAddr(addr))?;
                    *v
                }
                _ => unreachable!(),
            };
            // Ignore errors — the port may not have a handler
            let _ = vcpu.bus_pio.handle_out(port, byte_size as u8, val);
        } else {
            // INS: read from port, write to guest memory
            let val = vcpu
                .bus_pio
                .handle_in(port, byte_size as u8)
                .unwrap_or(0xFFFFFFFF);
            match byte_size {
                1 => {
                    mem.write::<u8>(GuestAddr(addr), &(val as u8));
                }
                2 => {
                    mem.write::<u16>(GuestAddr(addr), &(val as u16));
                }
                4 => {
                    mem.write::<u32>(GuestAddr(addr), &val);
                }
                _ => unreachable!(),
            }
        }

        // Advance or retreat address based on direction flag
        if direction_flag {
            addr = addr.wrapping_sub(byte_size as u64);
        } else {
            addr = addr.wrapping_add(byte_size as u64);
        }
        remaining -= 1;
    }

    // Write back updated registers
    vcpu.set_reg(addr_reg, addr).ok()?;
    if has_rep {
        vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RCX, 0).ok()?;
    }

    // Advance RIP past the instruction
    vcpu.set_reg(vm_reg_name::VM_REG_GUEST_RIP, exit_rip + inst_len).ok()?;

    Some(VmEntry::Run)
}

impl VcpuTasks {
    pub(crate) fn new(
        machine: &propolis::Machine,
        event_handler: Arc<dyn super::vm::guest_event::VcpuEventHandler>,
        bind_cpus: Option<Vec<pbind::processorid_t>>,
        log: slog::Logger,
    ) -> Result<Self, VcpuTaskError> {
        let generation = Arc::new(AtomicUsize::new(0));

        // We take in an `Option<Vec<..>>` but a `Vec<Option<..>>` is more
        // convenient for spawning below, so we have to shuffle values a bit..
        let mut bindings = vec![None; machine.vcpus.len()];
        if let Some(bind_cpus) = bind_cpus {
            if bind_cpus.len() != machine.vcpus.len() {
                return Err(VcpuTaskError::CpuBindingMismatch {
                    bindings: bind_cpus.len(),
                    vcpus: machine.vcpus.len(),
                });
            }
            for i in 0..machine.vcpus.len() {
                bindings[i] = Some(bind_cpus[i]);
            }
        }

        let mut tasks = Vec::new();
        for (vcpu, bind_cpu) in
            machine.vcpus.iter().map(Arc::clone).zip(bindings.into_iter())
        {
            let (task, ctrl) =
                propolis::tasks::TaskHdl::new_held(Some(vcpu.barrier_fn()));
            let task_log = log.new(slog::o!("vcpu" => vcpu.id));
            let task_event_handler = event_handler.clone();
            let task_gen = generation.clone();
            let task_acc_mem =
                machine.acc_mem.child(Some("vcpu-string-io".to_string()));
            let thread = std::thread::Builder::new()
                .name(format!("vcpu-{}", vcpu.id))
                .spawn(move || {
                    if let Some(bind_cpu) = bind_cpu {
                        pbind::bind_lwp(bind_cpu)
                            .expect("can bind to specified CPU");
                    }
                    Self::vcpu_loop(
                        vcpu.as_ref(),
                        &task_acc_mem,
                        task,
                        task_event_handler,
                        task_gen,
                        task_log,
                    )
                })
                .map_err(VcpuTaskError::BackingThreadSpawnFailed)?;
            tasks.push((ctrl, thread));
        }

        Ok(Self { tasks, generation })
    }

    fn vcpu_loop(
        vcpu: &Vcpu,
        acc_mem: &MemAccessor,
        task: propolis::tasks::TaskHdl,
        event_handler: Arc<dyn super::vm::guest_event::VcpuEventHandler>,
        generation: Arc<AtomicUsize>,
        log: slog::Logger,
    ) {
        info!(log, "Starting vCPU thread");
        let mut entry = VmEntry::Run;
        let mut exit = propolis::exits::VmExit::default();
        let mut local_gen = 0;
        loop {
            use propolis::tasks::Event;

            let mut force_exit_when_consistent = false;
            match task.pending_event() {
                Some(Event::Hold) => {
                    if !exit.kind.is_consistent() {
                        // Before the vCPU task can enter the held state, its
                        // associated in-kernel state must be driven to a point
                        // where it is consistent.
                        force_exit_when_consistent = true;
                    } else {
                        info!(log, "vCPU paused");
                        task.hold();
                        info!(log, "vCPU released from hold");

                        // If the VM was reset while the CPU was paused, clear out
                        // any re-entry reasons from the exit that occurred prior to
                        // the pause.
                        let current_gen = generation.load(Ordering::Acquire);
                        if local_gen != current_gen {
                            entry = VmEntry::Run;
                            local_gen = current_gen;
                        }

                        // This hold might have been satisfied by a request for the
                        // CPU to exit. Check for other pending events before
                        // re-entering the guest.
                        continue;
                    }
                }
                Some(Event::Exit) => break,
                None => {}
            }

            exit = match vcpu.run(&entry, force_exit_when_consistent) {
                Err(e) => {
                    event_handler.io_error_event(vcpu.id, e);
                    entry = VmEntry::Run;
                    continue;
                }
                Ok(exit) => exit,
            };

            entry = vcpu.process_vmexit(&exit).unwrap_or_else(|| {
                match exit.kind {
                    VmExitKind::Inout(pio) => {
                        debug!(&log, "Unhandled pio {:x?}", pio;
                                       "rip" => exit.rip);
                        VmEntry::InoutFulfill(exits::InoutRes::emulate_failed(
                            &pio,
                        ))
                    }
                    VmExitKind::Mmio(mmio) => {
                        debug!(&log, "Unhandled mmio {:x?}", mmio;
                                       "rip" => exit.rip);
                        VmEntry::MmioFulfill(exits::MmioRes::emulate_failed(
                            &mmio,
                        ))
                    }
                    VmExitKind::Rdmsr(msr) => {
                        debug!(&log, "Unhandled rdmsr {:08x}", msr;
                                       "rip" => exit.rip);
                        let _ = vcpu.set_reg(
                            bhyve_api::vm_reg_name::VM_REG_GUEST_RAX,
                            0,
                        );
                        let _ = vcpu.set_reg(
                            bhyve_api::vm_reg_name::VM_REG_GUEST_RDX,
                            0,
                        );
                        VmEntry::Run
                    }
                    VmExitKind::Wrmsr(msr, val) => {
                        debug!(&log, "Unhandled wrmsr {:08x} <- {:08x}", msr, val;
                                       "rip" => exit.rip);
                        VmEntry::Run
                    }
                    VmExitKind::Suspended(SuspendDetail { kind, when }) => {
                        use propolis::vcpu::Diagnostics;
                        match kind {
                            exits::Suspend::Halt => {
                                event_handler.suspend_halt_event(when);
                            }
                            exits::Suspend::Reset => {
                                event_handler.suspend_reset_event(when);
                            }
                            exits::Suspend::TripleFault(vcpuid) => {
                                slog::info!(
                                    &log,
                                    "triple fault on vcpu {}",
                                    vcpu.id;
                                    "state" => %Diagnostics::capture(vcpu)
                                );

                                if vcpuid == -1 || vcpuid == vcpu.id {
                                    event_handler
                                        .suspend_triple_fault_event(vcpu.id, when);
                                }
                            }
                        }

                        // This vCPU will not successfully re-enter the guest
                        // until the state worker does something about the
                        // suspend condition, so hold the task until it does so.
                        // Note that this blocks the task immediately.
                        //
                        // N.B.
                        // This usage assumes that it is safe for the VM
                        // controller to ask the task to hold again (which may
                        // occur if a separate pausing event is serviced in
                        // parallel on the state worker).
                        task.force_hold();
                        VmEntry::Run
                    }
                    VmExitKind::InstEmul(inst) => {
                        match emulate_string_io(
                            vcpu, acc_mem, &inst, exit.rip, &log,
                        ) {
                            Some(entry) => entry,
                            None => {
                                // Not a string I/O instruction — preserve
                                // existing behavior
                                let diag =
                                    propolis::vcpu::Diagnostics::capture(vcpu);
                                error!(log,
                                       "instruction emulation exit on vCPU {}",
                                       vcpu.id;
                                       "context" => ?inst,
                                       "vcpu_state" => %diag);

                                event_handler
                                    .unhandled_vm_exit(vcpu.id, exit.kind);
                                VmEntry::Run
                            }
                        }
                    }
                    VmExitKind::Unknown(code) => {
                        error!(log,
                               "unrecognized exit code on vCPU {}",
                               vcpu.id;
                               "code" => code);

                        event_handler.unhandled_vm_exit(vcpu.id, exit.kind);
                        VmEntry::Run
                    }
                    // Bhyve emits the `Bogus` exit kind when there is no actual
                    // guest exit for user space to handle, but circumstances
                    // nevertheless dictate that the kernel VMM should exit to
                    // user space (e.g. a caller requested that all vCPUs be
                    // forced to exit to user space so their threads can
                    // rendezvous there).
                    //
                    // `process_vmexit` should always successfully handle this
                    // exit, since it never entails any work that could fail to
                    // be completed.
                    VmExitKind::Bogus => {
                        unreachable!(
                            "propolis-lib always handles VmExitKind::Bogus"
                        );
                    }
                    VmExitKind::Debug => {
                        error!(log,
                               "lib returned debug exit from vCPU {}",
                               vcpu.id);

                        event_handler.unhandled_vm_exit(vcpu.id, exit.kind);
                        VmEntry::Run
                    }
                    VmExitKind::VmxError(detail) => {
                        error!(log,
                               "unclassified VMX exit on vCPU {}",
                               vcpu.id;
                               "detail" => ?detail);

                        event_handler.unhandled_vm_exit(vcpu.id, exit.kind);
                        VmEntry::Run
                    }
                    VmExitKind::SvmError(detail) => {
                        error!(log,
                               "unclassified SVM exit on vCPU {}",
                               vcpu.id;
                               "detail" => ?detail);

                        event_handler.unhandled_vm_exit(vcpu.id, exit.kind);
                        VmEntry::Run
                    }
                    VmExitKind::Paging(gpa, fault_type) => {
                        let diag = propolis::vcpu::Diagnostics::capture(vcpu);
                        error!(log,
                               "unhandled paging exit on vCPU {}",
                               vcpu.id;
                               "gpa" => gpa,
                               "fault_type" => fault_type,
                               "vcpu_state" => %diag);

                        event_handler.unhandled_vm_exit(vcpu.id, exit.kind);
                        VmEntry::Run
                    }
                }
            });
        }
        info!(log, "Exiting vCPU thread for CPU {}", vcpu.id);
    }
}

impl VcpuTaskController for VcpuTasks {
    fn pause_all(&mut self) {
        for task in self.tasks.iter_mut().map(|t| &mut t.0) {
            task.hold().unwrap();
        }
    }

    fn new_generation(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    fn resume_all(&mut self) {
        for task in self.tasks.iter_mut().map(|t| &mut t.0) {
            task.run().unwrap();
        }
    }

    fn exit_all(&mut self) {
        for task in self.tasks.iter_mut().map(|t| &mut t.0) {
            task.exit();
        }

        for thread in self.tasks.drain(..) {
            thread.1.join().unwrap();
        }
    }
}
