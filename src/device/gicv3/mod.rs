// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) 2020-2022 Andre Richter <andre.o.richter@gmail.com>

//! GICv2 Driver - ARM Generic Interrupt Controller v2.
//!
//! The following is a collection of excerpts with useful information from
//!   - `Programmer's Guide for ARMv8-A`
//!   - `ARM Generic Interrupt Controller Architecture Specification`
//!
//! # Programmer's Guide - 10.6.1 Configuration
//!
//! The GIC is accessed as a memory-mapped peripheral.
//!
//! All cores can access the common Distributor, but the CPU interface is banked, that is, each core
//! uses the same address to access its own private CPU interface.
//!
//! It is not possible for a core to access the CPU interface of another core.
//!
//! # Architecture Specification - 10.6.2 Initialization
//!
//! Both the Distributor and the CPU interfaces are disabled at reset. The GIC must be initialized
//! after reset before it can deliver interrupts to the core.
//!
//! In the Distributor, software must configure the priority, target, security and enable individual
//! interrupts. The Distributor must subsequently be enabled through its control register
//! (GICD_CTLR). For each CPU interface, software must program the priority mask and preemption
//! settings.
//!
//! Each CPU interface block itself must be enabled through its control register (GICD_CTLR). This
//! prepares the GIC to deliver interrupts to the core.
//!
//! Before interrupts are expected in the core, software prepares the core to take interrupts by
//! setting a valid interrupt vector in the vector table, and clearing interrupt mask bits in
//! PSTATE, and setting the routing controls.
//!
//! The entire interrupt mechanism in the system can be disabled by disabling the Distributor.
//! Interrupt delivery to an individual core can be disabled by disabling its CPU interface.
//! Individual interrupts can also be disabled (or enabled) in the distributor.
//!
//! For an interrupt to reach the core, the individual interrupt, Distributor and CPU interface must
//! all be enabled. The interrupt also needs to be of sufficient priority, that is, higher than the
//! core's priority mask.
//!
//! # Architecture Specification - 1.4.2 Interrupt types
//!
//! - Peripheral interrupt
//!     - Private Peripheral Interrupt (PPI)
//!         - This is a peripheral interrupt that is specific to a single processor.
//!     - Shared Peripheral Interrupt (SPI)
//!         - This is a peripheral interrupt that the Distributor can route to any of a specified
//!           combination of processors.
//!
//! - Software-generated interrupt (SGI)
//!     - This is an interrupt generated by software writing to a GICD_SGIR register in the GIC. The
//!       system uses SGIs for interprocessor communication.
//!     - An SGI has edge-triggered properties. The software triggering of the interrupt is
//!       equivalent to the edge transition of the interrupt request signal.
//!     - When an SGI occurs in a multiprocessor implementation, the CPUID field in the Interrupt
//!       Acknowledge Register, GICC_IAR, or the Aliased Interrupt Acknowledge Register, GICC_AIAR,
//!       identifies the processor that requested the interrupt.
//!
//! # Architecture Specification - 2.2.1 Interrupt IDs
//!
//! Interrupts from sources are identified using ID numbers. Each CPU interface can see up to 1020
//! interrupts. The banking of SPIs and PPIs increases the total number of interrupts supported by
//! the Distributor.
//!
//! The GIC assigns interrupt ID numbers ID0-ID1019 as follows:
//!   - Interrupt numbers 32..1019 are used for SPIs.
//!   - Interrupt numbers 0..31 are used for interrupts that are private to a CPU interface. These
//!     interrupts are banked in the Distributor.
//!       - A banked interrupt is one where the Distributor can have multiple interrupts with the
//!         same ID. A banked interrupt is identified uniquely by its ID number and its associated
//!         CPU interface number. Of the banked interrupt IDs:
//!           - 00..15 SGIs
//!           - 16..31 PPIs
#![allow(dead_code)]
pub mod gicd;
mod gicr;

use crate::arch::sysreg::{read_sysreg, smc_arg1, write_sysreg};
use crate::config::HvSystemConfig;
use crate::device::virtio_trampoline::handle_virtio_result;
use crate::hypercall::{SGI_EVENT_ID, SGI_RESUME_ID, SGI_VIRTIO_RES_ID};
use crate::percpu::check_events;

pub use gicd::{gicv3_gicd_mmio_handler, GICD_IROUTER};
pub use gicr::{gicv3_gicr_mmio_handler, LAST_GICR};

/// Representation of the GIC.
pub struct GICv3 {
    /// The Distributor.
    gicd: gicd::GICD,

    /// The CPU Interface.
    gicr: gicr::GICR,
}
impl GICv3 {
    /// - The user must ensure to provide a correct MMIO start address.
    pub const unsafe fn new(gicd_mmio_start_addr: usize, gicr_mmio_start_addr: usize) -> Self {
        Self {
            gicd: gicd::GICD::new(gicd_mmio_start_addr),
            gicr: gicr::GICR::new(gicr_mmio_start_addr),
        }
    }
    pub fn read_aff(&self) -> u64 {
        self.gicr.read_aff()
    }
}

pub fn gicv3_cpu_init() {
    let sdei_ver = unsafe { smc_arg1!(0xc4000020) }; //sdei_check();
    info!("sdei vecsion: {}", sdei_ver);
    info!("gicv3 init!");

    let _gicd_base: u64 = HvSystemConfig::get().platform_info.arch.gicd_base;
    let _gicr_base: u64 = HvSystemConfig::get().platform_info.arch.gicr_base;

    // Make ICC_EOIR1_EL1 provide priority drop functionality only. ICC_DIR_EL1 provides interrupt deactivation functionality.
    let _ctlr = read_sysreg!(icc_ctlr_el1);
    write_sysreg!(icc_ctlr_el1, 0x2);
    // Set Interrupt Controller Interrupt Priority Mask Register
    let pmr = read_sysreg!(icc_pmr_el1);
    write_sysreg!(icc_pmr_el1, 0xf0);
    // Enable group 1 irq
    let _igrpen = read_sysreg!(icc_igrpen1_el1);
    write_sysreg!(icc_igrpen1_el1, 0x1);

    gicv3_clear_pending_irqs();
    let _vtr = read_sysreg!(ich_vtr_el2);
    let vmcr = ((pmr & 0xff) << 24) | (1 << 1) | (1 << 9); //VPMR|VENG1|VEOIM
    write_sysreg!(ich_vmcr_el2, vmcr);
    write_sysreg!(ich_hcr_el2, 0x1); //enable virt cpu interface
}

fn gicv3_clear_pending_irqs() {
    let vtr = read_sysreg!(ich_vtr_el2) as usize;
    let lr_num: usize = (vtr & 0xf) + 1;
    for i in 0..lr_num {
        write_lr(i, 0) //clear lr
    }
    let num_priority_bits = (vtr >> 29) + 1;
    /* Clear active priority bits */
    if num_priority_bits >= 5 {
        write_sysreg!(ICH_AP1R0_EL2, 0); //Interrupt Controller Hyp Active Priorities Group 1 Register 0 No interrupt active
    }
    if num_priority_bits >= 6 {
        write_sysreg!(ICH_AP1R1_EL2, 0);
    }
    if num_priority_bits > 6 {
        write_sysreg!(ICH_AP1R2_EL2, 0);
        write_sysreg!(ICH_AP1R3_EL2, 0);
    }
}
pub fn gicv3_cpu_shutdown() {
    // unsafe {write_sysreg!(icc_sgi1r_el1, val);}
    // let intid = unsafe { read_sysreg!(icc_iar1_el1) } as u32;
    //arm_read_sysreg(ICC_CTLR_EL1, cell_icc_ctlr);
    info!("gicv3 shutdown!");
    let ctlr = read_sysreg!(icc_ctlr_el1);
    let pmr = read_sysreg!(icc_pmr_el1);
    let ich_hcr = read_sysreg!(ich_hcr_el2);
    debug!("ctlr: {:#x?}, pmr:{:#x?},ich_hcr{:#x?}", ctlr, pmr, ich_hcr);
    //TODO gicv3 reset
}

pub fn gicv3_handle_irq_el1() {
    if let Some(irq_id) = pending_irq() {
        //SGI
        if irq_id < 16 {
            trace!("sgi get {}", irq_id);
            if irq_id < 8 {
                trace!("sgi get {},inject", irq_id);
                deactivate_irq(irq_id);
                inject_irq(irq_id, false);
            } else if irq_id == SGI_EVENT_ID as usize {
                // info!("HV SGI EVENT {}", irq_id);
                check_events();
                deactivate_irq(irq_id);
            } else if irq_id == SGI_RESUME_ID as usize {
                info!("hv sgi got {}, resume", irq_id);
                // let cpu_data = unsafe { this_cpu_data() as &mut PerCpu };
                // cpu_data.suspend_cpu = false;
            } else if irq_id == SGI_VIRTIO_RES_ID as usize {
                handle_virtio_result();
                deactivate_irq(irq_id);
            } else {
                warn!("skip sgi {}", irq_id);
            }
        } else {
            //inject phy irq
            // if irq_id >= 32 {
            //     info!("get irq_id {}", irq_id);
            // }
            inject_irq(irq_id, true);
            deactivate_irq(irq_id);
        }
    }
}
fn pending_irq() -> Option<usize> {
    let iar = read_sysreg!(icc_iar1_el1) as usize;
    if iar >= 0x3fe {
        // spurious
        None
    } else {
        Some(iar as _)
    }
}
fn deactivate_irq(irq_id: usize) {
    write_sysreg!(icc_eoir1_el1, irq_id as u64);
    if irq_id < 16 {
        write_sysreg!(icc_dir_el1, irq_id as u64);
    }
    //write_sysreg!(icc_dir_el1, irq_id as u64);
}
fn read_lr(id: usize) -> u64 {
    match id {
        //TODO get lr size from gic reg
        0 => read_sysreg!(ich_lr0_el2),
        1 => read_sysreg!(ich_lr1_el2),
        2 => read_sysreg!(ich_lr2_el2),
        3 => read_sysreg!(ich_lr3_el2),
        4 => read_sysreg!(ich_lr4_el2),
        5 => read_sysreg!(ich_lr5_el2),
        6 => read_sysreg!(ich_lr6_el2),
        7 => read_sysreg!(ich_lr7_el2),
        8 => read_sysreg!(ich_lr8_el2),
        9 => read_sysreg!(ich_lr9_el2),
        10 => read_sysreg!(ich_lr10_el2),
        11 => read_sysreg!(ich_lr11_el2),
        12 => read_sysreg!(ich_lr12_el2),
        13 => read_sysreg!(ich_lr13_el2),
        14 => read_sysreg!(ich_lr14_el2),
        15 => read_sysreg!(ich_lr15_el2),
        _ => {
            error!("lr over");
            loop {}
        }
    }
}
fn write_lr(id: usize, val: u64) {
    match id {
        0 => write_sysreg!(ich_lr0_el2, val),
        1 => write_sysreg!(ich_lr1_el2, val),
        2 => write_sysreg!(ich_lr2_el2, val),
        3 => write_sysreg!(ich_lr3_el2, val),
        4 => write_sysreg!(ich_lr4_el2, val),
        5 => write_sysreg!(ich_lr5_el2, val),
        6 => write_sysreg!(ich_lr6_el2, val),
        7 => write_sysreg!(ich_lr7_el2, val),
        8 => write_sysreg!(ich_lr8_el2, val),
        9 => write_sysreg!(ich_lr9_el2, val),
        10 => write_sysreg!(ich_lr10_el2, val),
        11 => write_sysreg!(ich_lr11_el2, val),
        12 => write_sysreg!(ich_lr12_el2, val),
        13 => write_sysreg!(ich_lr13_el2, val),
        14 => write_sysreg!(ich_lr14_el2, val),
        15 => write_sysreg!(ich_lr15_el2, val),
        _ => {
            error!("lr over");
            loop {}
        }
    }
}

pub fn inject_irq(irq_id: usize, is_hardware: bool) {
    // mask
    const LR_VIRTIRQ_MASK: usize = (1 << 32) - 1;

    let elsr: u64 = read_sysreg!(ich_elrsr_el2);
    let vtr = read_sysreg!(ich_vtr_el2) as usize;
    let lr_num: usize = (vtr & 0xf) + 1;
    let mut free_ir = -1 as isize;
    for i in 0..lr_num {
        // find a free list register
        if (1 << i) & elsr > 0 {
            if free_ir == -1 {
                free_ir = i as isize;
            }
            continue;
        }
        let lr_val = read_lr(i) as usize;
        // if a virtual interrupt is enabled and equals to the physical interrupt irq_id
        if (lr_val & LR_VIRTIRQ_MASK) == irq_id {
            trace!("virtual irq {} enables again", irq_id);
            return;
        }
    }
    // debug!("To Inject IRQ {}, find lr {}", irq_id, free_ir);

    if free_ir == -1 {
        panic!("full lr");
    } else {
        let mut val = irq_id as u64; //v intid
        val |= 1 << 60; //group 1
        val |= 1 << 62; //state pending

        if !is_sgi(irq_id as _) && is_hardware {
            val |= 1 << 61; //map hardware
            val |= (irq_id as u64) << 32; //pINTID
        }
        write_lr(free_ir as usize, val);
    }
}

pub const GICD_SIZE: u64 = 0x10000;
pub const GICR_SIZE: u64 = 0x20000;
pub const IRQHVI: usize = 32 + 0x20;
pub fn is_sgi(irqn: u32) -> bool {
    irqn < 16
}

pub fn is_ppi(irqn: u32) -> bool {
    irqn > 15 && irqn < 32
}

pub fn is_spi(irqn: u32) -> bool {
    irqn > 31 && irqn < 1020
}